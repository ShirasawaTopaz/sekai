//! `--serve` 无头 JSONL 服务模式（协议 v1，冻结）。
//!
//! 供 DSH 插件子进程驱动：stdin 逐行读 JSONL 请求，stdout 只写 JSONL
//! （每行一个 JSON 对象）；人类可读日志 / 进度一律走 stderr。
//! 网关构造复用 [`crate::link_builder`]，命令解析与策略复用
//! [`crate::tools`]，Worker 通知复用 [`crate::session::Session::poll`]
//! 的基线 diff 手法。
//!
//! ## 线程模型
//!
//! - **主线程**：逐行读 stdin → mpsc → dispatcher（只转发，不写 stdout）；
//! - **dispatcher 线程**：独占协议状态（网关句柄 / 策略），串行处理请求，
//!   写响应与 link_state 通知（调用天然串行，无需额外排队）；
//! - **poll 线程**：约 200ms diff `list_workers()`，发 worker_online /
//!   worker_offline 通知。
//!
//! ## 协议（v1，冻结，插件侧并行开发，不得偏离）
//!
//! - 请求：`{"id":<整数>,"method":"<名>","params":{...}}`
//! - 响应：`{"id":..,"ok":true,"result":{...}}` /
//!   `{"id":..,"ok":false,"error":{"code":"...","message":"..."}}`
//! - 通知（无 id）：`{"event":"ready"}`、
//!   `{"event":"link_state","desc":"..","ready":bool}`、
//!   `{"event":"worker_online","worker_id":"..","worker":{...}}`、
//!   `{"event":"worker_offline","worker_id":".."}`
//! - 方法：link / link_off / workers / tool_call / task / policy.get /
//!   policy.set / status / shutdown
//! - 畸形 JSON 行：跳行回 `invalid_json`；无法取 id 时以通知形式
//!   `{"event":"invalid_json","message":"..."}` 回错，进程不得崩溃。
//! - stdin EOF 或 shutdown 请求：优雅退出，进程退出码 0。

use std::io::{BufRead, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use sekai::sekai_device_list::Endpoint;
use sekai::sekai_wire::validate_worker_id;

use crate::agent::retryable_gateway_code;
use crate::config;
use crate::control::{CommandKind, CommandRequest};
use crate::direct_gateway::DirectMode;
use crate::gateway::{GatewayHandle, WorkerGateway};
use crate::json::Json;
use crate::link_builder::{self, LinkOptions};
use crate::tools::{self, Policy};
use crate::topic::TopicRoot;
use crate::worker::WorkerInfo;

/// poll 线程轮询间隔（毫秒，200ms 级别）。
const POLL_INTERVAL_MS: u64 = 200;
/// 错误码：未知方法。
const CODE_UNKNOWN_METHOD: &str = "unknown_method";
/// 错误码：链路错误（建链失败 / 未连接）。
const CODE_LINK_ERROR: &str = "link_error";
/// 链路关闭后的描述。
const DESC_OFFLINE: &str = "未连接";

// ---------------------------------------------------------------------------
// 配置
// ---------------------------------------------------------------------------

/// serve 模式配置（链路默认值；由 CLI [`config::Config`] 投影，测试用
/// [`ServeConfig::minimal`]）。
#[derive(Debug, Clone)]
pub struct ServeConfig {
    /// `link mode:"tcp"` 默认监听地址。
    pub listen_addr: String,
    /// `link mode:"stdio"` 默认 Worker 程序。
    pub worker_bin: String,
    /// `link mode:"stdio"` 默认追加参数（params.worker_args 覆盖而非追加）。
    pub worker_args: Vec<String>,
    /// `link mode:"mqtt"` 默认 broker 端点（来自 SEKAI_MQTT_ADDR）。
    pub mqtt: Option<Endpoint>,
    /// MQTT 用户名。
    pub mqtt_username: Option<String>,
    /// MQTT 密码。
    pub mqtt_password: Option<String>,
    /// 主题根。
    pub topic_root: TopicRoot,
    /// 默认 DDL（毫秒；请求未带 ddl_ms 时使用）。
    pub default_ddl_ms: u32,
    /// Tool 超时覆盖（绝对毫秒）。
    pub tool_timeout_ms: Option<u64>,
    /// Task 超时覆盖（绝对毫秒）。
    pub task_timeout_ms: Option<u64>,
}

impl ServeConfig {
    /// 从 CLI 配置投影（bin `--serve` 分支构造）。
    pub fn from_config(config: &config::Config) -> Self {
        Self {
            listen_addr: config.listen_addr.clone(),
            worker_bin: config.worker_bin.clone(),
            worker_args: config.worker_args.clone(),
            mqtt: config.mqtt.clone(),
            mqtt_username: config.mqtt_username.clone(),
            mqtt_password: config.mqtt_password.clone(),
            topic_root: config.topic_root.clone(),
            default_ddl_ms: config.ddl_ms,
            tool_timeout_ms: config.timeout_ms,
            task_timeout_ms: config.timeout_ms,
        }
    }

    /// 最小默认集（测试 / 无环境变量场景）。
    pub fn minimal() -> Self {
        Self {
            listen_addr: String::from("127.0.0.1:7421"),
            worker_bin: String::from("sekai-worker"),
            worker_args: Vec::new(),
            mqtt: None,
            mqtt_username: None,
            mqtt_password: None,
            topic_root: TopicRoot::new("sekai/v1").expect("默认主题根合法"),
            default_ddl_ms: 1_000,
            tool_timeout_ms: None,
            task_timeout_ms: None,
        }
    }
}

// ---------------------------------------------------------------------------
// 协议状态机（无 IO；dispatcher 独占，测试可直接驱动）
// ---------------------------------------------------------------------------

/// poll 线程共享视图：当前网关句柄 + Worker 基线。
///
/// dispatcher 在建链 / 关链 / 注入后同步；poll 线程据此 diff 发通知。
/// 两边都按「先视图锁、再网关锁」的顺序拿锁，不会死锁。
#[derive(Default)]
struct PollView {
    handle: Option<GatewayHandle>,
    baseline: Vec<String>,
}

/// serve 协议状态（网关句柄、链路描述、策略；dispatcher 线程独占）。
pub struct ServeState {
    gateway: Option<GatewayHandle>,
    /// 链路描述（人读；status.link 与 link_state.desc 用）。
    link_desc: String,
    /// 链路标签（tcp/stdio/mqtt/off）。
    link_mode: &'static str,
    policy: Policy,
    config: ServeConfig,
    poll_view: Arc<Mutex<PollView>>,
}

impl ServeState {
    /// 构造状态（初始未连接）。
    pub fn new(config: ServeConfig, policy: Policy) -> Self {
        Self {
            gateway: None,
            link_desc: String::from(DESC_OFFLINE),
            link_mode: "off",
            policy,
            config,
            poll_view: Arc::new(Mutex::new(PollView::default())),
        }
    }

    /// 注入网关（测试 / 嵌入场景）；`mode` 为链路标签，`desc` 为链路描述。
    /// 注入即基线：poll 只报此后新增 / 消失的 Worker。
    pub fn set_gateway(
        &mut self,
        gateway: GatewayHandle,
        mode: &'static str,
        desc: impl Into<String>,
    ) {
        self.gateway = Some(gateway);
        self.link_mode = mode;
        self.link_desc = desc.into();
        self.sync_poll_view();
    }

    /// poll 共享视图（serve_loop 转交 poll 线程）。
    fn poll_view(&self) -> Arc<Mutex<PollView>> {
        Arc::clone(&self.poll_view)
    }

    /// 当前策略（测试用）。
    pub fn policy(&self) -> &Policy {
        &self.policy
    }

    /// 当前链路描述（测试用）。
    pub fn link_desc(&self) -> &str {
        &self.link_desc
    }

    /// 处理一行输入；返回（JSON 输出行, 是否退出）。
    ///
    /// 任何畸形输入只影响该行（跳行回错），不会 panic，进程继续。
    pub fn handle_line(&mut self, line: &str) -> (Vec<Json>, bool) {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            return (Vec::new(), false);
        }
        let value = match Json::parse(trimmed) {
            Ok(value) => value,
            Err(error) => {
                // 畸形 JSON 取不到 id：以通知形式回 invalid_json
                // （JsonError 的 Display 自带「JSON 解析失败（偏移 N）：」前缀）。
                return (vec![invalid_json_notification(&error.to_string())], false);
            }
        };
        if value.as_object().is_none() {
            return (
                vec![invalid_json_notification("请求必须是 JSON 对象")],
                false,
            );
        }
        let id = match value.get("id") {
            None => {
                return (vec![invalid_json_notification("请求缺少 id")], false);
            }
            Some(id) => match id.as_i64() {
                Some(id) => id,
                None => {
                    return (vec![invalid_json_notification("请求 id 必须是整数")], false);
                }
            },
        };
        let method = match value.get("method") {
            Some(Json::String(method)) => method.clone(),
            None => {
                return (
                    vec![error_response(
                        id,
                        tools::CODE_INVALID_ARGUMENTS,
                        "缺少必填参数 method",
                    )],
                    false,
                );
            }
            Some(_) => {
                return (
                    vec![error_response(
                        id,
                        tools::CODE_INVALID_ARGUMENTS,
                        "参数 method 必须是字符串",
                    )],
                    false,
                );
            }
        };
        let params = match value.get("params") {
            None => Json::object(Vec::new()),
            Some(value) if value.as_object().is_some() => value.clone(),
            Some(_) => {
                return (
                    vec![error_response(
                        id,
                        tools::CODE_INVALID_ARGUMENTS,
                        "参数 params 必须是 JSON 对象",
                    )],
                    false,
                );
            }
        };
        self.dispatch(id, &method, &params)
    }

    /// 方法分发（参数已通过结构校验）。
    fn dispatch(&mut self, id: i64, method: &str, params: &Json) -> (Vec<Json>, bool) {
        match method {
            "link" => (self.method_link(id, params), false),
            "link_off" => (self.method_link_off(id), false),
            "workers" => (vec![self.method_workers(id)], false),
            "tool_call" => (self.method_command(id, params, true), false),
            "task" => (self.method_command(id, params, false), false),
            "policy.get" => (vec![ok_response(id, self.policy_json())], false),
            "policy.set" => (self.method_policy_set(id, params), false),
            "status" => (vec![ok_response(id, self.status_json())], false),
            // shutdown：先回响应再退出（退出动作由运行层执行）。
            "shutdown" => (vec![ok_response(id, Json::Null)], true),
            other => (
                vec![error_response(
                    id,
                    CODE_UNKNOWN_METHOD,
                    &format!("未知方法 {other}"),
                )],
                false,
            ),
        }
    }

    // ---- link / link_off ----

    /// link 方法：字段白名单校验后按 mode 分发。
    fn method_link(&mut self, id: i64, params: &Json) -> Vec<Json> {
        const ALLOWED: &[&str] = &[
            "mode",
            "listen_addr",
            "worker_bin",
            "worker_args",
            "mqtt_addr",
        ];
        if let Err(error) = tools::check_fields(params, ALLOWED) {
            return vec![error_response(
                id,
                tools::CODE_INVALID_ARGUMENTS,
                &error.to_string(),
            )];
        }
        let mode = match params.get("mode") {
            Some(Json::String(mode)) => mode.as_str(),
            None => {
                return vec![error_response(
                    id,
                    tools::CODE_INVALID_ARGUMENTS,
                    "缺少必填参数 mode",
                )];
            }
            Some(_) => {
                return vec![error_response(
                    id,
                    tools::CODE_INVALID_ARGUMENTS,
                    "参数 mode 必须是字符串",
                )];
            }
        };
        match mode {
            "tcp" => self.link_tcp(id, params),
            "stdio" => self.link_stdio(id, params),
            "mqtt" => self.link_mqtt(id, params),
            other => vec![error_response(
                id,
                tools::CODE_INVALID_ARGUMENTS,
                &format!("mode 不支持 {other}（可选 tcp/stdio/mqtt）"),
            )],
        }
    }

    /// tcp 链路：监听等 Worker 连入；成功回实际监听地址（端口 0 已展开）。
    fn link_tcp(&mut self, id: i64, params: &Json) -> Vec<Json> {
        let listen_addr = match optional_string(params, "listen_addr") {
            Ok(addr) => addr.unwrap_or_else(|| self.config.listen_addr.clone()),
            Err(message) => {
                return vec![error_response(id, tools::CODE_INVALID_ARGUMENTS, &message)];
            }
        };
        let attempted = DirectMode::TcpListen {
            addr: listen_addr.clone(),
        }
        .describe();
        // 先关旧链路（失败时旧链路也已让位，与 REPL /link 语义一致）。
        self.link_off_quiet();
        match link_builder::listen_tcp(&self.link_options(), &listen_addr) {
            Ok((gateway, actual)) => {
                let desc = gateway.mode().describe();
                self.link_desc = desc.clone();
                self.link_mode = "tcp";
                self.gateway = Some(link_builder::handle_of(gateway));
                self.sync_poll_view();
                vec![
                    ok_response(
                        id,
                        Json::object(vec![
                            ("mode", Json::string("tcp")),
                            ("desc", Json::string(desc.as_str())),
                            ("addr", Json::string(actual.as_str())),
                        ]),
                    ),
                    link_state_notification(&desc, true),
                ]
            }
            Err(error) => vec![
                error_response(id, CODE_LINK_ERROR, &format!("监听失败：{error}")),
                link_state_notification(&attempted, false),
            ],
        }
    }

    /// stdio 链路：拉起 Worker 子进程。
    fn link_stdio(&mut self, id: i64, params: &Json) -> Vec<Json> {
        let worker_bin = match optional_string(params, "worker_bin") {
            Ok(program) => program.unwrap_or_else(|| self.config.worker_bin.clone()),
            Err(message) => {
                return vec![error_response(id, tools::CODE_INVALID_ARGUMENTS, &message)];
            }
        };
        let worker_args = match params.get("worker_args") {
            None => self.config.worker_args.clone(),
            Some(Json::Array(items)) => {
                let mut args = Vec::with_capacity(items.len());
                let mut all_strings = true;
                for item in items {
                    match item.as_str() {
                        Some(text) => args.push(String::from(text)),
                        None => {
                            all_strings = false;
                            break;
                        }
                    }
                }
                if !all_strings {
                    return vec![error_response(
                        id,
                        tools::CODE_INVALID_ARGUMENTS,
                        "参数 worker_args 必须是字符串数组",
                    )];
                }
                args
            }
            Some(_) => {
                return vec![error_response(
                    id,
                    tools::CODE_INVALID_ARGUMENTS,
                    "参数 worker_args 必须是字符串数组",
                )];
            }
        };
        let attempted = DirectMode::StdioSpawn {
            program: worker_bin.clone(),
        }
        .describe();
        self.link_off_quiet();
        match link_builder::spawn_stdio(&self.link_options(), &worker_bin, &worker_args) {
            Ok((gateway, _)) => {
                let desc = gateway.mode().describe();
                self.link_desc = desc.clone();
                self.link_mode = "stdio";
                self.gateway = Some(link_builder::handle_of(gateway));
                self.sync_poll_view();
                vec![
                    ok_response(
                        id,
                        Json::object(vec![
                            ("mode", Json::string("stdio")),
                            ("desc", Json::string(desc.as_str())),
                        ]),
                    ),
                    link_state_notification(&desc, true),
                ]
            }
            Err(error) => vec![
                error_response(id, CODE_LINK_ERROR, &format!("拉起失败：{error}")),
                link_state_notification(&attempted, false),
            ],
        }
    }

    /// mqtt 链路：连接 broker。
    fn link_mqtt(&mut self, id: i64, params: &Json) -> Vec<Json> {
        let endpoint = match optional_string(params, "mqtt_addr") {
            Ok(Some(text)) => match Endpoint::parse(&text) {
                Ok(endpoint) => endpoint,
                Err(error) => {
                    return vec![error_response(
                        id,
                        tools::CODE_INVALID_ARGUMENTS,
                        &format!("mqtt_addr 非法：{error}"),
                    )];
                }
            },
            Ok(None) => match self.config.mqtt.clone() {
                Some(endpoint) => endpoint,
                None => {
                    return vec![error_response(
                        id,
                        tools::CODE_INVALID_ARGUMENTS,
                        "缺少 broker 地址：传 mqtt_addr 或设置 SEKAI_MQTT_ADDR",
                    )];
                }
            },
            Err(message) => {
                return vec![error_response(id, tools::CODE_INVALID_ARGUMENTS, &message)];
            }
        };
        let attempted = format!("mqtt broker（{endpoint}）");
        self.link_off_quiet();
        match link_builder::connect_mqtt(&self.link_options(), &endpoint) {
            Ok((gateway, display)) => {
                let desc = format!("mqtt broker（{display}）");
                self.link_desc = desc.clone();
                self.link_mode = "mqtt";
                self.gateway = Some(link_builder::handle_of(gateway));
                self.sync_poll_view();
                vec![
                    ok_response(
                        id,
                        Json::object(vec![
                            ("mode", Json::string("mqtt")),
                            ("desc", Json::string(desc.as_str())),
                        ]),
                    ),
                    link_state_notification(&desc, true),
                ]
            }
            Err(error) => vec![
                error_response(id, CODE_LINK_ERROR, &format!("连接失败：{error}")),
                link_state_notification(&attempted, false),
            ],
        }
    }

    /// link_off 方法：关闭当前链路（幂等）。
    fn method_link_off(&mut self, id: i64) -> Vec<Json> {
        let had = self.link_off_quiet();
        let mut messages = vec![ok_response(
            id,
            Json::object(vec![
                ("mode", Json::string("off")),
                ("desc", Json::string(DESC_OFFLINE)),
                ("had_link", Json::Bool(had)),
            ]),
        )];
        if had {
            messages.push(link_state_notification(DESC_OFFLINE, false));
        }
        messages
    }

    /// 关闭当前链路（若有）并同步 poll 视图；返回是否关闭了链路。
    fn link_off_quiet(&mut self) -> bool {
        if self.gateway.take().is_some() {
            // 旧句柄 Drop：MqttGateway/DirectGateway 在 Drop 中停 IO 线程。
            self.link_desc = String::from(DESC_OFFLINE);
            self.link_mode = "off";
            self.sync_poll_view();
            true
        } else {
            false
        }
    }

    // ---- workers / tool_call / task ----

    /// workers 方法：{count,workers[]}（workers_json 形状）。
    fn method_workers(&mut self, id: i64) -> Json {
        let Some(mut gateway) = self.gateway.clone() else {
            return error_response(id, CODE_LINK_ERROR, "未连接：先 link tcp|stdio|mqtt");
        };
        ok_response(id, tools::workers_json(&gateway.list_workers()))
    }

    /// tool_call / task 方法：参数解析复用 tools::parse_call 严格校验，
    /// 策略在发布命令之前强制执行；结果与 agent tool 结果同形状
    /// （成功 command_result_json，本地 / 网关失败 failure_result_json）。
    fn method_command(&mut self, id: i64, params: &Json, is_tool: bool) -> Vec<Json> {
        let function = if is_tool {
            tools::FN_WORKER_TOOL
        } else {
            tools::FN_WORKER_TASK
        };
        let parsed = match tools::parse_call(function, &params.to_compact_string()) {
            Ok(parsed) => parsed,
            Err(error) => {
                return vec![error_response(
                    id,
                    tools::CODE_INVALID_ARGUMENTS,
                    &error.to_string(),
                )];
            }
        };
        let worker_id = String::from(parsed.worker_id().unwrap_or_default());
        let name = String::from(parsed.command_name().unwrap_or_default());
        let kind = if is_tool {
            CommandKind::Tool
        } else {
            CommandKind::Task
        };
        let Some(mut gateway) = self.gateway.clone() else {
            return vec![error_response(
                id,
                CODE_LINK_ERROR,
                "未连接：先 link tcp|stdio|mqtt",
            )];
        };
        if let Err(denied) = tools::check_policy(&self.policy, &parsed) {
            return vec![ok_response(
                id,
                tools::failure_result_json(
                    &worker_id,
                    Some(kind),
                    &name,
                    tools::CODE_POLICY_DENIED,
                    &denied.to_string(),
                    false,
                ),
            )];
        }
        let request = CommandRequest {
            timeout_ms: if is_tool {
                self.config.tool_timeout_ms
            } else {
                self.config.task_timeout_ms
            },
            ddl_ms: parsed.ddl_ms().unwrap_or(self.config.default_ddl_ms),
            worker_id: worker_id.clone(),
            kind,
            name: name.clone(),
            args: parsed.args().as_bytes().to_vec(),
        };
        match gateway.call(request) {
            Ok(result) => vec![ok_response(id, tools::command_result_json(&result))],
            Err(error) => vec![ok_response(
                id,
                tools::failure_result_json(
                    &worker_id,
                    Some(kind),
                    &name,
                    &error.code,
                    &error.message,
                    retryable_gateway_code(&error.code),
                ),
            )],
        }
    }

    // ---- policy ----

    /// 策略对象（policy.get 结果 / policy.set 返回值共用形状）。
    fn policy_json(&self) -> Json {
        Json::object(vec![
            ("allow_write", Json::Bool(self.policy.allow_write)),
            ("allow_tasks", Json::Bool(self.policy.allow_tasks)),
            ("allow_admin", Json::Bool(self.policy.allow_admin)),
            (
                "worker_filter",
                match &self.policy.worker_filter {
                    Some(worker_id) => Json::string(worker_id.as_str()),
                    None => Json::Null,
                },
            ),
        ])
    }

    /// policy.set 方法：只改给出的字段，返回更新后的完整策略。
    fn method_policy_set(&mut self, id: i64, params: &Json) -> Vec<Json> {
        const ALLOWED: &[&str] = &["allow_write", "allow_tasks", "allow_admin", "worker_filter"];
        if let Err(error) = tools::check_fields(params, ALLOWED) {
            return vec![error_response(
                id,
                tools::CODE_INVALID_ARGUMENTS,
                &error.to_string(),
            )];
        }
        for (key, slot) in [
            ("allow_write", &mut self.policy.allow_write),
            ("allow_tasks", &mut self.policy.allow_tasks),
            ("allow_admin", &mut self.policy.allow_admin),
        ] {
            match params.get(key) {
                None => {}
                Some(Json::Bool(value)) => *slot = *value,
                Some(_) => {
                    return vec![error_response(
                        id,
                        tools::CODE_INVALID_ARGUMENTS,
                        &format!("参数 {key} 必须是布尔值"),
                    )];
                }
            }
        }
        match params.get("worker_filter") {
            None => {}
            Some(Json::Null) => self.policy.worker_filter = None,
            Some(Json::String(text)) => {
                if let Err(message) = validate_worker_id(text) {
                    return vec![error_response(
                        id,
                        tools::CODE_INVALID_ARGUMENTS,
                        &format!("worker_filter 非法：{message}"),
                    )];
                }
                self.policy.worker_filter = Some(text.clone());
            }
            Some(_) => {
                return vec![error_response(
                    id,
                    tools::CODE_INVALID_ARGUMENTS,
                    "参数 worker_filter 必须是字符串或 null",
                )];
            }
        }
        vec![ok_response(id, self.policy_json())]
    }

    // ---- status ----

    /// status 方法：链路描述 / 模式 / 就绪 / Worker 数 / 策略摘要 / 版本 / pid。
    fn status_json(&self) -> Json {
        let (ready, worker_count) = match self.gateway.clone() {
            Some(mut gateway) => (gateway.is_ready(), gateway.list_workers().len()),
            None => (false, 0),
        };
        Json::object(vec![
            ("link", Json::string(self.link_desc.as_str())),
            ("mode", Json::string(self.link_mode)),
            ("ready", Json::Bool(ready)),
            ("worker_count", Json::num(worker_count as i64)),
            ("policy", Json::string(self.policy.summary())),
            ("version", Json::string(config::version_text())),
            ("pid", Json::num(i64::from(std::process::id()))),
        ])
    }

    // ---- 辅助 ----

    /// 链路构造参数（worker_id_hint 取策略里的 Worker 过滤器，无则兜底）。
    fn link_options(&self) -> LinkOptions {
        LinkOptions {
            topic_root: self.config.topic_root.clone(),
            default_ddl_ms: self.config.default_ddl_ms,
            tool_timeout_ms: self.config.tool_timeout_ms,
            task_timeout_ms: self.config.task_timeout_ms,
            worker_id_hint: self
                .policy
                .worker_filter
                .clone()
                .unwrap_or_else(|| String::from("pc-worker")),
            mqtt_username: self.config.mqtt_username.clone(),
            mqtt_password: self.config.mqtt_password.clone(),
            mqtt_client_id: None,
        }
    }

    /// 把当前网关与 Worker 基线同步进 poll 共享视图（建链 / 关链 / 注入后）。
    fn sync_poll_view(&mut self) {
        let mut view = self
            .poll_view
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        view.handle = self.gateway.clone();
        view.baseline = match self.gateway.clone() {
            Some(mut gateway) => gateway
                .list_workers()
                .into_iter()
                .map(|worker| worker.worker_id)
                .collect(),
            None => Vec::new(),
        };
    }
}

// ---------------------------------------------------------------------------
// 通知与响应构造
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

/// ready 通知（启动即发，stdout 第一行）。
fn ready_notification() -> Json {
    Json::object(vec![("event", Json::string("ready"))])
}

/// link_state 通知：ready 表示链路本身建立成功（tcp 已监听 / stdio 子进程
/// 已拉起 / mqtt broker 已连接）；Worker 上线由 worker_online 单独通知。
fn link_state_notification(desc: &str, ready: bool) -> Json {
    Json::object(vec![
        ("event", Json::string("link_state")),
        ("desc", Json::string(desc)),
        ("ready", Json::Bool(ready)),
    ])
}

/// 对比基线产出 worker_online / worker_offline 通知（Session::poll 的基线
/// diff 手法，另加离线方向）；同时把基线推进为当前集合。
///
/// poll 线程与测试共用：线上报完整 Worker 快照（workers_json 数组项形状），
/// 离线只报 worker_id。
pub fn worker_notifications(baseline: &mut Vec<String>, workers: &[WorkerInfo]) -> Vec<Json> {
    let current: Vec<String> = workers
        .iter()
        .map(|worker| worker.worker_id.clone())
        .collect();
    let mut notifications = Vec::new();
    for worker in workers {
        if !baseline.contains(&worker.worker_id) {
            notifications.push(Json::object(vec![
                ("event", Json::string("worker_online")),
                ("worker_id", Json::string(worker.worker_id.as_str())),
                ("worker", tools::worker_json(worker)),
            ]));
        }
    }
    for id in baseline.iter() {
        if !current.contains(id) {
            notifications.push(Json::object(vec![
                ("event", Json::string("worker_offline")),
                ("worker_id", Json::string(id.as_str())),
            ]));
        }
    }
    *baseline = current;
    notifications
}

/// 对象里的可选字符串字段；None 表示未给出，类型不符报中文错误。
fn optional_string(params: &Json, key: &str) -> Result<Option<String>, String> {
    match params.get(key) {
        None | Some(Json::Null) => Ok(None),
        Some(Json::String(text)) => Ok(Some(text.clone())),
        Some(_) => Err(format!("参数 {key} 必须是字符串")),
    }
}

// ---------------------------------------------------------------------------
// 运行层（线程编排）
// ---------------------------------------------------------------------------

/// 初始链路注入（测试 / 嵌入场景）：跳过 link 请求直接挂上网关。
pub struct InjectedLink {
    /// 网关句柄。
    pub handle: GatewayHandle,
    /// 链路标签（status.mode）。
    pub mode: &'static str,
    /// 链路描述（status.link 与 link_state.desc）。
    pub desc: String,
}

/// JSONL 行输出：每行一个 JSON 对象，写后即 flush（插件按行消费）。
struct JsonlWriter<W: Write> {
    inner: W,
}

impl<W: Write> JsonlWriter<W> {
    fn new(inner: W) -> Self {
        Self { inner }
    }

    fn write_line(&mut self, value: &Json) {
        if let Err(error) = writeln!(self.inner, "{}", value.to_compact_string()) {
            // stdout 断裂（插件已退出）：诊断走 stderr，进程随后随 EOF 收场。
            eprintln!("sekai-router: serve 写 stdout 失败：{error}");
        }
    }

    fn flush(&mut self) {
        if let Err(error) = self.inner.flush() {
            eprintln!("sekai-router: serve 刷 stdout 失败：{error}");
        }
    }
}

/// 经共享 writer 写一行 JSONL。
fn write_shared_line<W: Write>(writer: &Arc<Mutex<JsonlWriter<W>>>, value: &Json) {
    let mut writer = writer
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    writer.write_line(value);
}

/// serve 运行入口（测试可注入输入 / 输出 / 初始链路 / 退出动作）。
///
/// - `input`：JSONL 请求源（生产为 stdin）；
/// - `output`：JSONL 输出目标（生产为 stdout）；
/// - `injected`：初始链路注入（测试；正常启动为 None）；
/// - `terminate`：shutdown 响应写完后的收场动作。生产环境主线程多半仍
///   阻塞在 stdin 读上，只能进程级退出（`std::process::exit(0)`）；测试传
///   no-op，靠输入 EOF 让各线程正常汇合。
///
/// 返回进程退出码：EOF / shutdown 均为优雅退出（0）；仅线程启动失败为 1。
pub fn serve_loop<R, W, F>(
    input: R,
    output: W,
    config: ServeConfig,
    policy: Policy,
    injected: Option<InjectedLink>,
    terminate: F,
) -> i32
where
    R: BufRead,
    W: Write + Send + 'static,
    F: Fn() + Send + 'static,
{
    let writer = Arc::new(Mutex::new(JsonlWriter::new(output)));
    // 启动即宣告 ready（stdout 第一行）。
    write_shared_line(&writer, &ready_notification());

    let mut state = ServeState::new(config, policy);
    if let Some(injected) = injected {
        state.set_gateway(injected.handle, injected.mode, injected.desc);
    }
    let poll_view = state.poll_view();
    let (tx, rx) = mpsc::channel::<String>();
    let stop = Arc::new(AtomicBool::new(false));

    // dispatcher：唯一状态所有者，串行处理请求（调用天然串行）。
    let dispatcher_writer = Arc::clone(&writer);
    let dispatcher_stop = Arc::clone(&stop);
    let dispatcher = std::thread::Builder::new()
        .name(String::from("sekai-router-serve"))
        .spawn(move || {
            while let Ok(line) = rx.recv() {
                let (messages, exit) = state.handle_line(&line);
                for message in &messages {
                    write_shared_line(&dispatcher_writer, message);
                }
                if exit {
                    // shutdown：响应已写。停 poll、拆链路（Drop 停各 IO 线程），
                    // 再交给 terminate 收场（生产直接 exit(0)）。
                    dispatcher_stop.store(true, Ordering::Relaxed);
                    state.link_off_quiet();
                    let mut writer = dispatcher_writer
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    writer.flush();
                    drop(writer);
                    terminate();
                    break;
                }
            }
            // stdin EOF（通道关闭）：停 poll，主线程随后 join 返回。
            dispatcher_stop.store(true, Ordering::Relaxed);
        });
    let dispatcher = match dispatcher {
        Ok(handle) => handle,
        Err(error) => {
            eprintln!("sekai-router: serve dispatcher 线程启动失败：{error}");
            return 1;
        }
    };

    // poll 线程：~200ms diff Worker 集合，发上线 / 离线通知。
    let poll_writer = Arc::clone(&writer);
    let poll_stop = Arc::clone(&stop);
    let poller = std::thread::Builder::new()
        .name(String::from("sekai-router-serve-poll"))
        .spawn(move || {
            loop {
                std::thread::sleep(Duration::from_millis(POLL_INTERVAL_MS));
                if poll_stop.load(Ordering::Relaxed) {
                    break;
                }
                let notifications = {
                    let mut view = poll_view
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    let Some(mut handle) = view.handle.clone() else {
                        continue;
                    };
                    worker_notifications(&mut view.baseline, &handle.list_workers())
                };
                for notification in &notifications {
                    write_shared_line(&poll_writer, notification);
                }
            }
        });
    let poller = match poller {
        Ok(handle) => handle,
        Err(error) => {
            eprintln!("sekai-router: serve poll 线程启动失败：{error}");
            stop.store(true, Ordering::Relaxed);
            let _ = dispatcher.join();
            return 1;
        }
    };

    // 主线程：逐行读输入 → mpsc → dispatcher（stdout 只由 dispatcher/poll 写）。
    let mut input = input;
    loop {
        let mut line = String::new();
        match input.read_line(&mut line) {
            Ok(0) => break, // EOF：优雅退出。
            Ok(_) => {
                if tx.send(line).is_err() {
                    break; // dispatcher 已退出（shutdown）。
                }
            }
            Err(error) => {
                eprintln!("sekai-router: serve 读取输入失败：{error}");
                break;
            }
        }
    }
    drop(tx);
    let _ = dispatcher.join();
    stop.store(true, Ordering::Relaxed);
    let _ = poller.join();
    let mut writer = writer
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    writer.flush();
    0
}

/// 生产 stdout 适配：每次写锁全局 stdout（StdoutLock 守卫本身不跨线程
/// 持有；行完整性由 serve_loop 内的共享 writer 互斥保证）。
struct SharedStdout;

impl Write for SharedStdout {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        std::io::stdout().write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        std::io::stdout().flush()
    }
}

/// 生产入口：stdin / stdout + 进程级退出语义（bin 的 `--serve` 分支调用）。
pub fn run(config: ServeConfig, policy: Policy) -> i32 {
    eprintln!("sekai-router: serve 模式启动（协议 v1 JSONL，stdout 只输出 JSON）");
    let stdin = std::io::stdin();
    serve_loop(stdin.lock(), SharedStdout, config, policy, None, || {
        // 主线程通常仍阻塞在 stdin 读上，只能进程级退出；
        // shutdown 响应与 flush 已在 dispatcher 内完成。
        std::process::exit(0);
    })
}
