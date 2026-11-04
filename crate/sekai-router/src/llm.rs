//! DeepSeek Chat Completions 客户端（OpenAI 兼容）。
//!
//! 默认关闭 thinking 模式，保证 tool_choice 可以指定具体函数、输出可重复。
//! 传输层通过 HttpClient trait 注入：生产用 StdHttpClient，测试用 mock。
//!
//! 安全：API key 只作为 Authorization 请求头使用；LlmConfig 的手写 Debug 会
//! 脱敏 key；所有错误消息都不包含 key 或 Authorization 内容。
//!
//! 重试语义（计划 §5.1）：
//! - 429 / 500 / 502 / 503 / 504 与可恢复的传输错误最多重试 max_retries 次；
//! - 退避 retry_base_ms * 2^n（测试把 retry_base_ms 设为 0 即无等待）；
//! - 401 / 402 / 403 / 400 等 4xx 与非网络错误不重试。

use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::http::{HttpClient, HttpError, HttpRequest, HttpResponse};
use crate::json::Json;

/// 默认最大输出 token。
pub const DEFAULT_MAX_TOKENS: u32 = 4096;
/// 默认温度。
pub const DEFAULT_TEMPERATURE: f64 = 0.0;
/// 默认 HTTP 超时。
pub const DEFAULT_TIMEOUT_SECS: u64 = 90;
/// 默认重试 base 延迟（毫秒）。
pub const DEFAULT_RETRY_BASE_MS: u64 = 1_000;
/// 默认最大重试次数。
pub const DEFAULT_MAX_RETRIES: u32 = 3;
/// 错误消息最大展示字节数。
const MAX_ERROR_MESSAGE_BYTES: usize = 512;
/// 单次退避的最大毫秒数（防止配置异常导致长时间睡眠）。
const MAX_BACKOFF_MS: u64 = 60_000;

/// 模型返回的 token 用量。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Usage {
    /// prompt token 数。
    pub prompt_tokens: u64,
    /// completion token 数。
    pub completion_tokens: u64,
    /// 总 token 数。
    pub total_tokens: u64,
}

impl Usage {
    /// 累加另一次用量。
    pub fn add(&mut self, other: &Usage) {
        self.prompt_tokens = self.prompt_tokens.saturating_add(other.prompt_tokens);
        self.completion_tokens = self
            .completion_tokens
            .saturating_add(other.completion_tokens);
        self.total_tokens = self.total_tokens.saturating_add(other.total_tokens);
    }
}

/// 对话角色。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// system。
    System,
    /// user。
    User,
    /// assistant。
    Assistant,
    /// tool（工具结果）。
    Tool,
}

impl Role {
    /// OpenAI 协议中的字符串。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::User => "user",
            Self::Assistant => "assistant",
            Self::Tool => "tool",
        }
    }
}

/// 一次模型返回的工具调用。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCall {
    /// 调用 id（回填 tool 消息时使用）。
    pub id: String,
    /// 函数名。
    pub name: String,
    /// 原始 JSON 参数文本（可能是非法 JSON，由上层解析）。
    pub arguments: String,
}

/// 对话消息。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    /// 角色。
    pub role: Role,
    /// 文本内容。
    pub content: Option<String>,
    /// assistant 的工具调用。
    pub tool_calls: Vec<ToolCall>,
    /// tool 消息对应的调用 id。
    pub tool_call_id: Option<String>,
    /// 思考内容（默认关闭 thinking，通常为空）。
    pub reasoning_content: Option<String>,
}

impl Message {
    /// system 消息。
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: Role::System,
            content: Some(content.into()),
            tool_calls: Vec::new(),
            tool_call_id: None,
            reasoning_content: None,
        }
    }

    /// user 消息。
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: Some(content.into()),
            tool_calls: Vec::new(),
            tool_call_id: None,
            reasoning_content: None,
        }
    }

    /// tool 结果消息。
    pub fn tool(tool_call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: Role::Tool,
            content: Some(content.into()),
            tool_calls: Vec::new(),
            tool_call_id: Some(tool_call_id.into()),
            reasoning_content: None,
        }
    }

    /// assistant 消息（可带 content 与 tool_calls）。
    pub fn assistant(
        content: Option<String>,
        tool_calls: Vec<ToolCall>,
        reasoning_content: Option<String>,
    ) -> Self {
        Self {
            role: Role::Assistant,
            content,
            tool_calls,
            tool_call_id: None,
            reasoning_content,
        }
    }
}

/// LLM 工具定义。
#[derive(Debug, Clone, PartialEq)]
pub struct ToolDef {
    /// 函数名（符合 ^[a-zA-Z0-9_-]+$）。
    pub name: String,
    /// 描述。
    pub description: String,
    /// JSON Schema（parameters 字段）。
    pub parameters: Json,
}

/// tool_choice 取值。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolChoice {
    /// 自动。
    Auto,
    /// 强制指定函数。
    Function(String),
    /// 不允许调用工具。
    None,
}

/// 一次 chat 请求。
#[derive(Debug, Clone)]
pub struct ChatRequest {
    /// 模型。
    pub model: String,
    /// 对话消息。
    pub messages: Vec<Message>,
    /// 工具定义。
    pub tools: Vec<ToolDef>,
    /// tool_choice。
    pub tool_choice: ToolChoice,
    /// 温度。
    pub temperature: f64,
    /// 最大输出 token；0 表示使用 LlmConfig.max_tokens。
    pub max_tokens: u32,
    /// 是否关闭 thinking（默认 true）。
    pub disable_thinking: bool,
}

/// 一次 chat 响应。
#[derive(Debug, Clone)]
pub struct ChatResponse {
    /// 模型名（服务端返回）。
    pub model: String,
    /// 文本内容。
    pub content: Option<String>,
    /// 工具调用。
    pub tool_calls: Vec<ToolCall>,
    /// finish_reason。
    pub finish_reason: Option<String>,
    /// token 用量。
    pub usage: Usage,
}

/// LLM 客户端错误。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LlmError {
    /// 传输层错误。
    Transport(String),
    /// HTTP 非 2xx。
    Api {
        /// 状态码。
        status: u16,
        /// 服务端错误消息。
        message: String,
    },
    /// 响应协议错误。
    Protocol(String),
    /// 配置错误。
    Config(String),
}

impl std::fmt::Display for LlmError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Transport(message) => write!(f, "LLM 传输错误：{message}"),
            Self::Api { status, message } => write!(f, "LLM API 错误（HTTP {status}）：{message}"),
            Self::Protocol(message) => write!(f, "LLM 响应非法：{message}"),
            Self::Config(message) => write!(f, "LLM 配置错误：{message}"),
        }
    }
}

impl std::error::Error for LlmError {}

impl From<HttpError> for LlmError {
    fn from(error: HttpError) -> Self {
        Self::Transport(error.to_string())
    }
}

/// 可替换的 chat 客户端（测试用脚本化实现）。
pub trait ChatClient {
    /// 发起一次 chat 请求。
    fn chat(&mut self, request: &ChatRequest) -> Result<ChatResponse, LlmError>;
}

/// LlmClient 配置。
#[derive(Clone)]
pub struct LlmConfig {
    /// base URL（不带 /chat/completions）。
    pub base_url: String,
    /// 模型。
    pub model: String,
    /// API key。
    pub api_key: String,
    /// 最大输出 token。
    pub max_tokens: u32,
    /// 温度。
    pub temperature: f64,
    /// HTTP 超时。
    pub timeout: Duration,
    /// 重试 base 延迟（毫秒），测试可设为 0。
    pub retry_base_ms: u64,
    /// 最大重试次数。
    pub max_retries: u32,
}

impl fmt::Debug for LlmConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LlmConfig")
            .field("base_url", &self.base_url)
            .field("model", &self.model)
            .field("api_key", &"<redacted>")
            .field("max_tokens", &self.max_tokens)
            .field("temperature", &self.temperature)
            .field("timeout", &self.timeout)
            .field("retry_base_ms", &self.retry_base_ms)
            .field("max_retries", &self.max_retries)
            .finish()
    }
}

impl LlmConfig {
    /// 用 base URL、模型与 API key 构造默认配置。
    pub fn new(
        base_url: impl Into<String>,
        model: impl Into<String>,
        api_key: impl Into<String>,
    ) -> Self {
        Self {
            base_url: base_url.into(),
            model: model.into(),
            api_key: api_key.into(),
            max_tokens: DEFAULT_MAX_TOKENS,
            temperature: DEFAULT_TEMPERATURE,
            timeout: Duration::from_secs(DEFAULT_TIMEOUT_SECS),
            retry_base_ms: DEFAULT_RETRY_BASE_MS,
            max_retries: DEFAULT_MAX_RETRIES,
        }
    }

    /// chat completions URL。
    pub fn chat_url(&self) -> String {
        let base = self.base_url.trim_end_matches('/');
        format!("{base}/chat/completions")
    }

    /// models URL。
    pub fn models_url(&self) -> String {
        let base = self.base_url.trim_end_matches('/');
        format!("{base}/models")
    }
}

/// DeepSeek Chat Completions 客户端。
///
/// [`Clone`] 共享同一 HTTP 执行器（内部 `Arc<Mutex>`），供 REPL 会话在
/// 多轮 Agent 之间复用连接/助手进程状态。
#[derive(Clone)]
pub struct LlmClient {
    http: Arc<Mutex<Box<dyn HttpClient>>>,
    config: LlmConfig,
}

impl LlmClient {
    /// 构造客户端。
    pub fn new(http: Box<dyn HttpClient>, config: LlmConfig) -> Self {
        Self {
            http: Arc::new(Mutex::new(http)),
            config,
        }
    }

    /// 当前配置（key 已由 Debug 脱敏）。
    pub fn config(&self) -> &LlmConfig {
        &self.config
    }

    /// 构造 chat completions URL。
    pub fn chat_url(&self) -> String {
        self.config.chat_url()
    }

    /// 构造 models URL。
    pub fn models_url(&self) -> String {
        self.config.models_url()
    }

    /// 拉取模型列表（GET base_url/models）。
    pub fn list_models(&mut self) -> Result<Vec<String>, LlmError> {
        let request = self.authorized_request("GET", self.config.models_url());
        let response = self.execute_with_retry(request)?;
        let text = response.body_text();
        let value = Json::parse(&text).map_err(|error| {
            LlmError::Protocol(format!("GET /models 响应不是合法 JSON：{error}"))
        })?;
        let data = value
            .get("data")
            .and_then(Json::as_array)
            .ok_or_else(|| LlmError::Protocol(String::from("GET /models 响应缺少 data 数组")))?;
        let mut models = Vec::new();
        for item in data {
            let id = item
                .get("id")
                .and_then(Json::as_str)
                .ok_or_else(|| LlmError::Protocol(String::from("GET /models 的 data 项缺少 id")))?;
            if !id.is_empty() {
                models.push(String::from(id));
            }
        }
        Ok(models)
    }

    /// 构造带 Authorization/Accept 与超时的请求。
    fn authorized_request(&self, method: &str, url: String) -> HttpRequest {
        let mut request = HttpRequest::new(method, url).timeout(self.config.timeout);
        request.headers.push((
            String::from("Authorization"),
            format!("Bearer {}", self.config.api_key),
        ));
        request
            .headers
            .push((String::from("Accept"), String::from("application/json")));
        request
    }

    /// 带退避重试地执行请求，把非 2xx 转成 LlmError::Api。
    fn execute_with_retry(&mut self, request: HttpRequest) -> Result<HttpResponse, LlmError> {
        let attempts = self.config.max_retries.saturating_add(1);
        let mut last_error: Option<LlmError> = None;
        for attempt in 0..attempts {
            let http_result = match self.http.lock() {
                Ok(mut http) => http.execute(&request),
                Err(poisoned) => poisoned.into_inner().execute(&request),
            };
            match http_result {
                Ok(response) => {
                    if (200..300).contains(&response.status) {
                        return Ok(response);
                    }
                    let error = LlmError::Api {
                        status: response.status,
                        message: api_error_message(&response),
                    };
                    if is_retryable_status(response.status) && attempt + 1 < attempts {
                        last_error = Some(error);
                        self.backoff(attempt);
                        continue;
                    }
                    return Err(error);
                }
                Err(error) => {
                    let retryable = is_retryable_transport(&error);
                    let llm_error = LlmError::Transport(error.to_string());
                    if retryable && attempt + 1 < attempts {
                        last_error = Some(llm_error);
                        self.backoff(attempt);
                        continue;
                    }
                    return Err(llm_error);
                }
            }
        }
        Err(last_error.unwrap_or_else(|| LlmError::Transport(String::from("请求未被执行"))))
    }

    /// 指数退避；retry_base_ms 为 0 时不等待（测试）。
    fn backoff(&self, attempt: u32) {
        let base = self.config.retry_base_ms;
        if base == 0 {
            return;
        }
        let factor = 1u64.checked_shl(attempt.min(20)).unwrap_or(u64::MAX);
        let delay = base.saturating_mul(factor).min(MAX_BACKOFF_MS);
        if delay > 0 {
            std::thread::sleep(Duration::from_millis(delay));
        }
    }
}

impl ChatClient for LlmClient {
    fn chat(&mut self, request: &ChatRequest) -> Result<ChatResponse, LlmError> {
        if self.config.api_key.trim().is_empty() {
            return Err(LlmError::Config(String::from("缺少 API key")));
        }
        let body = build_chat_body(&self.config, request)?;
        let mut http = self.authorized_request("POST", self.config.chat_url());
        http = http.json_body(&body);
        let response = self.execute_with_retry(http)?;
        let text = response.body_text();
        let value = Json::parse(&text)
            .map_err(|error| LlmError::Protocol(format!("chat 响应不是合法 JSON：{error}")))?;
        parse_chat_response(&value)
    }
}

// ---------------------------------------------------------------------------
// 请求序列化
// ---------------------------------------------------------------------------

/// 构造 chat completions 请求体。
fn build_chat_body(config: &LlmConfig, request: &ChatRequest) -> Result<Json, LlmError> {
    if request.messages.is_empty() {
        return Err(LlmError::Config(String::from("messages 不能为空")));
    }
    let model = if request.model.trim().is_empty() {
        config.model.clone()
    } else {
        request.model.clone()
    };
    if model.trim().is_empty() {
        return Err(LlmError::Config(String::from("模型名不能为空")));
    }
    let max_tokens = if request.max_tokens == 0 {
        config.max_tokens
    } else {
        request.max_tokens
    };
    let temperature = if request.temperature.is_finite() {
        request.temperature
    } else {
        config.temperature
    };
    let mut body = Json::object(Vec::new());
    body.set("model", Json::string(model));
    let messages: Vec<Json> = request.messages.iter().map(message_json).collect();
    body.set("messages", Json::Array(messages));
    if !request.tools.is_empty() {
        let tools: Vec<Json> = request.tools.iter().map(tool_def_json).collect();
        body.set("tools", Json::Array(tools));
        body.set("tool_choice", tool_choice_json(&request.tool_choice));
    }
    body.set("stream", Json::Bool(false));
    body.set("temperature", Json::float(temperature));
    body.set("max_tokens", Json::num(i64::from(max_tokens)));
    if request.disable_thinking {
        body.set(
            "thinking",
            Json::object(vec![("type", Json::string("disabled"))]),
        );
    }
    Ok(body)
}

fn message_json(message: &Message) -> Json {
    let mut value = Json::object(Vec::new());
    value.set("role", Json::string(message.role.as_str()));
    match message.role {
        Role::Assistant => {
            match &message.content {
                Some(text) => {
                    value.set("content", Json::string(text.as_str()));
                }
                None => {
                    value.set("content", Json::Null);
                }
            }
            if let Some(reasoning) = &message.reasoning_content
                && !reasoning.is_empty()
            {
                value.set("reasoning_content", Json::string(reasoning.as_str()));
            }
            if !message.tool_calls.is_empty() {
                let calls: Vec<Json> = message.tool_calls.iter().map(tool_call_json).collect();
                value.set("tool_calls", Json::Array(calls));
            }
        }
        Role::Tool => {
            value.set(
                "content",
                Json::string(message.content.clone().unwrap_or_default()),
            );
            if let Some(id) = &message.tool_call_id {
                value.set("tool_call_id", Json::string(id.as_str()));
            }
        }
        Role::System | Role::User => {
            value.set(
                "content",
                Json::string(message.content.clone().unwrap_or_default()),
            );
        }
    }
    value
}

fn tool_call_json(call: &ToolCall) -> Json {
    Json::object(vec![
        ("id", Json::string(call.id.as_str())),
        ("type", Json::string("function")),
        (
            "function",
            Json::object(vec![
                ("name", Json::string(call.name.as_str())),
                ("arguments", Json::string(call.arguments.as_str())),
            ]),
        ),
    ])
}

fn tool_def_json(tool: &ToolDef) -> Json {
    Json::object(vec![
        ("type", Json::string("function")),
        (
            "function",
            Json::object(vec![
                ("name", Json::string(tool.name.as_str())),
                ("description", Json::string(tool.description.as_str())),
                ("parameters", tool.parameters.clone()),
            ]),
        ),
    ])
}

fn tool_choice_json(choice: &ToolChoice) -> Json {
    match choice {
        ToolChoice::Auto => Json::string("auto"),
        ToolChoice::None => Json::string("none"),
        ToolChoice::Function(name) => Json::object(vec![
            ("type", Json::string("function")),
            (
                "function",
                Json::object(vec![("name", Json::string(name.as_str()))]),
            ),
        ]),
    }
}

// ---------------------------------------------------------------------------
// 响应解析
// ---------------------------------------------------------------------------

/// 解析 chat completions 响应；content 与 tool_calls 都为空时返回 Protocol。
fn parse_chat_response(value: &Json) -> Result<ChatResponse, LlmError> {
    if value.as_object().is_none() {
        return Err(LlmError::Protocol(String::from("响应根不是 JSON 对象")));
    }
    let model = match value.get("model") {
        Some(Json::String(text)) => text.clone(),
        None | Some(Json::Null) => String::new(),
        Some(_) => return Err(LlmError::Protocol(String::from("model 必须是字符串"))),
    };
    let choices = value
        .get("choices")
        .and_then(Json::as_array)
        .ok_or_else(|| LlmError::Protocol(String::from("响应缺少 choices 数组")))?;
    let first = choices
        .first()
        .ok_or_else(|| LlmError::Protocol(String::from("choices 为空")))?;
    let message = first
        .get("message")
        .ok_or_else(|| LlmError::Protocol(String::from("choices[0] 缺少 message")))?;
    if message.as_object().is_none() {
        return Err(LlmError::Protocol(String::from(
            "choices[0].message 不是对象",
        )));
    }
    let content = match message.get("content") {
        None | Some(Json::Null) => None,
        Some(Json::String(text)) => Some(text.clone()),
        Some(_) => {
            return Err(LlmError::Protocol(String::from(
                "message.content 必须是字符串或 null",
            )));
        }
    };
    let tool_calls = match message.get("tool_calls") {
        None | Some(Json::Null) => Vec::new(),
        Some(Json::Array(items)) => {
            let mut calls = Vec::with_capacity(items.len());
            for item in items {
                calls.push(parse_tool_call(item)?);
            }
            calls
        }
        Some(_) => {
            return Err(LlmError::Protocol(String::from(
                "message.tool_calls 必须是数组或 null",
            )));
        }
    };
    let finish_reason = match first.get("finish_reason") {
        None | Some(Json::Null) => None,
        Some(Json::String(text)) => Some(text.clone()),
        Some(_) => {
            return Err(LlmError::Protocol(String::from(
                "finish_reason 必须是字符串或 null",
            )));
        }
    };
    let usage = match value.get("usage") {
        None | Some(Json::Null) => Usage::default(),
        Some(usage_value) if usage_value.as_object().is_some() => Usage {
            prompt_tokens: usage_value
                .get("prompt_tokens")
                .and_then(Json::as_u64)
                .unwrap_or(0),
            completion_tokens: usage_value
                .get("completion_tokens")
                .and_then(Json::as_u64)
                .unwrap_or(0),
            total_tokens: usage_value
                .get("total_tokens")
                .and_then(Json::as_u64)
                .unwrap_or(0),
        },
        Some(_) => return Err(LlmError::Protocol(String::from("usage 必须是对象或 null"))),
    };
    let content_is_empty = content.as_deref().map(str::is_empty).unwrap_or(true);
    if content_is_empty && tool_calls.is_empty() {
        return Err(LlmError::Protocol(String::from(
            "响应既无 content 也无 tool_calls",
        )));
    }
    Ok(ChatResponse {
        model,
        content,
        tool_calls,
        finish_reason,
        usage,
    })
}

fn parse_tool_call(item: &Json) -> Result<ToolCall, LlmError> {
    if item.as_object().is_none() {
        return Err(LlmError::Protocol(String::from("tool_calls 项不是对象")));
    }
    let id = item
        .get("id")
        .and_then(Json::as_str)
        .ok_or_else(|| LlmError::Protocol(String::from("tool_calls 项缺少 id")))?;
    match item.get("type") {
        None | Some(Json::Null) => {}
        Some(Json::String(kind)) if kind == "function" => {}
        Some(Json::String(kind)) => {
            return Err(LlmError::Protocol(format!(
                "tool_calls 项 type 不支持：{kind}"
            )));
        }
        Some(_) => {
            return Err(LlmError::Protocol(String::from(
                "tool_calls 项 type 必须是字符串",
            )));
        }
    }
    let function = item
        .get("function")
        .ok_or_else(|| LlmError::Protocol(String::from("tool_calls 项缺少 function")))?;
    let name = function
        .get("name")
        .and_then(Json::as_str)
        .ok_or_else(|| LlmError::Protocol(String::from("tool_calls 项缺少 function.name")))?;
    let arguments = match function.get("arguments") {
        None | Some(Json::Null) => String::new(),
        Some(Json::String(text)) => text.clone(),
        Some(_) => {
            return Err(LlmError::Protocol(String::from(
                "tool_calls 项 function.arguments 必须是字符串",
            )));
        }
    };
    Ok(ToolCall {
        id: String::from(id),
        name: String::from(name),
        arguments,
    })
}

// ---------------------------------------------------------------------------
// 错误分类与消息
// ---------------------------------------------------------------------------

/// 可重试的 HTTP 状态码。
fn is_retryable_status(status: u16) -> bool {
    matches!(status, 429 | 500 | 502 | 503 | 504)
}

/// 可重试的传输错误：连接、读写、超时与 helper 失败视为网络问题。
fn is_retryable_transport(error: &HttpError) -> bool {
    matches!(
        error,
        HttpError::Connect(_) | HttpError::Io(_) | HttpError::Timeout | HttpError::Helper(_)
    )
}

/// 从错误响应体提取 {"error":{"message":...}} 或 {"error":"..."} 或 {"message":...}。
fn api_error_message(response: &HttpResponse) -> String {
    let text = response.body_text();
    if let Ok(value) = Json::parse(&text) {
        if let Some(message) = value.get_path(&["error", "message"]).and_then(Json::as_str) {
            return truncate_message(message);
        }
        if let Some(message) = value.get("error").and_then(Json::as_str) {
            return truncate_message(message);
        }
        if let Some(message) = value.get("message").and_then(Json::as_str) {
            return truncate_message(message);
        }
    }
    let trimmed = text.trim();
    if trimmed.is_empty() {
        format!("HTTP 状态码 {}", response.status)
    } else {
        truncate_message(trimmed)
    }
}

fn truncate_message(text: &str) -> String {
    if text.len() <= MAX_ERROR_MESSAGE_BYTES {
        return String::from(text);
    }
    let mut end = MAX_ERROR_MESSAGE_BYTES;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> LlmConfig {
        let mut config = LlmConfig::new("http://127.0.0.1:1/v1", "deepseek-flash", "sk-unit-test");
        config.retry_base_ms = 0;
        config
    }

    fn request(tools: Vec<ToolDef>) -> ChatRequest {
        ChatRequest {
            model: String::new(),
            messages: vec![Message::system("sys"), Message::user("hi")],
            tools,
            tool_choice: ToolChoice::Auto,
            temperature: f64::NAN,
            max_tokens: 0,
            disable_thinking: true,
        }
    }

    fn tool() -> ToolDef {
        ToolDef {
            name: String::from("worker_tool"),
            description: String::from("desc"),
            parameters: Json::object(vec![("type", Json::string("object"))]),
        }
    }

    #[test]
    fn debug_redacts_api_key() {
        let text = format!("{:?}", config());
        assert!(!text.contains("sk-unit-test"));
        assert!(text.contains("redacted"));
    }

    #[test]
    fn urls_trim_trailing_slash() {
        let config = LlmConfig::new("https://api.deepseek.com/", "m", "k");
        assert_eq!(
            config.chat_url(),
            "https://api.deepseek.com/chat/completions"
        );
        assert_eq!(config.models_url(), "https://api.deepseek.com/models");
    }

    #[test]
    fn body_has_disabled_thinking_and_auto_choice() {
        let body = build_chat_body(&config(), &request(vec![tool()])).unwrap();
        assert_eq!(
            body.get("model").and_then(Json::as_str),
            Some("deepseek-flash")
        );
        assert_eq!(body.get("stream").and_then(Json::as_bool), Some(false));
        assert_eq!(body.get("temperature").and_then(Json::as_f64), Some(0.0));
        assert_eq!(body.get("max_tokens").and_then(Json::as_u64), Some(4096));
        assert_eq!(
            body.get_path(&["thinking", "type"]).and_then(Json::as_str),
            Some("disabled")
        );
        assert_eq!(body.get("tool_choice").and_then(Json::as_str), Some("auto"));
        let tools = body.get("tools").and_then(Json::as_array).unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(
            tools[0]
                .get_path(&["function", "name"])
                .and_then(Json::as_str),
            Some("worker_tool")
        );
        let roles: Vec<&str> = body
            .get("messages")
            .and_then(Json::as_array)
            .unwrap()
            .iter()
            .map(|message| message.get("role").and_then(Json::as_str).unwrap())
            .collect();
        assert_eq!(roles, vec!["system", "user"]);
    }

    #[test]
    fn body_omits_tools_when_empty() {
        let body = build_chat_body(&config(), &request(Vec::new())).unwrap();
        assert!(body.get("tools").is_none());
        assert!(body.get("tool_choice").is_none());
    }

    #[test]
    fn body_supports_function_choice_and_assistant_tool_calls() {
        let mut request = request(vec![tool()]);
        request.tool_choice = ToolChoice::Function(String::from("worker_tool"));
        request.messages.push(Message::assistant(
            None,
            vec![ToolCall {
                id: String::from("call_1"),
                name: String::from("worker_tool"),
                arguments: String::from("{}"),
            }],
            None,
        ));
        request
            .messages
            .push(Message::tool("call_1", "{\"ok\":true}"));
        let body = build_chat_body(&config(), &request).unwrap();
        assert_eq!(
            body.get_path(&["tool_choice", "function", "name"])
                .and_then(Json::as_str),
            Some("worker_tool")
        );
        let messages = body.get("messages").and_then(Json::as_array).unwrap();
        let assistant = &messages[2];
        assert_eq!(assistant.get("content").map(Json::is_null), Some(true));
        let calls = assistant
            .get("tool_calls")
            .and_then(Json::as_array)
            .unwrap();
        assert!(calls[0].get_path(&["function", "id"]).is_none());
        assert_eq!(calls[0].get("id").and_then(Json::as_str), Some("call_1"));
        let tool_message = &messages[3];
        assert_eq!(
            tool_message.get("tool_call_id").and_then(Json::as_str),
            Some("call_1")
        );
    }

    #[test]
    fn parses_plain_answer() {
        let value = Json::parse(
            r#"{"model":"deepseek-flash","choices":[{"message":{"role":"assistant","content":"hello"},"finish_reason":"stop"}],"usage":{"prompt_tokens":3,"completion_tokens":2,"total_tokens":5}}"#,
        )
        .unwrap();
        let response = parse_chat_response(&value).unwrap();
        assert_eq!(response.content.as_deref(), Some("hello"));
        assert!(response.tool_calls.is_empty());
        assert_eq!(response.finish_reason.as_deref(), Some("stop"));
        assert_eq!(response.usage.total_tokens, 5);
    }

    #[test]
    fn parses_multiple_tool_calls_and_keeps_malformed_arguments() {
        let value = Json::parse(
            r#"{"choices":[{"message":{"content":null,"tool_calls":[{"id":"a","type":"function","function":{"name":"worker_tool","arguments":"{\"worker_id\":\"dev\"}"}},{"id":"b","type":"function","function":{"name":"list_workers","arguments":"not-json"}}]},"finish_reason":"tool_calls"}]}"#,
        )
        .unwrap();
        let response = parse_chat_response(&value).unwrap();
        assert!(response.content.is_none());
        assert_eq!(response.tool_calls.len(), 2);
        assert_eq!(response.tool_calls[0].id, "a");
        assert_eq!(response.tool_calls[1].arguments, "not-json");
        assert_eq!(response.usage, Usage::default());
    }

    #[test]
    fn empty_content_and_no_tool_calls_is_protocol_error() {
        let value = Json::parse(r#"{"choices":[{"message":{"content":null}}]}"#).unwrap();
        assert!(matches!(
            parse_chat_response(&value),
            Err(LlmError::Protocol(_))
        ));
        let value = Json::parse(r#"{"choices":[{"message":{"content":""}}]}"#).unwrap();
        assert!(matches!(
            parse_chat_response(&value),
            Err(LlmError::Protocol(_))
        ));
    }

    #[test]
    fn malformed_response_shapes_are_protocol_errors() {
        for text in [
            r#"[]"#,
            r#"{"choices":[]}"#,
            r#"{"choices":[{"message":{}}]}"#,
            r#"{"choices":[{"message":{"content":7}}]}"#,
            r#"{"choices":[{"message":{"content":"x","tool_calls":[{"id":"a","function":{"name":7}}]}}]}"#,
        ] {
            let value = Json::parse(text).unwrap();
            assert!(
                matches!(parse_chat_response(&value), Err(LlmError::Protocol(_))),
                "should reject {text}"
            );
        }
    }

    #[test]
    fn error_status_classification() {
        for status in [429u16, 500, 502, 503, 504] {
            assert!(is_retryable_status(status));
        }
        for status in [400u16, 401, 402, 403, 404, 422] {
            assert!(!is_retryable_status(status));
        }
        assert!(is_retryable_transport(&HttpError::Timeout));
        assert!(is_retryable_transport(&HttpError::Connect(String::from(
            "x"
        ))));
        assert!(!is_retryable_transport(&HttpError::InvalidUrl(
            String::from("x")
        )));
        assert!(!is_retryable_transport(&HttpError::NoHttpsHelper));
    }

    #[test]
    fn api_error_message_prefers_error_object() {
        let response = HttpResponse {
            status: 401,
            headers: Vec::new(),
            body: br#"{"error":{"message":"invalid api key"}}"#.to_vec(),
        };
        assert_eq!(api_error_message(&response), "invalid api key");
        let response = HttpResponse {
            status: 500,
            headers: Vec::new(),
            body: b"plain failure".to_vec(),
        };
        assert_eq!(api_error_message(&response), "plain failure");
        let response = HttpResponse {
            status: 503,
            headers: Vec::new(),
            body: Vec::new(),
        };
        assert_eq!(api_error_message(&response), "HTTP 状态码 503");
    }
}
