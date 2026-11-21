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
use crate::llm::{ChatClient, ChatRequest, LlmError, Message, Role, ToolCall, ToolChoice, Usage};
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

/// 默认保留最近 N 轮工具结果原文（更早的压缩为占位）。
pub const DEFAULT_CONTEXT_RETAIN_STEPS: u32 = 4;
/// 默认上下文估算字节预算（64 KiB）。
pub const DEFAULT_CONTEXT_BUDGET_BYTES: usize = 64 * 1024;
/// 压缩占位保留的原文头部字符数（按 UTF-8 字符边界截取）。
pub const COMPACTED_TOOL_HEAD_CHARS: usize = 160;
/// 压缩占位的识别前缀（幂等判断，与 [`compacted_tool_result`] 的键序一致）。
const COMPACTED_TOOL_PREFIX: &str = "{\"compacted\":true";
/// 每条消息的角色/结构开销估算（字节；仅用于预算启发式）。
const MESSAGE_FRAMING_BYTES: usize = 16;

/// 单轮 agent 运行的上下文治理策略。
///
/// 只影响送给模型的 `messages`：不改策略、不改工具权限，也不写审计。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContextPolicy {
    /// 保留最近 N 轮工具结果原文；更早的替换为带摘要头的紧凑占位。
    ///
    /// `0` = 关闭滚动压缩（行为与旧版一致，只保留预算护栏）。
    pub retain_steps: u32,
    /// 上下文估算字节预算；超过即结束本轮，不再发起下一次 LLM 请求。
    pub budget_bytes: usize,
}

impl Default for ContextPolicy {
    fn default() -> Self {
        Self {
            retain_steps: DEFAULT_CONTEXT_RETAIN_STEPS,
            budget_bytes: DEFAULT_CONTEXT_BUDGET_BYTES,
        }
    }
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
    /// 上下文治理策略（旧工具结果压缩 + 估算字节预算）。
    pub context: ContextPolicy,
}

/// agent 终止状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentStatus {
    /// 模型给出最终回答。
    Completed,
    /// 达到最大步数。
    MaxSteps,
    /// 上下文估算字节超过预算，主动停止。
    ContextBudget,
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
    /// 仅由已鉴权的飞书前端按运行注入，不向其它入口共享。
    feishu_tools: Option<Arc<crate::feishu_api::FeishuTools>>,
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
            feishu_tools: None,
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

    /// 注入本轮飞书工具；凭据与工具能力不进入会话历史。
    pub fn with_feishu_tools(mut self, tools: Arc<crate::feishu_api::FeishuTools>) -> Self {
        self.feishu_tools = Some(tools);
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
        if self.feishu_tools.is_some() {
            system.push_str(&crate::prompt::feishu_tools_block());
        }
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
        let mut tools = tools::tool_defs();
        if let Some(feishu) = &self.feishu_tools {
            tools.extend(feishu.tool_defs());
        }
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
            // 预算护栏：估算字节超限就不再发起下一次请求。取消优先于预算，
            // 步数上限优先于预算（先判上限，避免把「该收尾」误报成预算）。
            let message_bytes = messages_bytes(&messages);
            if message_bytes > self.config.context.budget_bytes {
                self.trace.event(
                    "context_budget_exceeded",
                    vec![
                        ("step", Json::num(i64::from(step_index))),
                        ("message_bytes", Json::from(message_bytes)),
                        ("budget_bytes", Json::from(self.config.context.budget_bytes)),
                    ],
                );
                return Ok(AgentOutcome {
                    status: AgentStatus::ContextBudget,
                    final_text: None,
                    steps,
                    usage,
                    tool_calls: records,
                    had_worker_failure,
                    last_tool_failed,
                });
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
                    ("message_bytes", Json::from(message_bytes)),
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
            // 旧工具结果滚动压缩：只保留最近 N 轮原文，更早的换成紧凑占位。
            // 发生在下一次请求之前，因此请求体不会随步数无限增长。
            let bytes_before = messages_bytes(&messages);
            let pruned =
                prune_tool_results(&mut messages, &records, self.config.context.retain_steps);
            if pruned > 0 {
                self.trace.event(
                    "context_prune",
                    vec![
                        ("step", Json::num(i64::from(step_index))),
                        (
                            "retained_steps",
                            Json::from(self.config.context.retain_steps),
                        ),
                        ("pruned", Json::from(pruned)),
                        ("bytes_before", Json::from(bytes_before)),
                        ("bytes_after", Json::from(messages_bytes(&messages))),
                    ],
                );
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
        if let Some(feishu) = &self.feishu_tools
            && feishu.handles(&call.name)
        {
            let json = feishu.execute(&call.name, &call.arguments, self.policy.allow_write);
            let ok = json.get("ok").and_then(Json::as_bool) == Some(true);
            let error_code = json
                .get_path(&["error", "code"])
                .and_then(Json::as_str)
                .map(String::from);
            return CallOutcome {
                json,
                record: ToolCallRecord {
                    function: call.name.clone(),
                    worker_id: None,
                    name: Some(call.name.clone()),
                    ok,
                    error_code,
                },
                worker_failure: false,
            };
        }
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
        let kind = match tools::authorize_worker_call(&self.policy, parsed, worker.as_ref()) {
            Ok(Some(kind)) => kind,
            Ok(None) => unreachable!("execute_worker_command requires a Worker command"),
            Err(denied) => {
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
        };
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

// ---------------------------------------------------------------------------
// 上下文治理：估算、滚动压缩
// ---------------------------------------------------------------------------

/// 单条消息的估算字节数（含角色/结构开销；不是 token 精确计量）。
fn message_bytes(message: &Message) -> usize {
    let mut total = MESSAGE_FRAMING_BYTES;
    total += message.content.as_deref().map_or(0, str::len);
    total += message.reasoning_content.as_deref().map_or(0, str::len);
    total += message.tool_call_id.as_deref().map_or(0, str::len);
    for call in &message.tool_calls {
        total += call.id.len() + call.name.len() + call.arguments.len();
    }
    total
}

/// 整段 messages 的估算字节数。
fn messages_bytes(messages: &[Message]) -> usize {
    messages.iter().map(message_bytes).sum()
}

/// 把超出保留窗口的工具结果替换为紧凑占位；返回被压缩的消息条数。
///
/// 只改 `tool` 消息的 `content`，保留 assistant 的 `tool_calls` 与 tool 消息
/// 本身，因此工具调用/结果配对始终成立；`reasoning_content` 一律不动。
/// 已压缩的消息按占位前缀识别并跳过，因此可重复调用（幂等）。
fn prune_tool_results(
    messages: &mut [Message],
    records: &[ToolCallRecord],
    retain_steps: u32,
) -> usize {
    if retain_steps == 0 {
        return 0;
    }
    let exchanges = messages
        .iter()
        .filter(|message| message.role == Role::Assistant && !message.tool_calls.is_empty())
        .count();
    let retain = retain_steps as usize;
    if exchanges <= retain {
        return 0;
    }
    let cutoff = exchanges - retain;
    let mut exchange_index = 0usize;
    let mut tool_index = 0usize;
    let mut pruned = 0usize;
    for message in messages.iter_mut() {
        if message.role == Role::Assistant && !message.tool_calls.is_empty() {
            exchange_index += 1;
            continue;
        }
        if message.role != Role::Tool {
            continue;
        }
        let already_compacted = message
            .content
            .as_deref()
            .is_some_and(|content| content.starts_with(COMPACTED_TOOL_PREFIX));
        if exchange_index > 0
            && exchange_index <= cutoff
            && !already_compacted
            && let Some(record) = records.get(tool_index)
        {
            let original = message.content.as_deref().unwrap_or_default();
            let compacted = compacted_tool_result(record, original);
            // 占位有固定开销：只有真的变小才替换，避免把小的 list_workers
            // 结果「压缩」成更大的文本。
            if compacted.len() < original.len() {
                message.content = Some(compacted);
                pruned += 1;
            }
        }
        tool_index += 1;
    }
    pruned
}

/// 构造一条工具结果的压缩占位（紧凑 JSON）。
///
/// 保留成功/失败、函数名、Worker、命令名与错误码，外加原文头部
/// [`COMPACTED_TOOL_HEAD_CHARS`] 个字符，便于模型判断是否需要重查。
fn compacted_tool_result(record: &ToolCallRecord, original: &str) -> String {
    let mut value = Json::object(Vec::new());
    value.set("compacted", Json::Bool(true));
    value.set("ok", Json::Bool(record.ok));
    value.set("function", Json::string(record.function.as_str()));
    value.set(
        "worker_id",
        match &record.worker_id {
            Some(worker_id) => Json::string(worker_id.as_str()),
            None => Json::Null,
        },
    );
    value.set(
        "name",
        match &record.name {
            Some(name) => Json::string(name.as_str()),
            None => Json::Null,
        },
    );
    value.set(
        "error_code",
        match &record.error_code {
            Some(code) => Json::string(code.as_str()),
            None => Json::Null,
        },
    );
    value.set(
        "head",
        Json::string(head_chars(original, COMPACTED_TOOL_HEAD_CHARS)),
    );
    value.set("note", Json::string("旧工具结果已压缩；需要细节请重新调用"));
    value.to_compact_string()
}

/// 按 UTF-8 字符边界取前 `max_chars` 个字符。
fn head_chars(text: &str, max_chars: usize) -> String {
    match text.char_indices().nth(max_chars) {
        Some((end, _)) => String::from(&text[..end]),
        None => String::from(text),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(ok: bool, name: &str) -> ToolCallRecord {
        ToolCallRecord {
            function: String::from(tools::FN_WORKER_TOOL),
            worker_id: Some(String::from("pc-worker")),
            name: Some(String::from(name)),
            ok,
            error_code: if ok {
                None
            } else {
                Some(String::from("timeout"))
            },
        }
    }

    /// 6 轮工具调用：每轮 assistant(tool_calls) + tool 结果。
    fn six_exchanges() -> (Vec<Message>, Vec<ToolCallRecord>) {
        let mut messages = vec![Message::system("sys"), Message::user("goal")];
        let mut records = Vec::new();
        for index in 0..6 {
            let id = format!("call_{index}");
            messages.push(Message::assistant(
                None,
                vec![ToolCall {
                    id: id.clone(),
                    name: String::from(tools::FN_WORKER_TOOL),
                    arguments: String::from("{}"),
                }],
                None,
            ));
            messages.push(Message::tool(
                id,
                format!(
                    "{{\"ok\":true,\"payload\":\"{index}-{}\"}}",
                    "x".repeat(1_000)
                ),
            ));
            records.push(record(true, "pc.ls"));
        }
        (messages, records)
    }

    #[test]
    fn prune_keeps_recent_tool_results_and_pairing() {
        let (mut messages, records) = six_exchanges();
        let pruned = prune_tool_results(&mut messages, &records, 2);
        assert_eq!(pruned, 4, "6 轮保留 2 轮，应压缩前 4 条");

        let tool_messages: Vec<&Message> = messages
            .iter()
            .filter(|message| message.role == Role::Tool)
            .collect();
        assert_eq!(tool_messages.len(), 6, "tool 消息不得删除");
        for (index, message) in tool_messages.iter().enumerate() {
            let content = message.content.as_deref().unwrap_or_default();
            if index < 4 {
                assert!(content.starts_with(COMPACTED_TOOL_PREFIX), "{content}");
            } else {
                assert!(content.starts_with("{\"ok\":true"), "{content}");
            }
        }
        // 每个 assistant tool_calls 都能找到同 id 的 tool 消息（配对完整）。
        for message in messages.iter().filter(|m| !m.tool_calls.is_empty()) {
            let expected = message.tool_calls[0].id.as_str();
            assert!(
                messages.iter().any(|candidate| {
                    candidate.role == Role::Tool
                        && candidate.tool_call_id.as_deref() == Some(expected)
                }),
                "缺少 {expected} 的 tool 结果"
            );
        }
    }

    #[test]
    fn prune_keeps_everything_when_disabled() {
        let (mut messages, records) = six_exchanges();
        let pruned = prune_tool_results(&mut messages, &records, 0);
        assert_eq!(pruned, 0);
        assert!(
            messages
                .iter()
                .filter(|message| message.role == Role::Tool)
                .all(|message| !message
                    .content
                    .as_deref()
                    .unwrap_or_default()
                    .starts_with(COMPACTED_TOOL_PREFIX))
        );
    }

    #[test]
    fn prune_is_idempotent() {
        let (mut messages, records) = six_exchanges();
        assert_eq!(prune_tool_results(&mut messages, &records, 2), 4);
        let bytes = messages_bytes(&messages);
        assert_eq!(prune_tool_results(&mut messages, &records, 2), 0);
        assert_eq!(messages_bytes(&messages), bytes);
    }

    #[test]
    fn prune_within_retain_window_is_noop() {
        let (mut messages, records) = six_exchanges();
        assert_eq!(prune_tool_results(&mut messages, &records, 6), 0);
        assert_eq!(prune_tool_results(&mut messages, &records, 16), 0);
    }

    #[test]
    fn prune_skips_results_smaller_than_the_placeholder() {
        // 两轮「很小」的结果：进入压缩窗口也不该被换成更大的占位。
        let mut messages = vec![Message::system("sys"), Message::user("goal")];
        let mut records = Vec::new();
        for index in 0..2 {
            let id = format!("tiny_{index}");
            messages.push(Message::assistant(
                None,
                vec![ToolCall {
                    id: id.clone(),
                    name: String::from(tools::FN_LIST_WORKERS),
                    arguments: String::from("{}"),
                }],
                None,
            ));
            messages.push(Message::tool(id, "{\"count\":0}"));
            records.push(ToolCallRecord {
                function: String::from(tools::FN_LIST_WORKERS),
                worker_id: None,
                name: None,
                ok: true,
                error_code: None,
            });
        }
        let before = messages_bytes(&messages);
        assert_eq!(prune_tool_results(&mut messages, &records, 1), 0);
        assert_eq!(messages_bytes(&messages), before);
        assert_eq!(messages[3].content.as_deref(), Some("{\"count\":0}"));
    }

    #[test]
    fn messages_bytes_counts_content_and_tool_calls() {
        let messages = vec![
            Message::system("sys"),
            Message::assistant(
                Some(String::from("hi")),
                vec![ToolCall {
                    id: String::from("call_1"),
                    name: String::from("worker_tool"),
                    arguments: String::from("{}"),
                }],
                None,
            ),
        ];
        let total = messages_bytes(&messages);
        assert!(total >= 6 + 2 + 4 + 11 + 2, "{total}");
        assert_eq!(message_bytes(&messages[0]), MESSAGE_FRAMING_BYTES + 3);
    }

    #[test]
    fn compacted_tool_result_has_prefix_and_bounded_head() {
        let original = "结果".repeat(200);
        let text = compacted_tool_result(&record(true, "pc.ls"), &original);
        assert!(text.starts_with(COMPACTED_TOOL_PREFIX));
        assert!(text.contains("\"ok\":true"));
        assert!(text.len() < original.len());
        let parsed = Json::parse(&text).expect("占位必须是合法 JSON");
        let head = parsed.get("head").and_then(Json::as_str).unwrap();
        assert_eq!(head.chars().count(), COMPACTED_TOOL_HEAD_CHARS);
    }

    #[test]
    fn head_chars_respects_utf8_boundary() {
        assert_eq!(head_chars("中文测试", 2), "中文");
        assert_eq!(head_chars("abc", 10), "abc");
        assert_eq!(head_chars("", 3), "");
    }
}
