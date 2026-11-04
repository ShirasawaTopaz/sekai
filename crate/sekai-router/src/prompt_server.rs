//! prompt 通道：PC Worker 把自然语言 prompt 发送给 Router，由 Router 调
//! LLM 并返回回答（「Router 侧 LLM 中继」）。
//!
//! 两个入口共用同一核心 [`PromptCore`]：
//! - **TCP ingress**（JSONL 行协议，进程级传输层新增；Worker V1 二进制帧
//!   协议不受影响）：后台线程 accept，每连接逐行读 JSONL、串行处理
//!   （与 serve dispatcher 同风格）；
//! - **serve `prompt` 方法**（协议 v1.1 additive，向后兼容）：serve
//!   dispatcher 经 [`dispatch_prompt`] 复用同一执行路径。
//!
//! ## JSONL 协议（逐行 JSON）
//!
//! - 请求：`{"id":<int>,"method":"prompt","params":{"text":"..","max_steps"?:1..=32}}`
//! - 成功：`{"id":..,"ok":true,"result":{"answer":"..","status":"completed"|
//!   "max_steps","steps":<int>,"usage":{"prompt_tokens":n,"completion_tokens":n,
//!   "total_tokens":n}}}`
//! - 失败：`{"id":..,"ok":false,"error":{"code":"..","message":".."}}`，错误码：
//!   `llm_unavailable`（无 API key/LLM 配置）、`not_linked`（无链路网关）、
//!   `no_workers`（无已发现 Worker）、`invalid_arguments`（text 空/超
//!   8192 字节、max_steps 越界）、`agent_error`（Agent 循环失败）。
//! - `{"id":..,"method":"ping","params":{}}` →
//!   `{"id":..,"ok":true,"result":{"pong":true}}`；未知方法回
//!   `unknown_method`；畸形行沿用 serve.rs 惯例（无法取 id 时通知
//!   `invalid_json`，进程不得崩）。
//!
//! ## 防递归
//!
//! 所有经本通道发起的 Agent 运行都把 [`crate::tools::Policy::deny_llm_relay`]
//! 置 true：worker 的 pc.ask → router → 再派 pc.ask 会死循环，一律在
//! 策略层拒绝（见 tools.rs）。
//!
//! ## 线程模型
//!
//! `PromptCore` 是 `Arc` 共享的快照容器：网关/策略由 Session（REPL）或
//! ServeState 在 set_gateway / 策略变更时同步进来；chat 客户端可惰性构造
//! （serve 无 API key 时回 `llm_unavailable`）。ingress 每连接一个处理
//! 线程；跨连接的 Agent 运行经 `run_lock` 串行化，避免多路 prompt 交错
//! 操作同一网关。

use std::io::{BufRead, BufReader, BufWriter, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};

use crate::agent::{Agent, AgentConfig, AgentStatus};
use crate::gateway::{GatewayHandle, WorkerGateway};
use crate::json::Json;
use crate::llm::{ChatClient, Usage};
use crate::tools::{self, Policy};
use crate::trace::TraceWriter;

/// prompt 文本最大字节数（UTF-8）。
pub const MAX_PROMPT_TEXT_BYTES: usize = 8_192;
/// 请求级 max_steps 上限。
pub const MAX_PROMPT_STEPS: u32 = 32;
/// 错误码：LLM 未配置（无 API key）。
pub const CODE_LLM_UNAVAILABLE: &str = "llm_unavailable";
/// 错误码：无链路网关。
pub const CODE_NOT_LINKED: &str = "not_linked";
/// 错误码：无已发现的 Worker。
pub const CODE_NO_WORKERS: &str = "no_workers";
/// 错误码：Agent 循环失败。
pub const CODE_AGENT_ERROR: &str = "agent_error";
/// 错误码：未知方法（ingress）。
const CODE_UNKNOWN_METHOD: &str = "unknown_method";
/// ingress 单行最大字节数（text 上限 8192 字节，JSON 编码与字段开销留足
/// 余量）；超限视为损坏行，通知 invalid_json 后断开，防止无界内存。
const MAX_INGRESS_LINE_BYTES: usize = 64 * 1024;
/// ingress accept 线程名。
const INGRESS_THREAD_NAME: &str = "sekai-router-prompt-ingress";
/// ingress 单连接处理线程名。
const CONN_THREAD_NAME: &str = "sekai-router-prompt-conn";

// ---------------------------------------------------------------------------
// 共享核心
// ---------------------------------------------------------------------------

/// prompt 通道共享核心：`Arc` 共享的网关/策略快照 + Agent 配置。
///
/// 由 Session（REPL/TUI）或 ServeState 持有并在链路/策略变更时同步；
/// ingress 线程与 serve `prompt` 方法经 [`PromptCore::run_prompt`] 执行。
pub struct PromptCore<C: ChatClient + Clone> {
    /// chat 客户端槽位（惰性构造后常驻；Clone 复用连接/助手状态）。
    chat: Mutex<Option<C>>,
    /// chat 客户端构造器（惰性）；返回 None 表示 LLM 未配置。
    builder: Box<dyn Fn() -> Option<C> + Send + Sync>,
    /// 网关快照（None = 未连接）。
    gateway: Mutex<Option<GatewayHandle>>,
    /// 策略快照（执行时克隆并置 deny_llm_relay=true）。
    policy: Mutex<Policy>,
    /// 跨连接串行化 Agent 运行（ingress 多连接并发时避免交错）。
    run_lock: Mutex<()>,
    /// Agent 配置快照（max_steps 可被请求覆盖）。
    agent_config: AgentConfig,
    /// 脱敏 trace。
    trace: TraceWriter,
}

impl<C: ChatClient + Clone> PromptCore<C> {
    /// 直接给定 chat 客户端（REPL：与会话共用同一客户端克隆）。
    pub fn new(chat: C, policy: Policy, agent_config: AgentConfig, trace: TraceWriter) -> Self {
        Self {
            chat: Mutex::new(Some(chat)),
            // 槽位已预填，构造器不会被触发；返回 None 仅作占位。
            builder: Box::new(|| None::<C>),
            gateway: Mutex::new(None),
            policy: Mutex::new(policy),
            run_lock: Mutex::new(()),
            agent_config,
            trace,
        }
    }

    /// 惰性构造 chat 客户端：首次 prompt 请求才经 `builder` 建客户端；
    /// 构造器返回 None 表示 LLM 未配置（如 serve 无 API key）。
    pub fn lazy(
        builder: Box<dyn Fn() -> Option<C> + Send + Sync>,
        policy: Policy,
        agent_config: AgentConfig,
        trace: TraceWriter,
    ) -> Self {
        Self {
            chat: Mutex::new(None),
            builder,
            gateway: Mutex::new(None),
            policy: Mutex::new(policy),
            run_lock: Mutex::new(()),
            agent_config,
            trace,
        }
    }

    /// 同步网关（Session/ServeState 建链、关链、注入时调用）。
    pub fn set_gateway(&self, gateway: Option<GatewayHandle>) {
        *self.gateway.lock().unwrap_or_else(|p| p.into_inner()) = gateway;
    }

    /// 同步策略（`/policy`、serve policy.set 修改后调用）。
    pub fn set_policy(&self, policy: Policy) {
        *self.policy.lock().unwrap_or_else(|p| p.into_inner()) = policy;
    }

    /// 当前策略快照（测试用）。
    pub fn policy(&self) -> Policy {
        self.policy
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    /// 取 chat 客户端：槽位已有则克隆；否则经构造器惰性构造并缓存。
    fn acquire_chat(&self) -> Result<C, PromptRunError> {
        let mut slot = self.chat.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(chat) = slot.clone() {
            return Ok(chat);
        }
        match (self.builder)() {
            Some(chat) => {
                *slot = Some(chat.clone());
                Ok(chat)
            }
            None => Err(PromptRunError::new(
                CODE_LLM_UNAVAILABLE,
                "未配置 LLM：缺少 API key（设置 DEEPSEEK_API_KEY 后重启）",
            )),
        }
    }

    /// 执行一次 prompt：链路校验 → 构造 Agent（deny_llm_relay=true）→ 循环。
    ///
    /// 判定顺序（与 session.rs `run_goal` 语义一致）：无网关 → `not_linked`；
    /// 无已发现 Worker → `no_workers`；LLM 未配置 → `llm_unavailable`；
    /// Agent 循环失败 → `agent_error`（message 带 AgentError Display）。
    pub fn run_prompt(
        &self,
        text: &str,
        max_steps: Option<u32>,
    ) -> Result<PromptRunResult, PromptRunError> {
        let gateway = self
            .gateway
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone();
        let Some(mut gateway) = gateway else {
            return Err(PromptRunError::new(
                CODE_NOT_LINKED,
                "无链路网关：先建立 Worker 链路（/link 或 serve link 请求）",
            ));
        };
        if gateway.list_workers().is_empty() {
            return Err(PromptRunError::new(
                CODE_NO_WORKERS,
                "无已发现的 Worker：等待 Worker 上线后再试",
            ));
        }
        let chat = self.acquire_chat()?;
        // 防递归：本通道发起的 Agent 拒绝嵌套 pc.ask（见模块注释）。
        let mut policy = self.policy();
        policy.deny_llm_relay = true;
        let mut config = self.agent_config.clone();
        if let Some(steps) = max_steps {
            config.max_steps = steps;
        }
        let _guard = self.run_lock.lock().unwrap_or_else(|p| p.into_inner());
        let mut agent = Agent::new(chat, gateway, policy, config, self.trace.clone());
        match agent.run(text) {
            Ok(outcome) => Ok(PromptRunResult {
                answer: outcome.final_text.unwrap_or_default(),
                status: match outcome.status {
                    AgentStatus::Completed => "completed",
                    AgentStatus::MaxSteps => "max_steps",
                },
                steps: outcome.steps,
                usage: outcome.usage,
            }),
            Err(error) => Err(PromptRunError::new(CODE_AGENT_ERROR, error.to_string())),
        }
    }
}

/// prompt 成功执行结果（result 对象的四字段来源）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptRunResult {
    /// 最终回答（max_steps 时可为空串）。
    pub answer: String,
    /// `"completed"` / `"max_steps"`。
    pub status: &'static str,
    /// Agent 步数（LLM 调用次数）。
    pub steps: usize,
    /// token 用量汇总。
    pub usage: Usage,
}

impl PromptRunResult {
    /// 组装 result 对象 JSON（answer/status/steps/usage）。
    pub fn to_result_json(&self) -> Json {
        Json::object(vec![
            ("answer", Json::string(self.answer.as_str())),
            ("status", Json::string(self.status)),
            ("steps", Json::num(self.steps as i64)),
            (
                "usage",
                Json::object(vec![
                    ("prompt_tokens", usage_num(self.usage.prompt_tokens)),
                    ("completion_tokens", usage_num(self.usage.completion_tokens)),
                    ("total_tokens", usage_num(self.usage.total_tokens)),
                ]),
            ),
        ])
    }
}

/// prompt 执行失败（稳定错误码 + 中文消息）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptRunError {
    /// 稳定错误码（本模块 `CODE_*` 常量或 invalid_arguments）。
    pub code: String,
    /// 人读消息。
    pub message: String,
}

impl PromptRunError {
    /// 构造错误。
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }
}

impl std::fmt::Display for PromptRunError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}：{}", self.code, self.message)
    }
}

impl std::error::Error for PromptRunError {}

fn usage_num(value: u64) -> Json {
    Json::num(i64::try_from(value).unwrap_or(i64::MAX))
}

// ---------------------------------------------------------------------------
// prompt 方法（serve 方法与 ingress 共用）
// ---------------------------------------------------------------------------

/// 解析 `prompt` 方法 params（严格白名单 + 边界校验）。
///
/// 返回 `(text, max_steps)`；失败统一回 `invalid_arguments`。
pub fn parse_prompt_params(params: &Json) -> Result<(String, Option<u32>), PromptRunError> {
    if let Err(error) = tools::check_fields(params, &["text", "max_steps"]) {
        return Err(invalid_arguments(error.to_string()));
    }
    let text = match params.get("text") {
        Some(Json::String(text)) => text.clone(),
        None | Some(Json::Null) => return Err(invalid_arguments("缺少必填参数 text")),
        Some(_) => return Err(invalid_arguments("参数 text 必须是字符串")),
    };
    if text.trim().is_empty() {
        return Err(invalid_arguments("text 不能为空"));
    }
    if text.len() > MAX_PROMPT_TEXT_BYTES {
        return Err(invalid_arguments(format!(
            "text 超过 {MAX_PROMPT_TEXT_BYTES} 字节上限（当前 {} 字节）",
            text.len()
        )));
    }
    let max_steps = match params.get("max_steps") {
        None | Some(Json::Null) => None,
        Some(Json::Number(number)) => {
            let number = *number;
            if !number.is_finite() || number.fract() != 0.0 {
                return Err(invalid_arguments("参数 max_steps 必须是整数"));
            }
            if !(1.0..=f64::from(MAX_PROMPT_STEPS)).contains(&number) {
                return Err(invalid_arguments(format!(
                    "max_steps 必须在 1..={MAX_PROMPT_STEPS}"
                )));
            }
            Some(number as u32)
        }
        Some(_) => return Err(invalid_arguments("参数 max_steps 必须是整数")),
    };
    Ok((text, max_steps))
}

/// 处理 `prompt` 方法：参数解析 + 执行（serve 方法与 ingress 共用）。
///
/// 返回 result 对象 JSON 或（错误码, 消息）；信封由调用方组装。
pub fn dispatch_prompt<C: ChatClient + Clone>(
    core: &PromptCore<C>,
    params: &Json,
) -> Result<Json, PromptRunError> {
    let (text, max_steps) = parse_prompt_params(params)?;
    core.run_prompt(&text, max_steps)
        .map(|result| result.to_result_json())
}

fn invalid_arguments(message: impl Into<String>) -> PromptRunError {
    PromptRunError::new(tools::CODE_INVALID_ARGUMENTS, message)
}

// ---------------------------------------------------------------------------
// ingress TCP 服务器
// ---------------------------------------------------------------------------

/// 启动 prompt ingress TCP 服务器（后台线程；返回实际监听地址，端口 0 展开）。
///
/// - 每连接一个处理线程；连接内逐行读 JSONL、串行处理（与 serve
///   dispatcher 同风格）；跨连接的 Agent 运行经核心内 `run_lock` 串行化。
/// - 监听线程随进程退出（不 join）；监听器 Drop 即停。
/// - 绑定失败（端口占用等）返回 io 错误，由调用方决定报错策略
///   （REPL/serve 选择 stderr 报错后继续运行，见 bin 与 serve.rs）。
pub fn start_ingress<C: ChatClient + Clone + Send + 'static>(
    core: Arc<PromptCore<C>>,
    addr: &str,
) -> std::io::Result<std::net::SocketAddr> {
    let listener = TcpListener::bind(addr)?;
    let local_addr = listener.local_addr()?;
    std::thread::Builder::new()
        .name(String::from(INGRESS_THREAD_NAME))
        .spawn(move || accept_loop(listener, core))
        .map_err(|error| std::io::Error::other(format!("线程启动失败：{error}")))?;
    Ok(local_addr)
}

/// accept 循环：单次 accept 失败只记录并继续，不退出。
fn accept_loop<C: ChatClient + Clone + Send + 'static>(
    listener: TcpListener,
    core: Arc<PromptCore<C>>,
) {
    for stream in listener.incoming() {
        let stream = match stream {
            Ok(stream) => stream,
            Err(error) => {
                eprintln!("sekai-router: prompt 通道 accept 失败：{error}");
                continue;
            }
        };
        let core = Arc::clone(&core);
        let spawn = std::thread::Builder::new()
            .name(String::from(CONN_THREAD_NAME))
            .spawn(move || handle_connection(stream, core));
        if let Err(error) = spawn {
            eprintln!("sekai-router: prompt 通道连接线程启动失败：{error}");
        }
    }
}

/// 单连接处理：逐行读 → 处理 → 写响应行（写后即 flush）。
fn handle_connection<C: ChatClient + Clone>(stream: TcpStream, core: Arc<PromptCore<C>>) {
    let Ok(reader) = stream.try_clone() else {
        return;
    };
    let mut reader = BufReader::new(reader);
    let mut writer = BufWriter::new(stream);
    loop {
        let line = match read_line_capped(&mut reader, MAX_INGRESS_LINE_BYTES) {
            Ok(LineRead::Line(line)) => line,
            Ok(LineRead::Eof) => break,
            Ok(LineRead::TooLong) => {
                write_line(
                    &mut writer,
                    &invalid_json_notification(&format!(
                        "单行超过 {} 字节上限，连接已关闭",
                        MAX_INGRESS_LINE_BYTES
                    )),
                );
                break;
            }
            Err(error) => {
                eprintln!("sekai-router: prompt 通道读取失败：{error}");
                break;
            }
        };
        for message in handle_ingress_line(&core, &line) {
            write_line(&mut writer, &message);
        }
        if writer.flush().is_err() {
            // 对端已断开：读端随后 EOF，连接自然收场。
            break;
        }
    }
}

/// 处理一行 ingress 请求（serve `handle_line` 同风格：畸形行跳行回错，
/// 任何输入只影响该行，不 panic）。
fn handle_ingress_line<C: ChatClient + Clone>(core: &PromptCore<C>, line: &str) -> Vec<Json> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return Vec::new();
    }
    let value = match Json::parse(trimmed) {
        Ok(value) => value,
        Err(error) => {
            // JsonError 的 Display 自带「JSON 解析失败（偏移 N）：」前缀。
            return vec![invalid_json_notification(&error.to_string())];
        }
    };
    if value.as_object().is_none() {
        return vec![invalid_json_notification("请求必须是 JSON 对象")];
    }
    let id = match value.get("id") {
        Some(id) => match id.as_i64() {
            Some(id) => id,
            None => return vec![invalid_json_notification("请求 id 必须是整数")],
        },
        None => return vec![invalid_json_notification("请求缺少 id")],
    };
    let method = match value.get("method") {
        Some(Json::String(method)) => method.clone(),
        None => {
            return vec![error_response(
                id,
                tools::CODE_INVALID_ARGUMENTS,
                "缺少必填参数 method",
            )];
        }
        Some(_) => {
            return vec![error_response(
                id,
                tools::CODE_INVALID_ARGUMENTS,
                "参数 method 必须是字符串",
            )];
        }
    };
    let params = match value.get("params") {
        None => Json::object(Vec::new()),
        Some(value) if value.as_object().is_some() => value.clone(),
        Some(_) => {
            return vec![error_response(
                id,
                tools::CODE_INVALID_ARGUMENTS,
                "参数 params 必须是 JSON 对象",
            )];
        }
    };
    match method.as_str() {
        "ping" => vec![ok_response(
            id,
            Json::object(vec![("pong", Json::Bool(true))]),
        )],
        "prompt" => match dispatch_prompt(core, &params) {
            Ok(result) => vec![ok_response(id, result)],
            Err(error) => vec![error_response(id, &error.code, &error.message)],
        },
        other => vec![error_response(
            id,
            CODE_UNKNOWN_METHOD,
            &format!("未知方法 {other}"),
        )],
    }
}

/// 一行读取结果。
#[derive(Debug, PartialEq, Eq)]
enum LineRead {
    /// 读到一行（不含行尾换行）。
    Line(String),
    /// 流结束。
    Eof,
    /// 超过字节上限（调用方应通知并断开）。
    TooLong,
}

/// 读取一行（带字节上限，防止对端无界发送撑爆内存）；LF/CRLF 均接受。
fn read_line_capped<R: BufRead>(reader: &mut R, max_bytes: usize) -> std::io::Result<LineRead> {
    let mut bytes: Vec<u8> = Vec::new();
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            return Ok(if bytes.is_empty() {
                LineRead::Eof
            } else {
                LineRead::Line(String::from_utf8_lossy(&bytes).into_owned())
            });
        }
        let newline = available.iter().position(|&byte| byte == b'\n');
        let take = match newline {
            Some(index) => index + 1,
            None => available.len(),
        };
        bytes.extend_from_slice(&available[..take]);
        reader.consume(take);
        if newline.is_some() {
            if bytes.last() == Some(&b'\n') {
                bytes.pop();
            }
            if bytes.last() == Some(&b'\r') {
                bytes.pop();
            }
            return Ok(LineRead::Line(String::from_utf8_lossy(&bytes).into_owned()));
        }
        if bytes.len() > max_bytes {
            return Ok(LineRead::TooLong);
        }
    }
}

// ---------------------------------------------------------------------------
// 响应构造（信封形状与 serve.rs 一致）
// ---------------------------------------------------------------------------

/// 成功响应信封。
fn ok_response(id: i64, result: Json) -> Json {
    Json::object(vec![
        ("id", Json::num(id)),
        ("ok", Json::Bool(true)),
        ("result", result),
    ])
}

/// 失败响应信封（稳定错误码）。
fn error_response(id: i64, code: &str, message: &str) -> Json {
    Json::object(vec![
        ("id", Json::num(id)),
        ("ok", Json::Bool(false)),
        (
            "error",
            Json::object(vec![
                ("code", Json::string(code)),
                ("message", Json::string(message)),
            ]),
        ),
    ])
}

/// invalid_json 通知（畸形行取不到 id 时的错误回执形式）。
fn invalid_json_notification(message: &str) -> Json {
    Json::object(vec![
        ("event", Json::string("invalid_json")),
        ("message", Json::string(message)),
    ])
}

/// 写一行 JSONL（写失败仅 stderr 诊断；连接由读端收场）。
fn write_line<W: Write>(writer: &mut W, value: &Json) {
    if let Err(error) = writeln!(writer, "{}", value.to_compact_string()) {
        eprintln!("sekai-router: prompt 通道写响应失败：{error}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::{CommandRequest, CommandResult};
    use crate::gateway::GatewayError;
    use crate::llm::{ChatRequest, ChatResponse, LlmError, ToolCall};
    use crate::worker::WorkerInfo;
    use std::collections::VecDeque;

    // ---- 测试替身 ----

    /// 顺序返回预设响应的 chat 客户端（同时记录请求）。
    #[derive(Clone)]
    struct ScriptedChat {
        responses: Arc<Mutex<VecDeque<Result<ChatResponse, LlmError>>>>,
        requests: Arc<Mutex<Vec<ChatRequest>>>,
    }

    impl ScriptedChat {
        fn responses(list: Vec<Result<ChatResponse, LlmError>>) -> Self {
            Self {
                responses: Arc::new(Mutex::new(list.into())),
                requests: Arc::new(Mutex::new(Vec::new())),
            }
        }

        fn requests(&self) -> Vec<ChatRequest> {
            self.requests.lock().unwrap().clone()
        }
    }

    impl ChatClient for ScriptedChat {
        fn chat(&mut self, request: &ChatRequest) -> Result<ChatResponse, LlmError> {
            self.requests.lock().unwrap().push(request.clone());
            self.responses
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| Err(LlmError::Protocol(String::from("scripted chat 已耗尽"))))
        }
    }

    /// 可注入 Worker 快照、可记录命令的内存网关。
    struct MockGateway {
        workers: Vec<WorkerInfo>,
        calls: Arc<Mutex<Vec<CommandRequest>>>,
    }

    impl crate::gateway::WorkerGateway for MockGateway {
        fn list_workers(&mut self) -> Vec<WorkerInfo> {
            self.workers.clone()
        }

        fn submit(
            &mut self,
            request: CommandRequest,
        ) -> Result<crate::gateway::CommandReceipt, GatewayError> {
            self.calls.lock().unwrap().push(request.clone());
            let result = Ok(CommandResult {
                cmd_id: 7,
                worker_id: request.worker_id,
                kind: request.kind,
                name: request.name,
                payload: b"ok".to_vec(),
                phases: Vec::new(),
                elapsed_ms: 1,
                failure: None,
            });
            let (sender, receiver) = std::sync::mpsc::channel();
            let _ = sender.send(result);
            Ok(crate::gateway::CommandReceipt::new(
                receiver, 60_000, "内存",
            ))
        }

        fn wait_ready(&mut self, _timeout_ms: u64) -> Result<(), GatewayError> {
            Ok(())
        }

        fn is_ready(&self) -> bool {
            true
        }
    }

    fn worker(worker_id: &str) -> WorkerInfo {
        WorkerInfo {
            worker_id: String::from(worker_id),
            online: true,
            boot_id: 3,
            firmware: String::from("0.1.0"),
            uptime_ms: 10,
            unix_ms: 1_700_000_000_000,
            tools: vec![String::from("gpio.info"), String::from("pc.ask")],
            tasks: vec![String::from("echo")],
            workers: Vec::new(),
            active_tasks: Vec::new(),
            last_seen_mono_ms: 1,
            hello_seq: 1,
        }
    }

    fn gateway_handle(
        workers: Vec<WorkerInfo>,
        calls: Arc<Mutex<Vec<CommandRequest>>>,
    ) -> GatewayHandle {
        Arc::new(Mutex::new(
            Box::new(MockGateway { workers, calls }) as Box<dyn crate::gateway::WorkerGateway>
        ))
    }

    fn text_response(text: &str) -> Result<ChatResponse, LlmError> {
        Ok(ChatResponse {
            model: String::from("deepseek-flash"),
            content: Some(String::from(text)),
            tool_calls: Vec::new(),
            finish_reason: Some(String::from("stop")),
            usage: Usage {
                prompt_tokens: 10,
                completion_tokens: 5,
                total_tokens: 15,
            },
        })
    }

    fn tool_call_response(id: &str, name: &str, arguments: &str) -> Result<ChatResponse, LlmError> {
        Ok(ChatResponse {
            model: String::from("deepseek-flash"),
            content: None,
            tool_calls: vec![ToolCall {
                id: String::from(id),
                name: String::from(name),
                arguments: String::from(arguments),
            }],
            finish_reason: Some(String::from("tool_calls")),
            usage: Usage {
                prompt_tokens: 8,
                completion_tokens: 2,
                total_tokens: 10,
            },
        })
    }

    fn agent_config() -> AgentConfig {
        AgentConfig {
            model: String::from("deepseek-flash"),
            max_steps: 4,
            ddl_ms: 1_000,
            timeout_ms: None,
            temperature: 0.0,
            max_tokens: 4_096,
        }
    }

    fn core_with(
        responses: Vec<Result<ChatResponse, LlmError>>,
    ) -> (PromptCore<ScriptedChat>, Arc<Mutex<Vec<CommandRequest>>>) {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let core = PromptCore::new(
            ScriptedChat::responses(responses),
            Policy::default(),
            agent_config(),
            TraceWriter::disabled(),
        );
        core.set_gateway(Some(gateway_handle(
            vec![worker("pc-1")],
            Arc::clone(&calls),
        )));
        (core, calls)
    }

    // ---- run_prompt ----

    #[test]
    fn run_prompt_success_returns_answer_status_steps_usage() {
        let (core, _calls) = core_with(vec![text_response("回答：一切正常")]);
        let result = core.run_prompt("看看状态", None).expect("应成功");
        assert_eq!(result.answer, "回答：一切正常");
        assert_eq!(result.status, "completed");
        assert_eq!(result.steps, 1);
        assert_eq!(result.usage.total_tokens, 15);
        let json = result.to_result_json();
        assert_eq!(
            json.get("answer").and_then(Json::as_str),
            Some("回答：一切正常")
        );
        assert_eq!(json.get("status").and_then(Json::as_str), Some("completed"));
        assert_eq!(json.get("steps").and_then(Json::as_u64), Some(1));
        assert_eq!(
            json.get_path(&["usage", "total_tokens"])
                .and_then(Json::as_u64),
            Some(15)
        );
    }

    #[test]
    fn run_prompt_without_llm_config_is_llm_unavailable() {
        // 惰性构造器返回 None（无 API key）：即使链路与 Worker 就绪也回
        // llm_unavailable。
        let calls = Arc::new(Mutex::new(Vec::new()));
        let core = PromptCore::<ScriptedChat>::lazy(
            Box::new(|| None::<ScriptedChat>),
            Policy::default(),
            agent_config(),
            TraceWriter::disabled(),
        );
        let error = core.run_prompt("hi", None).expect_err("无 LLM 应报错");
        assert_eq!(error.code, CODE_NOT_LINKED, "未建链时先报 not_linked");
        core.set_gateway(Some(gateway_handle(vec![worker("pc-1")], calls)));
        let error = core.run_prompt("hi", None).expect_err("无 LLM 应报错");
        assert_eq!(error.code, CODE_LLM_UNAVAILABLE);
        assert!(error.message.contains("API key"), "{}", error.message);
    }

    #[test]
    fn run_prompt_requires_link_and_workers() {
        let (core, _calls) = core_with(vec![text_response("unused")]);
        let standalone = PromptCore::new(
            ScriptedChat::responses(vec![text_response("unused")]),
            Policy::default(),
            agent_config(),
            TraceWriter::disabled(),
        );
        let error = standalone.run_prompt("hi", None).expect_err("无网关应报错");
        assert_eq!(error.code, CODE_NOT_LINKED);

        // 有网关但 Worker 列表为空：no_workers（run_goal 同语义）。
        let calls = Arc::new(Mutex::new(Vec::new()));
        core.set_gateway(Some(gateway_handle(Vec::new(), calls)));
        let error = core.run_prompt("hi", None).expect_err("无 Worker 应报错");
        assert_eq!(error.code, CODE_NO_WORKERS);
    }

    #[test]
    fn run_prompt_denies_nested_pc_ask() {
        // 模型尝试嵌套 pc.ask：策略层拒绝（命令不触达网关），模型随后作答。
        let (core, calls) = core_with(vec![
            tool_call_response(
                "call_1",
                "worker_tool",
                r#"{"worker_id":"pc-1","tool":"pc.ask","args":"再问一层"}"#,
            ),
            text_response("嵌套 prompt 被拒绝"),
        ]);
        let result = core.run_prompt("帮我再问一层", None).expect("应成功");
        assert_eq!(result.answer, "嵌套 prompt 被拒绝");
        assert!(
            calls.lock().unwrap().is_empty(),
            "被拒绝的 pc.ask 不应触达网关"
        );
        // 第二步请求里应能看到拒绝的 tool 结果文本。
        let chat = core
            .chat
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
            .expect("槽位应已填充");
        let requests = chat.requests();
        assert_eq!(requests.len(), 2, "应有两步 LLM 调用");
        let tool_results: Vec<&crate::llm::Message> = requests[1]
            .messages
            .iter()
            .filter(|message| message.role == crate::llm::Role::Tool)
            .collect();
        assert_eq!(tool_results.len(), 1);
        let content = tool_results[0].content.as_deref().unwrap_or_default();
        assert!(
            content.contains("嵌套 prompt（pc.ask）"),
            "tool 结果应包含拒绝说明：{content}"
        );
        assert!(content.contains("policy_denied"), "{content}");
    }

    #[test]
    fn run_prompt_max_steps_override() {
        // 请求级 max_steps=1：第一步仍要调工具 → 达到上限，status=max_steps。
        let (core, _calls) = core_with(vec![tool_call_response("call_1", "list_workers", "{}")]);
        let result = core
            .run_prompt("列出 Worker 并逐一检查", Some(1))
            .expect("达到上限也是成功返回");
        assert_eq!(result.status, "max_steps");
        assert_eq!(result.steps, 1);
        assert_eq!(result.answer, "", "max_steps 时 answer 为空串");
    }

    #[test]
    fn run_prompt_agent_error_carries_display_message() {
        let (core, _calls) = core_with(vec![Err(LlmError::Transport(String::from("连接不上")))]);
        let error = core.run_prompt("hi", None).expect_err("LLM 失败应报错");
        assert_eq!(error.code, CODE_AGENT_ERROR);
        assert!(error.message.contains("连接不上"), "{}", error.message);
    }

    // ---- 参数解析 ----

    #[test]
    fn parse_prompt_params_validates_text_and_max_steps() {
        let params = Json::parse(r#"{"text":"你好","max_steps":8}"#).unwrap();
        assert_eq!(
            parse_prompt_params(&params).unwrap(),
            (String::from("你好"), Some(8))
        );
        let params = Json::parse(r#"{"text":"你好"}"#).unwrap();
        assert_eq!(
            parse_prompt_params(&params).unwrap(),
            (String::from("你好"), None)
        );
        for (line, needle) in [
            (r#"{}"#, "text"),
            (r#"{"text":""}"#, "不能为空"),
            (r#"{"text":"   "}"#, "不能为空"),
            (r#"{"text":123}"#, "必须是字符串"),
            (r#"{"text":"你好","extra":1}"#, "未知参数"),
            (r#"{"text":"你好","max_steps":0}"#, "max_steps"),
            (r#"{"text":"你好","max_steps":33}"#, "max_steps"),
            (r#"{"text":"你好","max_steps":1.5}"#, "max_steps"),
            (r#"{"text":"你好","max_steps":"3"}"#, "max_steps"),
        ] {
            let params = Json::parse(line).unwrap();
            let error = parse_prompt_params(&params).expect_err(line);
            assert_eq!(error.code, tools::CODE_INVALID_ARGUMENTS, "{line}");
            assert!(error.message.contains(needle), "{line}：{}", error.message);
        }
        // 恰好 8192 字节通过，8193 拒绝。
        let exact = "a".repeat(MAX_PROMPT_TEXT_BYTES);
        let params = Json::object(vec![("text", Json::string(exact.as_str()))]);
        assert!(parse_prompt_params(&params).is_ok());
        let over = "a".repeat(MAX_PROMPT_TEXT_BYTES + 1);
        let params = Json::object(vec![("text", Json::string(over.as_str()))]);
        let error = parse_prompt_params(&params).expect_err("超限应拒绝");
        assert!(error.message.contains("上限"), "{}", error.message);
    }

    // ---- ingress 行处理（无 IO）----

    #[test]
    fn ingress_line_ping_unknown_and_malformed() {
        let (core, _calls) = core_with(vec![text_response("unused")]);
        let messages = handle_ingress_line(&core, r#"{"id":1,"method":"ping","params":{}}"#);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].get("id").and_then(Json::as_i64), Some(1));
        assert_eq!(messages[0].get("ok").and_then(Json::as_bool), Some(true));
        assert_eq!(
            messages[0]
                .get_path(&["result", "pong"])
                .and_then(Json::as_bool),
            Some(true)
        );

        let messages = handle_ingress_line(&core, r#"{"id":2,"method":"bogus","params":{}}"#);
        assert_eq!(
            messages[0]
                .get("error")
                .unwrap()
                .get("code")
                .and_then(Json::as_str),
            Some("unknown_method")
        );

        for line in ["{oops", r#""hello""#, r#"{"method":"ping"}"#] {
            let messages = handle_ingress_line(&core, line);
            assert_eq!(
                messages[0].get("event").and_then(Json::as_str),
                Some("invalid_json"),
                "{line}"
            );
        }
        assert!(handle_ingress_line(&core, "   ").is_empty(), "空行静默跳过");
    }

    #[test]
    fn ingress_line_prompt_error_envelope_has_id_and_code() {
        // 无网关 → not_linked；错误信封带回请求 id。
        let core = PromptCore::new(
            ScriptedChat::responses(vec![text_response("unused")]),
            Policy::default(),
            agent_config(),
            TraceWriter::disabled(),
        );
        let messages = handle_ingress_line(
            &core,
            r#"{"id":9,"method":"prompt","params":{"text":"hi"}}"#,
        );
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].get("id").and_then(Json::as_i64), Some(9));
        assert_eq!(messages[0].get("ok").and_then(Json::as_bool), Some(false));
        assert_eq!(
            messages[0]
                .get_path(&["error", "code"])
                .and_then(Json::as_str),
            Some(CODE_NOT_LINKED)
        );
        // 参数非法：invalid_arguments 优先于链路检查。
        let messages =
            handle_ingress_line(&core, r#"{"id":10,"method":"prompt","params":{"text":""}}"#);
        assert_eq!(
            messages[0]
                .get_path(&["error", "code"])
                .and_then(Json::as_str),
            Some(tools::CODE_INVALID_ARGUMENTS)
        );
    }

    /// 读行上限：超限行返回 TooLong，正常行剥掉 LF/CRLF。
    #[test]
    fn read_line_capped_enforces_limit() {
        let mut reader = std::io::Cursor::new(b"hello\r\nworld\n".to_vec());
        assert_eq!(
            read_line_capped(&mut reader, 64).unwrap(),
            LineRead::Line(String::from("hello"))
        );
        assert_eq!(
            read_line_capped(&mut reader, 64).unwrap(),
            LineRead::Line(String::from("world"))
        );
        assert_eq!(read_line_capped(&mut reader, 64).unwrap(), LineRead::Eof);

        // 4 字节上限 + 无换行的 10 字节 → TooLong。
        let mut reader = std::io::Cursor::new(b"0123456789".to_vec());
        assert_eq!(read_line_capped(&mut reader, 4).unwrap(), LineRead::TooLong);

        // 末行无换行：EOF 前返回剩余内容。
        let mut reader = std::io::Cursor::new(b"tail".to_vec());
        assert_eq!(
            read_line_capped(&mut reader, 64).unwrap(),
            LineRead::Line(String::from("tail"))
        );
    }
}
