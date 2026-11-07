//! 多步 tool-calling agent 循环。
//!
//! 循环：system + user 消息 -> chat(auto tool_choice) -> 无 tool_calls 则完成；
//! 否则按顺序执行每个调用，把结构化 JSON 作为 tool 消息回填，继续下一轮。
//! 工具错误（未知工具/参数非法/策略拒绝/网关错误/Worker Failed）都是 tool 结果，
//! 不中断循环，让模型有机会解释或换路径。
//!
//! trace 只记录摘要：llm_request / llm_response / usage / tool_call / tool_result；
//! 不记录 API key、Authorization 或 MQTT 凭据。

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::Duration;

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
/// 工具等待的取消轮询间隔（毫秒）：50ms 内响应取消 / Ctrl-C。
pub const CANCEL_POLL_MS: u64 = 50;

/// 协作式取消令牌（线程安全；REPL /cancel、TUI Ctrl-C、serve cancel 共用）。
#[derive(Debug, Clone, Default)]
pub struct CancelToken {
    cancelled: Arc<AtomicBool>,
}

impl CancelToken {
    /// 新建未取消令牌。
    pub fn new() -> Self {
        Self {
            cancelled: Arc::new(AtomicBool::new(false)),
        }
    }

    /// 请求取消（幂等）。
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
    }

    /// 是否已请求取消。
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }

    /// 清除取消状态（同一 agent 复用前的复位；默认新建即未取消）。
    pub fn reset(&self) {
        self.cancelled.store(false, Ordering::SeqCst);
    }
}

/// 一轮已完成的会话问答（带外前端的多轮上下文）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConversationTurn {
    /// 用户输入。
    pub user: String,
    /// 机器人回复（实际发给用户的文本）。
    pub assistant: String,
}

/// 带外前端的会话上下文（飞书等）：会话元数据 + 最近若干轮问答。
///
/// 由前端（如 `feishu.rs`）在每次运行时注入；REPL、批处理、prompt 通道的
/// 运行不设置该结构，行为与无上下文时完全一致。上下文只作为对话内容送给
/// 模型，不改变任何策略或工具权限。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConversationContext {
    /// 渠道名（如 `feishu`）。
    pub channel: String,
    /// 会话类型（如 `p2p` / `group`）。
    pub chat_type: String,
    /// 会话 id（飞书为 `chat_id`）。
    pub chat_id: String,
    /// 话题/线程 id（可选）。
    pub thread_id: Option<String>,
    /// 触发本次运行的消息 id。
    pub message_id: String,
    /// 回复场景的根消息 id（可选）。
    pub root_id: Option<String>,
    /// 回复场景的父消息 id（可选）。
    pub parent_id: Option<String>,
    /// 发送者 id。
    pub sender_id: String,
    /// 消息时间（原样字符串，如飞书毫秒时间戳；可选）。
    pub message_time: Option<String>,
    /// 最近若干轮问答（按时间升序）。
    pub turns: Vec<ConversationTurn>,
}

/// agent 运行配置。
#[derive(Debug, Clone)]
pub struct AgentConfig {
    /// 模型名。
    pub model: String,
    /// 最大步数；`None` = 不设上限。
    ///
    /// 直连 PC Worker（tcp/stdio）默认 `None`（只受取消令牌约束）；非 PC
    /// Worker（MQTT/嵌入式）默认 `Some(DEFAULT_MAX_STEPS)`。显式
    /// `--max-steps`/`SEKAI_MAX_STEPS` 或请求级 `max_steps` 覆盖后，
    /// 两类链路都按给定上限收敛。
    pub max_steps: Option<u32>,
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
    cancel: CancelToken,
    /// 带外前端注入的会话上下文（`None` = 纯文本目标，行为同旧版）。
    context: Option<ConversationContext>,
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
            cancel: CancelToken::new(),
            context: None,
        }
    }

    /// 挂接外部取消令牌（REPL /cancel、TUI Ctrl-C 用）。
    pub fn with_cancel(mut self, cancel: CancelToken) -> Self {
        self.cancel = cancel;
        self
    }

    /// 挂接带外前端的会话上下文（飞书多轮记忆 + 会话元数据）。
    pub fn with_conversation_context(mut self, context: ConversationContext) -> Self {
        self.context = Some(context);
        self
    }

    /// 执行目标，直到模型给出最终回答或达到最大步数。
    pub fn run(&mut self, goal: &str) -> Result<AgentOutcome, AgentError> {
        let workers = self.gateway.list_workers();
        let context_turns = self
            .context
            .as_ref()
            .map(|context| context.turns.len())
            .unwrap_or(0);
        self.trace.event(
            "agent_start",
            vec![
                ("worker_count", Json::from(workers.len())),
                ("goal_len", Json::from(goal.len())),
                ("policy", Json::string(self.policy.summary())),
                ("context_turns", Json::from(context_turns)),
            ],
        );
        let mut system = crate::prompt::system_prompt(&workers, &self.policy);
        let mut messages: Vec<Message> = Vec::new();
        if let Some(context) = &self.context {
            system.push_str(&crate::prompt::conversation_context_block(context));
        }
        messages.push(Message::system(system));
        if let Some(context) = &self.context {
            for turn in &context.turns {
                messages.push(Message::user(turn.user.clone()));
                messages.push(Message::assistant(
                    Some(turn.assistant.clone()),
                    Vec::new(),
                    None,
                ));
            }
        }
        messages.push(Message::user(goal));
        let tools = tools::tool_defs();
        let mut usage = Usage::default();
        let mut records: Vec<ToolCallRecord> = Vec::new();
        let mut had_worker_failure = false;
        let mut last_tool_failed = false;
        let mut steps: usize = 0;

        let mut step_index: u32 = 0;
        loop {
            if self.cancel.is_cancelled() {
                return Err(cancelled_error());
            }
            // 上限只在显式给出时生效：直连 PC Worker 默认不设上限，
            // 循环仅由模型完成/取消/LLM 错误终止。
            if let Some(max_steps) = self.config.max_steps
                && step_index >= max_steps
            {
                break;
            }
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

            // 思考模式下回填 reasoning_content：带 tools 的请求在后续轮次
            // 必须完整回传历史 reasoning_content（DeepSeek 思考模式要求），
            // 否则 API 返回 400；thinking 关闭时服务端不返回该字段，为 None。
            messages.push(Message::assistant(
                response.content.clone(),
                response.tool_calls.clone(),
                response.reasoning_content.clone(),
            ));
            for call in &response.tool_calls {
                if self.cancel.is_cancelled() {
                    return Err(cancelled_error());
                }
                let outcome = self.execute_call(call);
                last_tool_failed = !outcome.record.ok;
                if outcome.worker_failure {
                    had_worker_failure = true;
                }
                let result_text = outcome.json.to_compact_string();
                records.push(outcome.record);
                messages.push(Message::tool(call.id.clone(), result_text));
                if self.cancel.is_cancelled() {
                    // 取消后不再进入下一轮 LLM，避免模型继续派工。
                    return Err(cancelled_error());
                }
            }
            step_index += 1;
        }

        self.trace.event(
            "max_steps",
            vec![
                ("steps", Json::from(steps)),
                (
                    "max_steps",
                    match self.config.max_steps {
                        Some(max_steps) => Json::num(i64::from(max_steps)),
                        None => Json::Null,
                    },
                ),
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
        let requested_kind = parsed.command_kind().unwrap_or(CommandKind::Tool);
        // 先取 Worker 能力快照：策略分类（D5）与线类型路由都依赖 tasks 列表。
        let worker = self
            .gateway
            .list_workers()
            .into_iter()
            .find(|worker| worker.worker_id == worker_id);
        if let Err(denied) = tools::check_policy_for_worker(&self.policy, parsed, worker.as_ref()) {
            return failure_outcome(
                &call.name,
                &worker_id,
                Some(requested_kind),
                &name,
                tools::CODE_POLICY_DENIED,
                &denied.to_string(),
                false,
                false,
            );
        }
        // D5：按「名字 → 线类型」自动路由（Worker tasks 列表 / pc.ask/pc.job）。
        let kind = tools::route_command_kind(worker.as_ref(), &name, requested_kind);
        let request = CommandRequest {
            worker_id: worker_id.clone(),
            kind,
            name: name.clone(),
            args: parsed.args().as_bytes().to_vec(),
            ddl_ms: parsed.ddl_ms().unwrap_or(self.config.ddl_ms),
            timeout_ms: self.config.timeout_ms,
        };
        match self.submit_and_wait(request) {
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

    /// 提交命令并在 50ms 粒度上等待最终事件；取消令牌置位时先下发
    /// `sekai.cancel` 再返回 `cancelled`，不等 Worker 的最终事件。
    ///
    /// 等待本身在辅助线程上阻塞（`CommandReceipt::wait`），主循环用
    /// `recv_timeout` 轮询取消位——因此取消在 50ms 内生效，且不需要
    /// 网关层新增非阻塞等待 API。
    fn submit_and_wait(
        &mut self,
        request: CommandRequest,
    ) -> Result<crate::control::CommandResult, GatewayError> {
        let receipt = self.gateway.submit(request)?;
        let cmd_id = receipt.cmd_id();
        let (sender, receiver) = mpsc::channel();
        std::thread::Builder::new()
            .name(String::from("sekai-router-agent-wait"))
            .spawn(move || {
                let _ = sender.send(receipt.wait());
            })
            .map_err(|error| {
                GatewayError::new("protocol_error", format!("无法启动命令等待线程：{error}"))
            })?;
        loop {
            match receiver.recv_timeout(Duration::from_millis(CANCEL_POLL_MS)) {
                Ok(result) => return result,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if self.cancel.is_cancelled() {
                        // 取消：先派发 Worker 侧 sekai.cancel（8 字节大端 cmd_id），
                        // 再本地放弃等待；Worker 的最终事件晚到由 ControlPlane 记账。
                        let _ = self.gateway.cancel(cmd_id);
                        return Err(GatewayError::new(
                            "cancelled",
                            format!("用户取消命令 {cmd_id}（已下发 sekai.cancel）"),
                        ));
                    }
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(GatewayError::new("link_lost", "命令等待线程已退出"));
                }
            }
        }
    }
}

/// 取消时的 agent 终止错误（沿用 Gateway 变体，避免扩大公开枚举）。
fn cancelled_error() -> AgentError {
    AgentError::Gateway(String::from("cancelled: 用户已取消当前目标"))
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
