//! LLM 工具定义、参数解析与服务端策略。
//!
//! 三个固定函数名（list_workers / worker_tool / worker_task）映射到Worker能力；
//! Worker的 gpio.write 等带点名称只能作为参数值，不能直接做 OpenAI 函数名。
//!
//! 策略在**发布 Stdin 之前**强制执行：模型即使忽略 system prompt，也无法
//! 绕过 --allow-write / --allow-tasks / --allow-admin / --worker 过滤。

use sekai::sekai_wire::{MAX_DDL_MS, truncate_utf8, validate_name, validate_worker_id};

use crate::control::{CommandKind, CommandResult};
use crate::json::Json;
use crate::llm::ToolDef;
use crate::worker::WorkerInfo;

/// 函数名：列出Worker。
pub const FN_LIST_WORKERS: &str = "list_workers";
/// 函数名：调用Worker工具。
pub const FN_WORKER_TOOL: &str = "worker_tool";
/// 函数名：下发Worker任务。
pub const FN_WORKER_TASK: &str = "worker_task";

/// 只读工具白名单：默认允许。
pub const READ_ONLY_TOOLS: &[&str] = &[
    "gpio.info",
    "gpio.read",
    "sekai.info",
    "sekai.time",
    "sekai.workers",
];
/// 写工具白名单：需要 --allow-write。
pub const WRITE_TOOLS: &[&str] = &["gpio.config", "gpio.write"];

/// 工具结果 payload 的展示上限（约 3.5 KiB）；超过时截断并标 truncated。
pub const PAYLOAD_TRUNCATE_BYTES: usize = 3_584;

/// 服务端本地错误码：参数非法。
pub const CODE_INVALID_ARGUMENTS: &str = "invalid_arguments";
/// 服务端本地错误码：未知函数。
pub const CODE_UNKNOWN_TOOL: &str = "unknown_tool";
/// 服务端本地错误码：策略拒绝。
pub const CODE_POLICY_DENIED: &str = "policy_denied";

/// 三个 LLM 工具定义。
pub fn tool_defs() -> Vec<ToolDef> {
    vec![
        ToolDef {
            name: String::from(FN_LIST_WORKERS),
            description: String::from(
                "List connected Sekai workers with online state, boot_id, firmware, advertised tools/tasks and active tasks. Call this first; worker ids, tool names and task names must come from this result.",
            ),
            parameters: object_schema(Vec::new(), &[]),
        },
        ToolDef {
            name: String::from(FN_WORKER_TOOL),
            description: String::from(
                "Invoke a ToolCall on one connected Sekai worker. args is raw UTF-8 tool argument text, e.g. 'gpio.info help'. Returns the worker result or a structured error.",
            ),
            parameters: object_schema(
                vec![
                    (
                        "worker_id",
                        Json::object(vec![
                            ("type", Json::string("string")),
                            (
                                "description",
                                Json::string("Exact worker id from list_workers."),
                            ),
                        ]),
                    ),
                    (
                        "tool",
                        Json::object(vec![
                            ("type", Json::string("string")),
                            (
                                "description",
                                Json::string(
                                    "Tool name advertised by the worker, e.g. gpio.write.",
                                ),
                            ),
                        ]),
                    ),
                    (
                        "args",
                        Json::object(vec![
                            ("type", Json::string("string")),
                            (
                                "description",
                                Json::string("Raw UTF-8 argument text; empty string for no args."),
                            ),
                        ]),
                    ),
                    (
                        "ddl_ms",
                        Json::object(vec![
                            ("type", Json::string("integer")),
                            ("minimum", Json::num(1)),
                            ("maximum", Json::num(i64::from(MAX_DDL_MS))),
                            (
                                "description",
                                Json::string("Optional execution budget; omit for default."),
                            ),
                        ]),
                    ),
                ],
                &["worker_id", "tool", "args"],
            ),
        },
        ToolDef {
            name: String::from(FN_WORKER_TASK),
            description: String::from(
                "Start a Task on one connected Sekai worker. args is raw UTF-8 task argument text; empty string for no args. Only allowed when tasks are explicitly enabled. Returns the worker result or a structured error.",
            ),
            parameters: object_schema(
                vec![
                    (
                        "worker_id",
                        Json::object(vec![
                            ("type", Json::string("string")),
                            (
                                "description",
                                Json::string("Exact worker id from list_workers."),
                            ),
                        ]),
                    ),
                    (
                        "task",
                        Json::object(vec![
                            ("type", Json::string("string")),
                            (
                                "description",
                                Json::string("Task name advertised by the worker, e.g. echo."),
                            ),
                        ]),
                    ),
                    (
                        "args",
                        Json::object(vec![
                            ("type", Json::string("string")),
                            (
                                "description",
                                Json::string(
                                    "Raw UTF-8 task argument text; empty string for no args.",
                                ),
                            ),
                        ]),
                    ),
                    (
                        "ddl_ms",
                        Json::object(vec![
                            ("type", Json::string("integer")),
                            ("minimum", Json::num(1)),
                            ("maximum", Json::num(i64::from(MAX_DDL_MS))),
                            (
                                "description",
                                Json::string("Optional execution budget; omit for default."),
                            ),
                        ]),
                    ),
                ],
                &["worker_id", "task", "args"],
            ),
        },
    ]
}

fn object_schema(properties: Vec<(&str, Json)>, required: &[&str]) -> Json {
    let mut value = Json::object(Vec::new());
    value.set("type", Json::string("object"));
    let mut property_object = Json::object(Vec::new());
    for (name, schema) in properties {
        property_object.set(name, schema);
    }
    value.set("properties", property_object);
    if !required.is_empty() {
        let items: Vec<Json> = required.iter().map(|name| Json::string(*name)).collect();
        value.set("required", Json::Array(items));
    }
    value.set("additionalProperties", Json::Bool(false));
    value
}

/// 服务端策略。
#[derive(Debug, Clone, Default)]
pub struct Policy {
    /// 允许 GPIO 写。
    pub allow_write: bool,
    /// 允许 Task。
    pub allow_tasks: bool,
    /// 允许管理工具与其他用户工具。
    pub allow_admin: bool,
    /// Worker过滤器：只允许操作该Worker。
    pub worker_filter: Option<String>,
}

impl Policy {
    /// 策略摘要（prompt 与 CLI 输出共用）。
    pub fn summary(&self) -> String {
        let write = if self.allow_write { "允许" } else { "拒绝" };
        let tasks = if self.allow_tasks { "允许" } else { "拒绝" };
        let admin = if self.allow_admin { "允许" } else { "拒绝" };
        let filter = match &self.worker_filter {
            Some(worker_id) => format!("只允许操作Worker {worker_id}"),
            None => String::from("不限制Worker（可操作所有已发现Worker）"),
        };
        format!("GPIO 写：{write}；Task：{tasks}；管理/其他用户工具：{admin}；Worker过滤：{filter}")
    }
}

/// 策略拒绝。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PolicyDenied {
    /// Worker不在过滤器内。
    WorkerFiltered(String),
    /// 能力未授权。
    Forbidden {
        /// 被拒绝的能力描述。
        what: String,
    },
}

impl std::fmt::Display for PolicyDenied {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::WorkerFiltered(worker_id) => {
                write!(f, "Worker {worker_id} 不在 --worker 过滤范围内")
            }
            Self::Forbidden { what } => write!(f, "{what} 未授权（需要显式 CLI 开关）"),
        }
    }
}

impl std::error::Error for PolicyDenied {}

/// 解析后的工具调用。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParsedCall {
    /// list_workers。
    ListWorkers,
    /// worker_tool。
    WorkerTool {
        /// Worker id。
        worker_id: String,
        /// 工具名。
        name: String,
        /// UTF-8 参数文本。
        args: String,
        /// 可选 DDL。
        ddl_ms: Option<u32>,
    },
    /// worker_task。
    WorkerTask {
        /// Worker id。
        worker_id: String,
        /// 任务名。
        name: String,
        /// UTF-8 参数文本。
        args: String,
        /// 可选 DDL。
        ddl_ms: Option<u32>,
    },
}

impl ParsedCall {
    /// 调用的函数名。
    pub fn function_name(&self) -> &'static str {
        match self {
            Self::ListWorkers => FN_LIST_WORKERS,
            Self::WorkerTool { .. } => FN_WORKER_TOOL,
            Self::WorkerTask { .. } => FN_WORKER_TASK,
        }
    }

    /// 命令类型；list_workers 返回 None。
    pub fn command_kind(&self) -> Option<CommandKind> {
        match self {
            Self::ListWorkers => None,
            Self::WorkerTool { .. } => Some(CommandKind::Tool),
            Self::WorkerTask { .. } => Some(CommandKind::Task),
        }
    }

    /// 目标Worker id；list_workers 返回 None。
    pub fn worker_id(&self) -> Option<&str> {
        match self {
            Self::ListWorkers => None,
            Self::WorkerTool { worker_id, .. } | Self::WorkerTask { worker_id, .. } => {
                Some(worker_id)
            }
        }
    }

    /// 工具/任务名；list_workers 返回 None。
    pub fn command_name(&self) -> Option<&str> {
        match self {
            Self::ListWorkers => None,
            Self::WorkerTool { name, .. } | Self::WorkerTask { name, .. } => Some(name),
        }
    }

    /// args 文本；list_workers 返回空串。
    pub fn args(&self) -> &str {
        match self {
            Self::ListWorkers => "",
            Self::WorkerTool { args, .. } | Self::WorkerTask { args, .. } => args,
        }
    }

    /// DDL；未给出时为 None。
    pub fn ddl_ms(&self) -> Option<u32> {
        match self {
            Self::ListWorkers => None,
            Self::WorkerTool { ddl_ms, .. } | Self::WorkerTask { ddl_ms, .. } => *ddl_ms,
        }
    }
}

/// 工具参数解析错误。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolArgError {
    /// 可读消息。
    pub message: String,
}

impl ToolArgError {
    /// 构造错误。
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl std::fmt::Display for ToolArgError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ToolArgError {}

/// 解析模型工具调用参数（严格 JSON + 未知字段拒绝）。
pub fn parse_call(name: &str, arguments: &str) -> Result<ParsedCall, ToolArgError> {
    match name {
        FN_LIST_WORKERS => {
            let value = parse_arguments_object(arguments)?;
            check_fields(&value, &[])?;
            Ok(ParsedCall::ListWorkers)
        }
        FN_WORKER_TOOL => parse_worker_call(arguments, true),
        FN_WORKER_TASK => parse_worker_call(arguments, false),
        other => Err(ToolArgError::new(format!("未知工具函数 {other}"))),
    }
}

fn parse_worker_call(arguments: &str, is_tool: bool) -> Result<ParsedCall, ToolArgError> {
    let value = parse_arguments_object(arguments)?;
    let allowed: &[&str] = if is_tool {
        &["worker_id", "tool", "args", "ddl_ms"]
    } else {
        &["worker_id", "task", "args", "ddl_ms"]
    };
    check_fields(&value, allowed)?;
    let worker_id = required_string(&value, "worker_id")?;
    validate_worker_id(&worker_id)
        .map_err(|message| ToolArgError::new(format!("worker_id 非法：{message}")))?;
    let name_key = if is_tool { "tool" } else { "task" };
    let command_name = required_string(&value, name_key)?;
    validate_name(&command_name)
        .map_err(|message| ToolArgError::new(format!("{name_key} 非法：{message}")))?;
    let args = required_string(&value, "args")?;
    let ddl_ms = parse_ddl(&value)?;
    if is_tool {
        Ok(ParsedCall::WorkerTool {
            worker_id,
            name: command_name,
            args,
            ddl_ms,
        })
    } else {
        Ok(ParsedCall::WorkerTask {
            worker_id,
            name: command_name,
            args,
            ddl_ms,
        })
    }
}

/// 解析参数 JSON 对象；空字符串按空对象处理（无参数函数的常见写法）。
fn parse_arguments_object(arguments: &str) -> Result<Json, ToolArgError> {
    let trimmed = arguments.trim();
    if trimmed.is_empty() {
        return Ok(Json::object(Vec::new()));
    }
    let value = Json::parse(trimmed)
        .map_err(|error| ToolArgError::new(format!("参数不是合法 JSON：{error}")))?;
    if value.as_object().is_none() {
        return Err(ToolArgError::new("参数必须是 JSON 对象"));
    }
    Ok(value)
}

/// 校验对象字段白名单：未知字段、重复字段拒绝（serve link/policy.set
/// 的参数校验复用）。
pub fn check_fields(value: &Json, allowed: &[&str]) -> Result<(), ToolArgError> {
    let Some(entries) = value.as_object() else {
        return Err(ToolArgError::new("参数必须是 JSON 对象"));
    };
    for (key, _) in entries {
        if !allowed.contains(&key.as_str()) {
            return Err(ToolArgError::new(format!("未知参数 {key}")));
        }
    }
    for (index, (key, _)) in entries.iter().enumerate() {
        if entries[..index].iter().any(|(other, _)| other == key) {
            return Err(ToolArgError::new(format!("参数重复 {key}")));
        }
    }
    Ok(())
}

fn required_string(value: &Json, key: &str) -> Result<String, ToolArgError> {
    match value.get(key) {
        None | Some(Json::Null) => Err(ToolArgError::new(format!("缺少必填参数 {key}"))),
        Some(Json::String(text)) => Ok(text.clone()),
        Some(_) => Err(ToolArgError::new(format!("参数 {key} 必须是字符串"))),
    }
}

fn parse_ddl(value: &Json) -> Result<Option<u32>, ToolArgError> {
    match value.get("ddl_ms") {
        None => Ok(None),
        Some(Json::Number(number)) => {
            let number = *number;
            if !number.is_finite() || number.fract() != 0.0 {
                return Err(ToolArgError::new("参数 ddl_ms 必须是整数"));
            }
            if number < 1.0 || number > f64::from(MAX_DDL_MS) {
                return Err(ToolArgError::new(format!("ddl_ms 必须在 1..={MAX_DDL_MS}")));
            }
            Ok(Some(number as u32))
        }
        Some(_) => Err(ToolArgError::new("参数 ddl_ms 必须是整数")),
    }
}

/// 检查策略；发布命令前调用。
pub fn check_policy(policy: &Policy, call: &ParsedCall) -> Result<(), PolicyDenied> {
    if let Some(worker_id) = call.worker_id()
        && let Some(filter) = &policy.worker_filter
        && filter != worker_id
    {
        return Err(PolicyDenied::WorkerFiltered(String::from(worker_id)));
    }
    match call {
        ParsedCall::ListWorkers => Ok(()),
        ParsedCall::WorkerTask { name, .. } => {
            if policy.allow_tasks {
                Ok(())
            } else {
                Err(PolicyDenied::Forbidden {
                    what: format!("Task {name}"),
                })
            }
        }
        ParsedCall::WorkerTool { name, .. } => {
            if is_read_only_tool(name) {
                Ok(())
            } else if is_write_tool(name) {
                if policy.allow_write {
                    Ok(())
                } else {
                    Err(PolicyDenied::Forbidden {
                        what: format!("写操作 {name}（gpio.config/gpio.write）"),
                    })
                }
            } else if policy.allow_admin {
                Ok(())
            } else {
                Err(PolicyDenied::Forbidden {
                    what: format!("工具 {name}"),
                })
            }
        }
    }
}

/// 工具是否只读。
pub fn is_read_only_tool(name: &str) -> bool {
    READ_ONLY_TOOLS.contains(&name)
}

/// 工具是否为 GPIO 写。
pub fn is_write_tool(name: &str) -> bool {
    WRITE_TOOLS.contains(&name)
}

/// 成功/失败结果转 tool JSON（计划 §5.5）。
pub fn command_result_json(result: &CommandResult) -> Json {
    let mut value = Json::object(Vec::new());
    value.set("ok", Json::Bool(result.ok()));
    value.set("worker_id", Json::string(result.worker_id.as_str()));
    value.set("kind", Json::string(result.kind.as_str()));
    value.set("name", Json::string(result.name.as_str()));
    match &result.failure {
        None => {
            value.set("cmd_id", Json::num(saturating_i64(result.cmd_id)));
            value.set("elapsed_ms", Json::num(saturating_i64(result.elapsed_ms)));
            let (payload, encoding, truncated) = encode_payload(&result.payload);
            value.set("payload", Json::string(payload));
            value.set("encoding", Json::string(encoding));
            if truncated {
                value.set("truncated", Json::Bool(true));
            }
            let phases: Vec<Json> = result.phase_names().into_iter().map(Json::string).collect();
            value.set("phases", Json::Array(phases));
        }
        Some(failure) => {
            let mut error = Json::object(Vec::new());
            error.set("code", Json::string(failure.code.as_str()));
            match failure.worker_code {
                Some(code) => {
                    error.set("worker_code", Json::num(i64::from(code.as_u16())));
                    error.set("worker_code_name", Json::string(format!("{code:?}")));
                }
                None => {
                    error.set("worker_code", Json::Null);
                    error.set("worker_code_name", Json::Null);
                }
            }
            error.set("message", Json::string(failure.message.as_str()));
            error.set("retryable", Json::Bool(failure.retryable));
            value.set("error", error);
        }
    }
    value
}

/// 服务端本地失败（Worker离线/策略拒绝/超时等）转 tool JSON。
pub fn failure_result_json(
    worker_id: &str,
    kind: Option<CommandKind>,
    name: &str,
    code: &str,
    message: &str,
    retryable: bool,
) -> Json {
    let mut error = Json::object(Vec::new());
    error.set("code", Json::string(code));
    error.set("worker_code", Json::Null);
    error.set("worker_code_name", Json::Null);
    error.set("message", Json::string(message));
    error.set("retryable", Json::Bool(retryable));
    let mut value = Json::object(Vec::new());
    value.set("ok", Json::Bool(false));
    value.set("worker_id", Json::string(worker_id));
    match kind {
        Some(kind) => value.set("kind", Json::string(kind.as_str())),
        None => value.set("kind", Json::Null),
    };
    value.set("name", Json::string(name));
    value.set("error", error);
    value
}

/// Worker列表转 JSON。
pub fn workers_json(workers: &[WorkerInfo]) -> Json {
    let items: Vec<Json> = workers.iter().map(worker_json).collect();
    let mut value = Json::object(Vec::new());
    value.set("count", Json::num(items.len() as i64));
    value.set("workers", Json::Array(items));
    value
}

/// 单个Worker转 JSON（`workers_json` 数组项同形状；serve `worker_online`
/// 通知复用同一形状，保证插件侧两处一致）。
pub fn worker_json(worker: &WorkerInfo) -> Json {
    let tools: Vec<Json> = worker
        .tools
        .iter()
        .map(|name| Json::string(name.as_str()))
        .collect();
    let tasks: Vec<Json> = worker
        .tasks
        .iter()
        .map(|name| Json::string(name.as_str()))
        .collect();
    let active_tasks: Vec<Json> = worker
        .active_tasks
        .iter()
        .map(|task| {
            Json::object(vec![
                ("cmd_id", Json::num(saturating_i64(task.cmd_id))),
                ("name", Json::string(task.name.as_str())),
                ("elapsed_ms", Json::num(saturating_i64(task.elapsed_ms))),
            ])
        })
        .collect();
    let jump_workers: Vec<Json> = worker
        .workers
        .iter()
        .map(|(name, host, port)| {
            Json::object(vec![
                ("name", Json::string(name.as_str())),
                ("host", Json::string(host.as_str())),
                ("port", Json::num(i64::from(*port))),
            ])
        })
        .collect();
    Json::object(vec![
        ("worker_id", Json::string(worker.worker_id.as_str())),
        ("online", Json::Bool(worker.online)),
        ("boot_id", Json::num(saturating_i64(worker.boot_id))),
        ("firmware", Json::string(worker.firmware.as_str())),
        ("uptime_ms", Json::num(saturating_i64(worker.uptime_ms))),
        ("unix_ms", Json::num(saturating_i64(worker.unix_ms))),
        ("tools", Json::Array(tools)),
        ("tasks", Json::Array(tasks)),
        ("workers", Json::Array(jump_workers)),
        ("active_tasks", Json::Array(active_tasks)),
    ])
}

fn saturating_i64(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

/// 把载荷转成展示字符串：UTF-8 原样，非 UTF-8 用大写 hex；超限截断。
fn encode_payload(payload: &[u8]) -> (String, &'static str, bool) {
    match std::str::from_utf8(payload) {
        Ok(text) => {
            if text.len() <= PAYLOAD_TRUNCATE_BYTES {
                (String::from(text), "utf8", false)
            } else {
                (
                    String::from(truncate_utf8(text, PAYLOAD_TRUNCATE_BYTES)),
                    "utf8",
                    true,
                )
            }
        }
        Err(_) => {
            let mut hex = String::with_capacity(payload.len().saturating_mul(2));
            for byte in payload {
                hex.push_str(&format!("{byte:02X}"));
            }
            if hex.len() <= PAYLOAD_TRUNCATE_BYTES {
                (hex, "hex", false)
            } else {
                (String::from(&hex[..PAYLOAD_TRUNCATE_BYTES]), "hex", true)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::{CommandFailure, CommandResult};
    use crate::worker::ActiveTaskInfo;
    use sekai::sekai_stdout::Phase;
    use sekai::sekai_wire::ErrorCode;

    fn policy() -> Policy {
        Policy::default()
    }

    fn ok_result(payload: &[u8]) -> CommandResult {
        CommandResult {
            cmd_id: 1_790_000_000_123,
            worker_id: String::from("host-gpio"),
            kind: CommandKind::Tool,
            name: String::from("gpio.read"),
            payload: payload.to_vec(),
            phases: vec![Phase::Accepted, Phase::Completed],
            elapsed_ms: 8,
            failure: None,
        }
    }

    fn failed_result() -> CommandResult {
        CommandResult {
            cmd_id: 7,
            worker_id: String::from("host-gpio"),
            kind: CommandKind::Tool,
            name: String::from("gpio.config"),
            payload: Vec::new(),
            phases: vec![Phase::Accepted, Phase::Failed],
            elapsed_ms: 3,
            failure: Some(CommandFailure::worker(
                ErrorCode::InvalidArgument,
                "引脚 IO6 已保留（SPI flash）",
            )),
        }
    }

    fn sample_worker(worker_id: &str, online: bool) -> WorkerInfo {
        WorkerInfo {
            worker_id: String::from(worker_id),
            online,
            boot_id: 42,
            firmware: String::from("0.1.0"),
            uptime_ms: 1000,
            unix_ms: 1_700_000_000_000,
            tools: vec![
                String::from("gpio.info"),
                String::from("gpio.read"),
                String::from("sekai.info"),
            ],
            tasks: vec![String::from("echo")],
            workers: vec![(String::from("peer"), String::from("10.0.0.2"), 1883)],
            active_tasks: vec![ActiveTaskInfo {
                cmd_id: 9,
                name: String::from("sleep"),
                elapsed_ms: 100,
            }],
            last_seen_mono_ms: 1,
            hello_seq: 3,
        }
    }

    #[test]
    fn tool_defs_are_openai_compatible() {
        let defs = tool_defs();
        assert_eq!(defs.len(), 3);
        assert_eq!(defs[0].name, FN_LIST_WORKERS);
        assert_eq!(defs[1].name, FN_WORKER_TOOL);
        assert_eq!(defs[2].name, FN_WORKER_TASK);
        for def in &defs {
            assert!(
                def.name
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
            );
            assert_eq!(
                def.parameters.get("type").and_then(Json::as_str),
                Some("object")
            );
            assert_eq!(
                def.parameters
                    .get("additionalProperties")
                    .and_then(Json::as_bool),
                Some(false)
            );
        }
        let tool_schema = &defs[1].parameters;
        let required: Vec<&str> = tool_schema
            .get("required")
            .and_then(Json::as_array)
            .unwrap()
            .iter()
            .map(|item| item.as_str().unwrap())
            .collect();
        assert_eq!(required, vec!["worker_id", "tool", "args"]);
        let properties = tool_schema
            .get("properties")
            .and_then(Json::as_object)
            .unwrap();
        let names: Vec<&str> = properties.iter().map(|(key, _)| key.as_str()).collect();
        assert_eq!(names, vec!["worker_id", "tool", "args", "ddl_ms"]);
        let ddl = tool_schema.get_path(&["properties", "ddl_ms"]).unwrap();
        assert_eq!(ddl.get("minimum").and_then(Json::as_u64), Some(1));
        assert_eq!(
            ddl.get("maximum").and_then(Json::as_u64),
            Some(u64::from(MAX_DDL_MS))
        );
        assert!(defs[0].parameters.get("required").is_none());
    }

    #[test]
    fn list_workers_accepts_empty_and_empty_object() {
        assert_eq!(
            parse_call(FN_LIST_WORKERS, "").unwrap(),
            ParsedCall::ListWorkers
        );
        assert_eq!(
            parse_call(FN_LIST_WORKERS, "  ").unwrap(),
            ParsedCall::ListWorkers
        );
        assert_eq!(
            parse_call(FN_LIST_WORKERS, "{}").unwrap(),
            ParsedCall::ListWorkers
        );
        assert!(parse_call(FN_LIST_WORKERS, r#"{"x":1}"#).is_err());
        assert!(parse_call(FN_LIST_WORKERS, "[]").is_err());
        assert!(parse_call(FN_LIST_WORKERS, "nope").is_err());
    }

    #[test]
    fn parses_worker_tool_with_optional_ddl() {
        let call = parse_call(
            FN_WORKER_TOOL,
            r#"{"worker_id":"host-gpio","tool":"gpio.info","args":"help","ddl_ms":500}"#,
        )
        .unwrap();
        assert_eq!(
            call,
            ParsedCall::WorkerTool {
                worker_id: String::from("host-gpio"),
                name: String::from("gpio.info"),
                args: String::from("help"),
                ddl_ms: Some(500),
            }
        );
        assert_eq!(call.function_name(), FN_WORKER_TOOL);
        assert_eq!(call.command_kind(), Some(CommandKind::Tool));
        assert_eq!(call.worker_id(), Some("host-gpio"));
        assert_eq!(call.command_name(), Some("gpio.info"));
        assert_eq!(call.args(), "help");
        assert_eq!(call.ddl_ms(), Some(500));
        let call = parse_call(
            FN_WORKER_TOOL,
            r#"{"worker_id":"d","tool":"gpio.info","args":""}"#,
        )
        .unwrap();
        assert_eq!(call.ddl_ms(), None);
    }

    #[test]
    fn parses_worker_task() {
        let call = parse_call(
            FN_WORKER_TASK,
            r#"{"worker_id":"host-gpio","task":"echo","args":"hello"}"#,
        )
        .unwrap();
        assert_eq!(
            call,
            ParsedCall::WorkerTask {
                worker_id: String::from("host-gpio"),
                name: String::from("echo"),
                args: String::from("hello"),
                ddl_ms: None,
            }
        );
        assert_eq!(call.command_kind(), Some(CommandKind::Task));
        // worker_task 不接受 tool 字段名。
        assert!(
            parse_call(
                FN_WORKER_TASK,
                r#"{"worker_id":"d","tool":"echo","args":""}"#
            )
            .is_err()
        );
    }

    #[test]
    fn rejects_unknown_and_duplicate_fields() {
        assert!(
            parse_call(
                FN_WORKER_TOOL,
                r#"{"worker_id":"d","tool":"gpio.info","args":"","extra":1}"#
            )
            .is_err()
        );
        assert!(
            parse_call(
                FN_WORKER_TOOL,
                r#"{"worker_id":"d","worker_id":"e","tool":"gpio.info","args":""}"#
            )
            .is_err()
        );
    }

    #[test]
    fn rejects_missing_or_wrong_typed_fields() {
        for text in [
            r#"{"tool":"gpio.info","args":""}"#,
            r#"{"worker_id":"d","args":""}"#,
            r#"{"worker_id":"d","tool":"gpio.info"}"#,
            r#"{"worker_id":"d","tool":"gpio.info","args":null}"#,
            r#"{"worker_id":7,"tool":"gpio.info","args":""}"#,
            r#"{"worker_id":"d","tool":7,"args":""}"#,
            r#"{"worker_id":"d","tool":"gpio.info","args":7}"#,
            r#"{"worker_id":"bad id","tool":"gpio.info","args":""}"#,
            r#"{"worker_id":"d","tool":"GPIO.INFO","args":""}"#,
            r#"{"worker_id":"d","tool":"","args":""}"#,
            "not json",
        ] {
            assert!(
                parse_call(FN_WORKER_TOOL, text).is_err(),
                "should reject {text}"
            );
        }
    }

    #[test]
    fn ddl_bounds_are_enforced() {
        let base = r#"{"worker_id":"d","tool":"gpio.info","args":"","ddl_ms":%}"#;
        for value in ["1", "300000"] {
            let text = base.replace('%', value);
            assert_eq!(
                parse_call(FN_WORKER_TOOL, &text).unwrap().ddl_ms(),
                Some(value.parse::<u32>().unwrap())
            );
        }
        for value in ["0", "300001", "1.5", "\"5\"", "null", "true"] {
            let text = base.replace('%', value);
            assert!(
                parse_call(FN_WORKER_TOOL, &text).is_err(),
                "ddl_ms={value} should be rejected"
            );
        }
    }

    #[test]
    fn policy_default_allows_only_readonly() {
        let policy = policy();
        for (name, allowed) in [
            ("gpio.info", true),
            ("gpio.read", true),
            ("sekai.info", true),
            ("sekai.time", true),
            ("sekai.workers", true),
            ("gpio.config", false),
            ("gpio.write", false),
            ("sekai.set_time", false),
            ("sekai.workers.set", false),
            ("sekai.cancel", false),
            ("echo", false),
        ] {
            let call = ParsedCall::WorkerTool {
                worker_id: String::from("dev"),
                name: String::from(name),
                args: String::new(),
                ddl_ms: None,
            };
            assert_eq!(
                check_policy(&policy, &call).is_ok(),
                allowed,
                "tool {name} policy mismatch"
            );
        }
        let task = ParsedCall::WorkerTask {
            worker_id: String::from("dev"),
            name: String::from("echo"),
            args: String::new(),
            ddl_ms: None,
        };
        assert!(check_policy(&policy, &task).is_err());
        assert!(check_policy(&policy, &ParsedCall::ListWorkers).is_ok());
    }

    #[test]
    fn policy_switches_unlock_capabilities() {
        let mut policy = policy();
        policy.allow_write = true;
        policy.allow_tasks = true;
        policy.allow_admin = true;
        for name in ["gpio.config", "gpio.write", "sekai.set_time", "echo"] {
            let call = ParsedCall::WorkerTool {
                worker_id: String::from("dev"),
                name: String::from(name),
                args: String::new(),
                ddl_ms: None,
            };
            assert!(check_policy(&policy, &call).is_ok(), "tool {name}");
        }
        let task = ParsedCall::WorkerTask {
            worker_id: String::from("dev"),
            name: String::from("echo"),
            args: String::new(),
            ddl_ms: None,
        };
        assert!(check_policy(&policy, &task).is_ok());
    }

    #[test]
    fn policy_worker_filter_applies_to_reads_and_writes() {
        let mut policy = policy();
        policy.allow_write = true;
        policy.worker_filter = Some(String::from("allowed"));
        for name in ["gpio.info", "gpio.write"] {
            let call = ParsedCall::WorkerTool {
                worker_id: String::from("other"),
                name: String::from(name),
                args: String::new(),
                ddl_ms: None,
            };
            assert_eq!(
                check_policy(&policy, &call),
                Err(PolicyDenied::WorkerFiltered(String::from("other")))
            );
        }
        let call = ParsedCall::WorkerTool {
            worker_id: String::from("allowed"),
            name: String::from("gpio.info"),
            args: String::new(),
            ddl_ms: None,
        };
        assert!(check_policy(&policy, &call).is_ok());
    }

    #[test]
    fn policy_denied_display_is_informative() {
        let denied = PolicyDenied::Forbidden {
            what: String::from("写操作 gpio.write"),
        };
        assert!(denied.to_string().contains("gpio.write"));
        let denied = PolicyDenied::WorkerFiltered(String::from("other"));
        assert!(denied.to_string().contains("other"));
    }

    #[test]
    fn success_result_json_has_payload_encoding_and_phases() {
        let value = command_result_json(&ok_result(b"pin=IO4 level=1"));
        assert_eq!(value.get("ok").and_then(Json::as_bool), Some(true));
        assert_eq!(
            value.get("worker_id").and_then(Json::as_str),
            Some("host-gpio")
        );
        assert_eq!(value.get("kind").and_then(Json::as_str), Some("tool"));
        assert_eq!(value.get("name").and_then(Json::as_str), Some("gpio.read"));
        assert_eq!(
            value.get("cmd_id").and_then(Json::as_u64),
            Some(1_790_000_000_123)
        );
        assert_eq!(value.get("elapsed_ms").and_then(Json::as_u64), Some(8));
        assert_eq!(
            value.get("payload").and_then(Json::as_str),
            Some("pin=IO4 level=1")
        );
        assert_eq!(value.get("encoding").and_then(Json::as_str), Some("utf8"));
        assert!(value.get("truncated").is_none());
        let phases: Vec<&str> = value
            .get("phases")
            .and_then(Json::as_array)
            .unwrap()
            .iter()
            .map(|item| item.as_str().unwrap())
            .collect();
        assert_eq!(phases, vec!["Accepted", "Completed"]);
    }

    #[test]
    fn non_utf8_payload_uses_uppercase_hex() {
        let value = command_result_json(&ok_result(&[0xFF, 0x0A, 0x01]));
        assert_eq!(value.get("payload").and_then(Json::as_str), Some("FF0A01"));
        assert_eq!(value.get("encoding").and_then(Json::as_str), Some("hex"));
    }

    #[test]
    fn oversized_payload_is_truncated() {
        let big = vec![b'a'; PAYLOAD_TRUNCATE_BYTES + 16];
        let value = command_result_json(&ok_result(&big));
        assert_eq!(value.get("truncated").and_then(Json::as_bool), Some(true));
        let payload = value.get("payload").and_then(Json::as_str).unwrap();
        assert_eq!(payload.len(), PAYLOAD_TRUNCATE_BYTES);

        let big_hex = vec![0xFFu8; PAYLOAD_TRUNCATE_BYTES];
        let value = command_result_json(&ok_result(&big_hex));
        assert_eq!(value.get("truncated").and_then(Json::as_bool), Some(true));
        let payload = value.get("payload").and_then(Json::as_str).unwrap();
        assert_eq!(payload.len(), PAYLOAD_TRUNCATE_BYTES);
        assert!(payload.bytes().all(|byte| byte.is_ascii_hexdigit()));

        let exact = vec![b'a'; PAYLOAD_TRUNCATE_BYTES];
        let value = command_result_json(&ok_result(&exact));
        assert!(value.get("truncated").is_none());
    }

    #[test]
    fn failed_result_json_has_worker_error_details() {
        let value = command_result_json(&failed_result());
        assert_eq!(value.get("ok").and_then(Json::as_bool), Some(false));
        assert_eq!(
            value.get("worker_id").and_then(Json::as_str),
            Some("host-gpio")
        );
        assert_eq!(value.get("kind").and_then(Json::as_str), Some("tool"));
        assert_eq!(
            value.get("name").and_then(Json::as_str),
            Some("gpio.config")
        );
        let error = value.get("error").unwrap();
        assert_eq!(
            error.get("code").and_then(Json::as_str),
            Some("worker_failed")
        );
        assert_eq!(error.get("worker_code").and_then(Json::as_u64), Some(3));
        assert_eq!(
            error.get("worker_code_name").and_then(Json::as_str),
            Some("InvalidArgument")
        );
        assert!(
            error
                .get("message")
                .and_then(Json::as_str)
                .unwrap()
                .contains("IO6")
        );
        assert_eq!(error.get("retryable").and_then(Json::as_bool), Some(false));
        assert!(value.get("payload").is_none());
    }

    #[test]
    fn failure_result_json_shape() {
        let value = failure_result_json(
            "host-gpio",
            Some(CommandKind::Task),
            "echo",
            "timeout",
            "等待最终事件超时",
            true,
        );
        assert_eq!(value.get("ok").and_then(Json::as_bool), Some(false));
        assert_eq!(value.get("kind").and_then(Json::as_str), Some("task"));
        let error = value.get("error").unwrap();
        assert_eq!(error.get("code").and_then(Json::as_str), Some("timeout"));
        assert!(error.get("worker_code").unwrap().is_null());
        assert!(error.get("worker_code_name").unwrap().is_null());
        assert_eq!(error.get("retryable").and_then(Json::as_bool), Some(true));
        let value = failure_result_json(
            "",
            None,
            "unknown",
            CODE_UNKNOWN_TOOL,
            "未知工具函数 unknown",
            false,
        );
        assert!(value.get("kind").unwrap().is_null());
    }

    #[test]
    fn workers_json_lists_capabilities() {
        let value = workers_json(&[
            sample_worker("host-gpio", true),
            sample_worker("host-f1", false),
        ]);
        assert_eq!(value.get("count").and_then(Json::as_u64), Some(2));
        let workers = value.get("workers").and_then(Json::as_array).unwrap();
        assert_eq!(workers.len(), 2);
        let first = &workers[0];
        assert_eq!(
            first.get("worker_id").and_then(Json::as_str),
            Some("host-gpio")
        );
        assert_eq!(first.get("online").and_then(Json::as_bool), Some(true));
        assert_eq!(first.get("boot_id").and_then(Json::as_u64), Some(42));
        assert_eq!(first.get("firmware").and_then(Json::as_str), Some("0.1.0"));
        let tools: Vec<&str> = first
            .get("tools")
            .and_then(Json::as_array)
            .unwrap()
            .iter()
            .map(|item| item.as_str().unwrap())
            .collect();
        assert!(tools.contains(&"gpio.read"));
        let tasks: Vec<&str> = first
            .get("tasks")
            .and_then(Json::as_array)
            .unwrap()
            .iter()
            .map(|item| item.as_str().unwrap())
            .collect();
        assert_eq!(tasks, vec!["echo"]);
        let active = first.get("active_tasks").and_then(Json::as_array).unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].get("name").and_then(Json::as_str), Some("sleep"));
        assert_eq!(
            active[0].get("elapsed_ms").and_then(Json::as_u64),
            Some(100)
        );
        assert_eq!(
            workers[1].get("online").and_then(Json::as_bool),
            Some(false)
        );
        assert_eq!(
            workers[1]
                .get("active_tasks")
                .and_then(Json::as_array)
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn workers_json_handles_empty_list() {
        let value = workers_json(&[]);
        assert_eq!(value.get("count").and_then(Json::as_u64), Some(0));
        assert!(
            value
                .get("workers")
                .and_then(Json::as_array)
                .unwrap()
                .is_empty()
        );
    }
}
