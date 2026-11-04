//! 多步 tool-calling agent 循环。
//!
//! 循环：system + user 消息 -> chat(auto tool_choice) -> 无 tool_calls 则完成；
//! 否则按顺序执行每个调用，把结构化 JSON 作为 tool 消息回填，继续下一轮。
//! 工具错误（未知工具/参数非法/策略拒绝/网关错误/Worker Failed）都是 tool 结果，
//! 不中断循环，让模型有机会解释或换路径。
//!
//! trace 只记录摘要：llm_request / llm_response / usage / tool_call / tool_result；
//! 不记录 API key、Authorization 或 MQTT 凭据。

use crate::control::{CommandKind, CommandRequest};
use crate::gateway::{GatewayError, WorkerGateway};
use crate::json::Json;
use crate::llm::{ChatClient, ChatRequest, LlmError, Message, ToolCall, ToolChoice, Usage};
use crate::tools::{self, ParsedCall, Policy};
use crate::trace::TraceWriter;

/// trace 中单个字段的最大展示字节数。
const TRACE_FIELD_MAX_BYTES: usize = 4_096;
/// trace 中工具参数的最大展示字节数。
const TRACE_ARGS_MAX_BYTES: usize = 2_048;

/// agent 运行配置。
#[derive(Debug, Clone)]
pub struct AgentConfig {
    /// 模型名。
    pub model: String,
    /// 最大步数。
    pub max_steps: u32,
    /// 默认 DDL。
    pub ddl_ms: u32,
    /// 命令超时覆盖。
    pub timeout_ms: Option<u64>,
    /// 温度。
    pub temperature: f64,
    /// 最大输出 token。
    pub max_tokens: u32,
}

/// agent 终止状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentStatus {
    /// 模型给出最终回答。
    Completed,
    /// 达到最大步数。
    MaxSteps,
}

/// 一次工具调用记录（trace/CLI 摘要用）。
#[derive(Debug, Clone, PartialEq)]
pub struct ToolCallRecord {
    /// 函数名。
    pub function: String,
    /// 目标Worker。
    pub worker_id: Option<String>,
    /// 工具/任务名。
    pub name: Option<String>,
    /// 是否成功。
    pub ok: bool,
    /// 错误码（失败时）。
    pub error_code: Option<String>,
}

/// agent 运行结果。
#[derive(Debug, Clone)]
pub struct AgentOutcome {
    /// 终止状态。
    pub status: AgentStatus,
    /// 最终自然语言回答。
    pub final_text: Option<String>,
    /// 使用的步数（LLM 调用次数）。
    pub steps: usize,
    /// token 用量汇总。
    pub usage: Usage,
    /// 工具调用记录。
    pub tool_calls: Vec<ToolCallRecord>,
    /// 是否发生过Worker命令失败。
    pub had_worker_failure: bool,
    /// 最后一次工具调用是否失败。
    pub last_tool_failed: bool,
}

/// agent 错误（无法继续循环）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentError {
    /// LLM 错误。
    Llm(String),
    /// 网关错误。
    Gateway(String),
    /// 模型既无 content 也无 tool_calls。
    EmptyResponse,
}

impl std::fmt::Display for AgentError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Llm(message) => write!(f, "LLM 错误：{message}"),
            Self::Gateway(message) => write!(f, "网关错误：{message}"),
            Self::EmptyResponse => write!(f, "模型既无 content 也无 tool_calls"),
        }
    }
}

impl std::error::Error for AgentError {}

impl From<LlmError> for AgentError {
    fn from(error: LlmError) -> Self {
        Self::Llm(error.to_string())
    }
}

impl From<GatewayError> for AgentError {
    fn from(error: GatewayError) -> Self {
        Self::Gateway(error.to_string())
    }
}

/// 单次工具调用的执行结果。
struct CallOutcome {
    json: Json,
    record: ToolCallRecord,
    worker_failure: bool,
}

/// agent。
pub struct Agent<C: ChatClient, G: WorkerGateway> {
    chat: C,
    gateway: G,
    policy: Policy,
    config: AgentConfig,
    trace: TraceWriter,
}

impl<C: ChatClient, G: WorkerGateway> Agent<C, G> {
    /// 构造 agent。
    pub fn new(
        chat: C,
        gateway: G,
        policy: Policy,
        config: AgentConfig,
        trace: TraceWriter,
    ) -> Self {
        Self {
            chat,
            gateway,
            policy,
            config,
            trace,
        }
    }

    /// 执行目标，直到模型给出最终回答或达到最大步数。
    pub fn run(&mut self, goal: &str) -> Result<AgentOutcome, AgentError> {
        let workers = self.gateway.list_workers();
        self.trace.event(
            "agent_start",
            vec![
                ("worker_count", Json::from(workers.len())),
                ("goal_len", Json::from(goal.len())),
                ("policy", Json::string(self.policy.summary())),
            ],
        );
        let system = crate::prompt::system_prompt(&workers, &self.policy);
        let mut messages: Vec<Message> = vec![Message::system(system), Message::user(goal)];
        let tools = tools::tool_defs();
        let mut usage = Usage::default();
        let mut records: Vec<ToolCallRecord> = Vec::new();
        let mut had_worker_failure = false;
        let mut last_tool_failed = false;
        let mut steps: usize = 0;

        for step_index in 0..self.config.max_steps {
            steps = step_index as usize + 1;
            let request = ChatRequest {
                model: self.config.model.clone(),
                messages: messages.clone(),
                tools: tools.clone(),
                tool_choice: ToolChoice::Auto,
                temperature: self.config.temperature,
                max_tokens: self.config.max_tokens,
                disable_thinking: true,
            };
            self.trace.event(
                "llm_request",
                vec![
                    ("step", Json::num(i64::from(step_index))),
                    ("model", Json::string(request.model.as_str())),
                    ("message_count", Json::from(messages.len())),
                    ("tool_count", Json::from(tools.len())),
                ],
            );
            let response = match self.chat.chat(&request) {
                Ok(response) => response,
                Err(error) => {
                    self.trace.event(
                        "llm_error",
                        vec![
                            ("step", Json::num(i64::from(step_index))),
                            ("error", Json::string(error.to_string())),
                        ],
                    );
                    return Err(AgentError::Llm(error.to_string()));
                }
            };
            usage.add(&response.usage);
            self.trace.event(
                "usage",
                vec![
                    ("step", Json::num(i64::from(step_index))),
                    ("prompt_tokens", Json::from(response.usage.prompt_tokens)),
                    (
                        "completion_tokens",
                        Json::from(response.usage.completion_tokens),
                    ),
                    ("total_tokens", Json::from(response.usage.total_tokens)),
                    ("cumulative_total_tokens", Json::from(usage.total_tokens)),
                ],
            );
            self.trace.event(
                "llm_response",
                vec![
                    ("step", Json::num(i64::from(step_index))),
                    ("model", Json::string(response.model.as_str())),
                    (
                        "content_len",
                        Json::from(response.content.as_deref().map(str::len).unwrap_or(0)),
                    ),
                    ("tool_call_count", Json::from(response.tool_calls.len())),
                    (
                        "finish_reason",
                        match &response.finish_reason {
                            Some(reason) => Json::string(reason.as_str()),
                            None => Json::Null,
                        },
                    ),
                ],
            );

            if response.tool_calls.is_empty() {
                let Some(text) = response.content else {
                    self.trace.event(
                        "empty_response",
                        vec![("step", Json::num(i64::from(step_index)))],
                    );
                    return Err(AgentError::EmptyResponse);
                };
                if text.trim().is_empty() {
                    self.trace.event(
                        "empty_response",
                        vec![("step", Json::num(i64::from(step_index)))],
                    );
                    return Err(AgentError::EmptyResponse);
                }
                self.trace.event(
                    "completed",
                    vec![
                        ("step", Json::num(i64::from(step_index))),
                        ("steps", Json::from(steps)),
                        ("text_len", Json::from(text.len())),
                    ],
                );
                return Ok(AgentOutcome {
                    status: AgentStatus::Completed,
                    final_text: Some(text),
                    steps,
                    usage,
                    tool_calls: records,
                    had_worker_failure,
                    last_tool_failed,
                });
            }

            // thinking 默认 disabled，服务端不会返回 reasoning_content；
            // ChatResponse 也不暴露该字段（跨模块契约），因此回填 None。
            messages.push(Message::assistant(
                response.content.clone(),
                response.tool_calls.clone(),
                None,
            ));
            for call in &response.tool_calls {
                let outcome = self.execute_call(call);
                last_tool_failed = !outcome.record.ok;
                if outcome.worker_failure {
                    had_worker_failure = true;
                }
                let result_text = outcome.json.to_compact_string();
                records.push(outcome.record);
                messages.push(Message::tool(call.id.clone(), result_text));
            }
        }

        self.trace.event(
            "max_steps",
            vec![
                ("steps", Json::from(steps)),
                ("max_steps", Json::num(i64::from(self.config.max_steps))),
            ],
        );
        Ok(AgentOutcome {
            status: AgentStatus::MaxSteps,
            final_text: None,
            steps,
            usage,
            tool_calls: records,
            had_worker_failure,
            last_tool_failed,
        })
    }

    /// 执行一个 tool_call：记录 trace、在内部执行、记录结果；所有错误都转 JSON。
    fn execute_call(&mut self, call: &ToolCall) -> CallOutcome {
        self.trace.event(
            "tool_call",
            vec![
                ("id", Json::string(call.id.as_str())),
                ("function", Json::string(call.name.as_str())),
                (
                    "arguments",
                    Json::string(truncate_for_trace(&call.arguments, TRACE_ARGS_MAX_BYTES)),
                ),
            ],
        );
        let outcome = self.execute_call_inner(call);
        let mut fields: Vec<(&str, Json)> = vec![
            ("id", Json::string(call.id.as_str())),
            ("function", Json::string(call.name.as_str())),
            ("ok", Json::Bool(outcome.record.ok)),
            (
                "result",
                Json::string(truncate_for_trace(
                    &outcome.json.to_compact_string(),
                    TRACE_FIELD_MAX_BYTES,
                )),
            ),
        ];
        if let Some(code) = &outcome.record.error_code {
            fields.push(("error_code", Json::string(code.as_str())));
        }
        self.trace.event("tool_result", fields);
        outcome
    }

    fn execute_call_inner(&mut self, call: &ToolCall) -> CallOutcome {
        if !is_known_function(&call.name) {
            return failure_outcome(
                &call.name,
                "",
                None,
                "",
                tools::CODE_UNKNOWN_TOOL,
                &format!(
                    "未知工具函数 {}；只能使用 list_workers / worker_tool / worker_task",
                    call.name
                ),
                false,
                false,
            );
        }
        let parsed = match tools::parse_call(&call.name, &call.arguments) {
            Ok(parsed) => parsed,
            Err(error) => {
                let (worker_id, kind, name) = peek_identity(&call.name, &call.arguments);
                return failure_outcome(
                    &call.name,
                    &worker_id,
                    kind,
                    &name,
                    tools::CODE_INVALID_ARGUMENTS,
                    &error.message,
                    false,
                    false,
                );
            }
        };
        match parsed {
            ParsedCall::ListWorkers => {
                let workers = self.gateway.list_workers();
                CallOutcome {
                    json: tools::workers_json(&workers),
                    record: ToolCallRecord {
                        function: String::from(tools::FN_LIST_WORKERS),
                        worker_id: None,
                        name: None,
                        ok: true,
                        error_code: None,
                    },
                    worker_failure: false,
                }
            }
            ParsedCall::WorkerTool { .. } | ParsedCall::WorkerTask { .. } => {
                self.execute_worker_command(call, &parsed)
            }
        }
    }

    fn execute_worker_command(&mut self, call: &ToolCall, parsed: &ParsedCall) -> CallOutcome {
        let worker_id = String::from(parsed.worker_id().unwrap_or_default());
        let name = String::from(parsed.command_name().unwrap_or_default());
        let kind = parsed.command_kind().unwrap_or(CommandKind::Tool);
        if let Err(denied) = tools::check_policy(&self.policy, parsed) {
            return failure_outcome(
                &call.name,
                &worker_id,
                Some(kind),
                &name,
                tools::CODE_POLICY_DENIED,
                &denied.to_string(),
                false,
                false,
            );
        }
        let request = CommandRequest {
            worker_id: worker_id.clone(),
            kind,
            name: name.clone(),
            args: parsed.args().as_bytes().to_vec(),
            ddl_ms: parsed.ddl_ms().unwrap_or(self.config.ddl_ms),
            timeout_ms: self.config.timeout_ms,
        };
        match self.gateway.call(request) {
            Ok(result) => {
                let ok = result.ok();
                let error_code = result.failure.as_ref().map(|failure| failure.code.clone());
                CallOutcome {
                    json: tools::command_result_json(&result),
                    record: ToolCallRecord {
                        function: String::from(parsed.function_name()),
                        worker_id: Some(worker_id),
                        name: Some(name),
                        ok,
                        error_code,
                    },
                    worker_failure: !ok,
                }
            }
            Err(error) => failure_outcome(
                &call.name,
                &worker_id,
                Some(kind),
                &name,
                &error.code,
                &error.message,
                retryable_gateway_code(&error.code),
                true,
            ),
        }
    }
}

/// 服务端错误码是否提示可以重试（agent 回路与 serve `tool_call`/`task`
/// 的失败结果共用）。
pub fn retryable_gateway_code(code: &str) -> bool {
    matches!(
        code,
        "timeout" | "link_lost" | "worker_offline" | "worker_rebooted" | "worker_busy"
    )
}

fn is_known_function(name: &str) -> bool {
    matches!(
        name,
        tools::FN_LIST_WORKERS | tools::FN_WORKER_TOOL | tools::FN_WORKER_TASK
    )
}

/// 参数非法时尽量从原始 JSON 里提取 worker_id / tool|task，便于记录。
fn peek_identity(function: &str, arguments: &str) -> (String, Option<CommandKind>, String) {
    let kind = if function == tools::FN_WORKER_TASK {
        Some(CommandKind::Task)
    } else if function == tools::FN_WORKER_TOOL {
        Some(CommandKind::Tool)
    } else {
        None
    };
    let name_key = if function == tools::FN_WORKER_TASK {
        "task"
    } else {
        "tool"
    };
    let mut worker_id = String::new();
    let mut name = String::new();
    if let Ok(value) = Json::parse(arguments) {
        if let Some(text) = value.get("worker_id").and_then(Json::as_str) {
            worker_id = String::from(text);
        }
        if let Some(text) = value.get(name_key).and_then(Json::as_str) {
            name = String::from(text);
        }
    }
    (worker_id, kind, name)
}

// 8 个参数各自携带独立语义（函数名/Worker/类型/命令名/错误码/消息/可重试/Worker侧失败），
// 聚合进临时结构反而降低调用点可读性，这里显式豁免参数数量阈值。
#[allow(clippy::too_many_arguments)]
fn failure_outcome(
    function: &str,
    worker_id: &str,
    kind: Option<CommandKind>,
    name: &str,
    code: &str,
    message: &str,
    retryable: bool,
    worker_failure: bool,
) -> CallOutcome {
    let worker_id_field = if worker_id.is_empty() {
        None
    } else {
        Some(String::from(worker_id))
    };
    let name_field = if name.is_empty() {
        None
    } else {
        Some(String::from(name))
    };
    CallOutcome {
        json: tools::failure_result_json(worker_id, kind, name, code, message, retryable),
        record: ToolCallRecord {
            function: String::from(function),
            worker_id: worker_id_field,
            name: name_field,
            ok: false,
            error_code: Some(String::from(code)),
        },
        worker_failure,
    }
}

fn truncate_for_trace(text: &str, max: usize) -> String {
    if text.len() <= max {
        return String::from(text);
    }
    let mut end = max;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…(truncated)", &text[..end])
}
