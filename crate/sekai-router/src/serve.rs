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
//! - **dispatcher 线程**：独占协议状态（网关句柄 / 策略），串行处理请求；
//!   **长方法（tool_call / task / prompt / jump / admin）不在这里阻塞**：
//!   dispatcher 拆出 [`Deferred`] 任务（含已校验请求与网关克隆）到工作线程，
//!   结果经共享写队列按 id 回写 JSONL 行（行完整；乱序由插件按 id 匹配）；
//! - **后台命令线程**：每请求一个，执行 `submit + wait` 或 LLM 调用；
//!   `async:true` 的请求在 dispatcher 内完成提交、立刻回 `accepted`，
//!   完成时工作线程发无 id 的 `{"method":"command_result",...}` 通知；
//! - **poll 线程**：约 200ms diff `list_workers()` 发 worker_online /
//!   worker_offline；同时观测网关 ready 由 true→false / false→true 时补发
//!   `link_state` 通知（B11）；
//! - **prompt ingress 线程**（--prompt-addr 启用时）：TCP JSONL 通道，
//!   见 [`crate::prompt_server`]。
//!
//! ## 协议（v1，冻结，插件侧并行开发，不得偏离）
//!
//! **v1.1 additive**：新增 `prompt` 方法（参数/响应形状与错误码见
//! [`crate::prompt_server`]；由 Router 调 LLM 后返回回答）。
//!
//! **v1.2 additive**：`tool_call` / `task` 支持 `async:true`；新增
//! `cancel {cmd_id}` / `commands` / `admin` / `jump` 方法；`link` 响应追加
//! 实际监听地址键（tcp:`listen_addr`、mqtt:`mqtt_addr`）；`status` 追加
//! `link_info` / `inflight` / `counters` / `clock_synced`；新增无 id 的
//! `{"method":"command_result","params":{...}}` 完成通知。均为纯增量——
//! 旧插件不调用 / 不读取时行为不变。
//!
//! - 请求：`{"id":<整数>,"method":"<名>","params":{...}}`
//! - 响应：`{"id":..,"ok":true,"result":{...}}` /
//!   `{"id":..,"ok":false,"error":{"code":"...","message":"..."}}`
//! - 通知（无 id）：`{"event":"ready"}`、
//!   `{"event":"link_state","desc":"..","ready":bool}`、
//!   `{"event":"worker_online","worker_id":"..","worker":{...}}`、
//!   `{"event":"worker_offline","worker_id":".."}`
//! - 方法：link / link_off / workers / tool_call / task / policy.get /
//!   policy.set / status / shutdown / prompt（v1.1 additive）/
//!   cancel / commands / jump / admin（v1.2 additive）
//! - 畸形 JSON 行：跳行回 `invalid_json`；无法取 id 时以通知形式
//!   `{"event":"invalid_json","message":"..."}` 回错，进程不得崩溃。
//! - stdin EOF 或 shutdown 请求：优雅退出，进程退出码 0。

use std::collections::BTreeMap;
use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use sekai::sekai_device_list::{Device, DeviceList, Endpoint};
use sekai::sekai_wire::validate_worker_id;

use crate::agent::AgentConfig;
use crate::agent::retryable_gateway_code;
use crate::config;
use crate::control::{
    CommandKind, CommandRequest, GatewayDiagnostics, InflightCommand, LateResult,
};
use crate::direct_gateway::DirectMode;
use crate::gateway::{GatewayHandle, WorkerGateway};
use crate::http::StdHttpClient;
use crate::json::Json;
use crate::link_builder::{self, LinkOptions};
use crate::llm::{self, LlmClient, LlmConfig};
use crate::prompt_server::{self, PromptCore};
use crate::tools::{self, Policy};
use crate::topic::TopicRoot;
use crate::trace::TraceWriter;
use crate::worker::WorkerInfo;

/// poll 线程轮询间隔（毫秒，200ms 级别）。
const POLL_INTERVAL_MS: u64 = 200;
/// 错误码：未知方法。
const CODE_UNKNOWN_METHOD: &str = "unknown_method";
/// 错误码：链路错误（建链失败 / 未连接）。
const CODE_LINK_ERROR: &str = "link_error";
/// 链路关闭后的描述。
const DESC_OFFLINE: &str = "未连接";
/// `status` / `commands` 里最多列出的在途命令条数。
const MAX_LISTED_INFLIGHT: usize = 32;

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
    /// DeepSeek API key（`prompt` 方法用；None/空 → `llm_unavailable`）。
    pub api_key: Option<String>,
    /// OpenAI 兼容 base URL（prompt 方法用）。
    pub base_url: String,
    /// 模型名（prompt 方法用）。
    pub model: String,
    /// Agent 最大步数（prompt 方法用；请求 max_steps 可覆盖到 1..=32）。
    ///
    /// `None` = 不设上限（直连 PC Worker 的缺省；见 `config::Config::max_steps`）。
    pub max_steps: Option<u32>,
    /// 步数上限是否随链路自动取值（未显式 `--max-steps`/`SEKAI_MAX_STEPS`）。
    ///
    /// 为 `true` 时，运行期 `link` 请求改写链路后按新链路重算默认值并同步给
    /// prompt hub（直连 PC Worker 不设上限、MQTT 保留 12）。
    pub max_steps_auto: bool,
    /// 温度（prompt 方法用）。
    pub temperature: f64,
    /// 最大输出 token（prompt 方法用）。
    pub max_tokens: u32,
    /// prompt ingress 监听地址（`--prompt-addr`；None = 不起 ingress，
    /// serve 的 `prompt` 方法仍可用）。
    pub prompt_addr: Option<String>,
    /// trace JSONL 路径（`--trace`；serve 下对 prompt/agent 生效，B8）。
    pub trace_path: Option<PathBuf>,
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
            api_key: config.api_key.clone(),
            base_url: config.base_url.clone(),
            model: config.model.clone(),
            max_steps: config.max_steps,
            max_steps_auto: !config.max_steps_explicit(),
            temperature: 0.0,
            max_tokens: 4_096,
            prompt_addr: config.prompt_addr.clone(),
            trace_path: config.trace_path.clone(),
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
            default_ddl_ms: config::DEFAULT_DDL_MS,
            tool_timeout_ms: None,
            task_timeout_ms: None,
            api_key: None,
            base_url: String::from(config::DEFAULT_BASE_URL),
            model: String::from(config::DEFAULT_MODEL),
            // 测试/嵌入场景没有链路信息，取非 PC Worker 的保守默认（12）。
            max_steps: Some(config::DEFAULT_MAX_STEPS),
            // 默认不随运行期 link 自动改步数（显式行为更可控）；生产入口
            // 经 `ServeConfig::from_config` 按 CLI 配置开启。
            max_steps_auto: false,
            temperature: 0.0,
            max_tokens: 4_096,
            prompt_addr: None,
            trace_path: None,
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
    /// 上一次观测到的网关就绪状态（用于 true→false / false→true 补发
    /// link_state 通知，B11）。
    last_ready: bool,
    /// 当前链路描述（link_state 通知的 desc）。
    desc: String,
}

// ---------------------------------------------------------------------------
// 在途账本 / 后台任务
// ---------------------------------------------------------------------------

/// serve 侧在途命令账本项（与 ControlPlane 的诊断视图互补：这里额外记录
/// 是否 async、是否已被取消）。
#[derive(Debug, Clone)]
struct LedgerEntry {
    worker_id: String,
    name: String,
    kind: CommandKind,
    started: Instant,
    async_mode: bool,
    cancelled: bool,
}

/// serve 侧在途账本（dispatcher 与后台线程共享）。
///
/// 晚到终态统一由 [`crate::control::ControlPlane::diagnostics`] 提供
/// （环形 + 计数），serve 只维护“谁还在途、是否 async / 已取消”。
#[derive(Debug, Default)]
struct ServeLedger {
    inflight: BTreeMap<u64, LedgerEntry>,
}

impl ServeLedger {
    /// 登记一条已提交命令（cmd_id == 0 表示网关未知 id，不登记）。
    fn begin(
        &mut self,
        cmd_id: u64,
        worker_id: &str,
        name: &str,
        kind: CommandKind,
        async_mode: bool,
    ) {
        if cmd_id == 0 {
            return;
        }
        self.inflight.insert(
            cmd_id,
            LedgerEntry {
                worker_id: String::from(worker_id),
                name: String::from(name),
                kind,
                started: Instant::now(),
                async_mode,
                cancelled: false,
            },
        );
    }

    /// 命令结束（无论成功/失败）：返回账本项。
    fn finish(&mut self, cmd_id: u64) -> Option<LedgerEntry> {
        self.inflight.remove(&cmd_id)
    }

    /// 标记取消；返回该 cmd 是否仍在账本中。
    fn mark_cancelled(&mut self, cmd_id: u64) -> bool {
        match self.inflight.get_mut(&cmd_id) {
            Some(entry) => {
                entry.cancelled = true;
                true
            }
            None => false,
        }
    }

    /// 在途账本转 JSON 行（cmd_id / worker / name / elapsed）。
    fn commands_json(&self) -> Vec<Json> {
        self.inflight
            .iter()
            .take(MAX_LISTED_INFLIGHT)
            .map(|(cmd_id, entry)| {
                Json::object(vec![
                    ("cmd_id", Json::from(*cmd_id)),
                    ("worker_id", Json::string(entry.worker_id.as_str())),
                    ("name", Json::string(entry.name.as_str())),
                    ("kind", Json::string(entry.kind.as_str())),
                    (
                        "elapsed_ms",
                        Json::from(entry.started.elapsed().as_millis() as u64),
                    ),
                    ("async", Json::Bool(entry.async_mode)),
                    ("cancelled", Json::Bool(entry.cancelled)),
                ])
            })
            .collect()
    }
}

/// dispatcher 拆出的后台任务：在工作线程执行，返回要回写的 JSONL 行。
pub struct Deferred {
    id: i64,
    label: String,
    run: Box<dyn FnOnce() -> Vec<Json> + Send + 'static>,
}

impl Deferred {
    /// 构造后台任务（`id` 为对应请求 id，线程启动失败时用于回错）。
    fn new(
        id: i64,
        label: impl Into<String>,
        run: Box<dyn FnOnce() -> Vec<Json> + Send + 'static>,
    ) -> Self {
        Self {
            id,
            label: label.into(),
            run,
        }
    }

    /// 对应请求 id。
    pub fn id(&self) -> i64 {
        self.id
    }

    /// 任务标签（诊断用）。
    pub fn label(&self) -> &str {
        &self.label
    }

    /// 执行并返回输出行（只允许调用一次）。
    pub fn run(self) -> Vec<Json> {
        (self.run)()
    }
}

/// 后台任务计数：EOF / shutdown 后等待收尾，保证 JSONL 行都写完再退出
/// （生产 shutdown 直接 `process::exit`，本计数只服务于测试与优雅退出）。
#[derive(Default)]
struct JobTracker {
    count: Mutex<usize>,
    done: std::sync::Condvar,
}

impl JobTracker {
    fn begin(&self) {
        let mut count = self
            .count
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *count = count.saturating_add(1);
    }

    fn end(&self) {
        let mut count = self
            .count
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *count = count.saturating_sub(1);
        self.done.notify_all();
    }

    fn wait_idle(&self, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        let mut count = self
            .count
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        while *count > 0 {
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            let (guard, _) = self
                .done
                .wait_timeout(count, deadline - now)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            count = guard;
        }
    }
}

/// 单步处理结果：（即时输出, 是否退出, 可选后台任务）。
type Step = (Vec<Json>, bool, Option<Deferred>);

/// 即时输出的 [`Step`]。
fn immediate(messages: Vec<Json>) -> Step {
    (messages, false, None)
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
    /// prompt 通道共享核心（serve `prompt` 方法与 ingress 共用；
    /// None 时首次 prompt 请求按 config 惰性构造）。
    prompt_hub: Option<Arc<PromptCore<LlmClient>>>,
    /// 在途账本 / 本地晚到终态（dispatcher 与后台任务线程共享）。
    ledger: Arc<Mutex<ServeLedger>>,
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
            prompt_hub: None,
            ledger: Arc::new(Mutex::new(ServeLedger::default())),
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
        self.sync_prompt_hub();
    }

    /// 注入 prompt 通道共享核心（测试 / 嵌入场景；覆盖惰性构造），
    /// 注入时同步当前网关与策略。
    pub fn set_prompt_hub(&mut self, hub: Arc<PromptCore<LlmClient>>) {
        self.prompt_hub = Some(hub);
        self.sync_prompt_hub();
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
    /// 同步语义：拆出的后台任务在内联执行完才返回（测试 / 嵌入场景用）。
    /// 运行层用 [`ServeState::step`] 把后台任务交给工作线程，保持可响应。
    ///
    /// 任何畸形输入只影响该行（跳行回错），不会 panic，进程继续。
    pub fn handle_line(&mut self, line: &str) -> (Vec<Json>, bool) {
        let (mut messages, exit, deferred) = self.step(line);
        if let Some(deferred) = deferred {
            messages.extend(deferred.run());
        }
        (messages, exit)
    }

    /// 单步处理一行：即时输出 + 是否退出 + 可选后台任务（B3）。
    ///
    /// dispatcher 拿到 [`Deferred`] 后交给工作线程执行，结果经共享写队列
    /// 按 id 回写；dispatcher 自身永不阻塞在长命令 / LLM 调用上。
    pub fn step(&mut self, line: &str) -> Step {
        let (id, method, params) = match parse_request(line) {
            Ok(parsed) => parsed,
            Err(messages) => return immediate(messages),
        };
        // shutdown：先回响应再退出（退出动作由运行层执行）。
        if method == "shutdown" {
            return (vec![ok_response(id, Json::Null)], true, None);
        }
        self.dispatch(id, &method, &params)
    }
    /// 方法分发（参数已通过结构校验）。
    fn dispatch(&mut self, id: i64, method: &str, params: &Json) -> Step {
        match method {
            "link" => immediate(self.method_link(id, params)),
            "link_off" => immediate(self.method_link_off(id)),
            "workers" => immediate(vec![self.method_workers(id)]),
            "tool_call" => self.method_command(id, params, true),
            "task" => self.method_command(id, params, false),
            "cancel" => immediate(self.method_cancel(id, params)),
            "commands" => immediate(vec![ok_response(id, self.method_commands())]),
            "jump" => self.method_jump(id, params),
            "admin" => self.method_admin(id, params),
            "policy.get" => immediate(vec![ok_response(id, self.policy_json())]),
            "policy.set" => immediate(self.method_policy_set(id, params)),
            "status" => immediate(vec![ok_response(id, self.status_json())]),
            // v1.1 additive：prompt 方法（见模块注释与 prompt_server）。
            "prompt" => self.method_prompt(id, params),
            other => immediate(vec![error_response(
                id,
                CODE_UNKNOWN_METHOD,
                &format!("未知方法 {other}"),
            )]),
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
                // 让新建链路对 serve `prompt` 方法与 ingress 立即可见
                // （并按新链路重算未显式指定的步数上限）。
                self.sync_prompt_hub();
                vec![
                    ok_response(
                        id,
                        Json::object(vec![
                            ("mode", Json::string("tcp")),
                            ("desc", Json::string(desc.as_str())),
                            // v1.2 additive：插件读 listen_addr（B11）。
                            ("listen_addr", Json::string(actual.as_str())),
                            // 兼容旧字段。
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
                self.sync_prompt_hub();
                vec![
                    ok_response(
                        id,
                        Json::object(vec![
                            ("mode", Json::string("stdio")),
                            ("desc", Json::string(desc.as_str())),
                            ("worker_bin", Json::string(worker_bin.as_str())),
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
                self.sync_prompt_hub();
                vec![
                    ok_response(
                        id,
                        Json::object(vec![
                            ("mode", Json::string("mqtt")),
                            ("desc", Json::string(desc.as_str())),
                            // v1.2 additive：插件读 mqtt_addr（B11）。
                            ("mqtt_addr", Json::string(display.as_str())),
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
            self.sync_prompt_hub();
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
    ///
    /// v1.2 additive：
    /// - `async:true`：dispatcher 内只做提交，立刻回
    ///   `{accepted:true,cmd_id,worker_id,kind,name}`；完成时工作线程发无 id 的
    ///   `{"method":"command_result","params":{...}}` 通知；
    /// - 默认（同步）路径也拆到后台线程执行，dispatcher 不阻塞。
    ///
    /// D5：名字在 Worker `tasks` 列表（或 pc.ask/pc.job）时按 Task 下发；
    /// 策略分类仍按名字（见 `tools::check_policy_for_worker`）。
    fn method_command(&mut self, id: i64, params: &Json, is_tool: bool) -> Step {
        let async_mode = match parse_async_flag(params) {
            Ok(value) => value,
            Err(message) => {
                return immediate(vec![error_response(
                    id,
                    tools::CODE_INVALID_ARGUMENTS,
                    &message,
                )]);
            }
        };
        let filtered = without_async_flag(params);
        let function = if is_tool {
            tools::FN_WORKER_TOOL
        } else {
            tools::FN_WORKER_TASK
        };
        let parsed = match tools::parse_call(function, &filtered.to_compact_string()) {
            Ok(parsed) => parsed,
            Err(error) => {
                return immediate(vec![error_response(
                    id,
                    tools::CODE_INVALID_ARGUMENTS,
                    &error.to_string(),
                )]);
            }
        };
        let worker_id = String::from(parsed.worker_id().unwrap_or_default());
        let name = String::from(parsed.command_name().unwrap_or_default());
        let requested_kind = if is_tool {
            CommandKind::Tool
        } else {
            CommandKind::Task
        };
        let Some(mut gateway) = self.gateway.clone() else {
            return immediate(vec![error_response(
                id,
                CODE_LINK_ERROR,
                "未连接：先 link tcp|stdio|mqtt",
            )]);
        };
        // D5：先看 Worker 能力快照（策略分类与线类型都要用 tasks 列表）。
        let worker = gateway
            .list_workers()
            .into_iter()
            .find(|worker| worker.worker_id == worker_id);
        if let Err(denied) = tools::check_policy_for_worker(&self.policy, &parsed, worker.as_ref())
        {
            return immediate(vec![ok_response(
                id,
                tools::failure_result_json(
                    &worker_id,
                    Some(requested_kind),
                    &name,
                    tools::CODE_POLICY_DENIED,
                    &denied.to_string(),
                    false,
                ),
            )]);
        }
        let kind = tools::route_command_kind(worker.as_ref(), &name, requested_kind);
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
        if async_mode {
            return self.spawn_async_command(id, gateway, request, worker_id, name, kind);
        }
        let job_ledger = Arc::clone(&self.ledger);
        let worker_id_job = worker_id.clone();
        let name_job = name.clone();
        let deferred = Deferred::new(
            id,
            "command",
            Box::new(move || match gateway.submit(request) {
                Ok(receipt) => {
                    let cmd_id = receipt.cmd_id();
                    begin_ledger(&job_ledger, cmd_id, &worker_id_job, &name_job, kind, false);
                    let outcome = receipt.wait();
                    let cancelled = finish_ledger(&job_ledger, cmd_id)
                        .map(|entry| entry.cancelled)
                        .unwrap_or(false);
                    match outcome {
                        Ok(result) => vec![ok_response(
                            id,
                            command_result_json_with_cancel(&result, cancelled),
                        )],
                        Err(error) => vec![ok_response(
                            id,
                            tools::failure_result_json(
                                &worker_id_job,
                                Some(kind),
                                &name_job,
                                &error.code,
                                &error.message,
                                retryable_gateway_code(&error.code),
                            ),
                        )],
                    }
                }
                Err(error) => vec![ok_response(
                    id,
                    tools::failure_result_json(
                        &worker_id_job,
                        Some(kind),
                        &name_job,
                        &error.code,
                        &error.message,
                        retryable_gateway_code(&error.code),
                    ),
                )],
            }),
        );
        (Vec::new(), false, Some(deferred))
    }

    /// 异步命令：dispatcher 内完成提交并立刻回 accepted；完成时后台线程
    /// 发无 id 的 `command_result` 通知。
    fn spawn_async_command(
        &mut self,
        id: i64,
        mut gateway: GatewayHandle,
        request: CommandRequest,
        worker_id: String,
        name: String,
        kind: CommandKind,
    ) -> Step {
        match gateway.submit(request) {
            Ok(receipt) => {
                let cmd_id = receipt.cmd_id();
                begin_ledger(&self.ledger, cmd_id, &worker_id, &name, kind, true);
                let ledger = Arc::clone(&self.ledger);
                let worker_id_job = worker_id.clone();
                let name_job = name.clone();
                let deferred = Deferred::new(
                    id,
                    "async-command",
                    Box::new(move || {
                        let outcome = receipt.wait();
                        let cancelled = finish_ledger(&ledger, cmd_id)
                            .map(|entry| entry.cancelled)
                            .unwrap_or(false);
                        match outcome {
                            Ok(result) => {
                                vec![command_result_notification(&result, cancelled)]
                            }
                            Err(error) => vec![command_error_notification(
                                &worker_id_job,
                                Some(kind),
                                &name_job,
                                &error.code,
                                &error.message,
                                retryable_gateway_code(&error.code),
                                cancelled,
                            )],
                        }
                    }),
                );
                immediate_with_deferred(
                    vec![ok_response(
                        id,
                        Json::object(vec![
                            ("accepted", Json::Bool(true)),
                            ("cmd_id", Json::from(cmd_id)),
                            ("worker_id", Json::string(worker_id.as_str())),
                            ("kind", Json::string(kind.as_str())),
                            ("name", Json::string(name.as_str())),
                            ("async", Json::Bool(true)),
                        ]),
                    )],
                    deferred,
                )
            }
            Err(error) => immediate(vec![ok_response(
                id,
                tools::failure_result_json(
                    &worker_id,
                    Some(kind),
                    &name,
                    &error.code,
                    &error.message,
                    retryable_gateway_code(&error.code),
                ),
            )]),
        }
    }

    // ---- cancel / commands（B2）----

    /// cancel 方法：`{cmd_id}` → 网关取消（内部映射 Worker 并下发 sekai.cancel）。
    fn method_cancel(&mut self, id: i64, params: &Json) -> Vec<Json> {
        if let Err(error) = tools::check_fields(params, &["cmd_id"]) {
            return vec![error_response(
                id,
                tools::CODE_INVALID_ARGUMENTS,
                &error.to_string(),
            )];
        }
        let cmd_id = match params.get("cmd_id").and_then(Json::as_u64) {
            Some(cmd_id) if cmd_id > 0 => cmd_id,
            _ => {
                return vec![error_response(
                    id,
                    tools::CODE_INVALID_ARGUMENTS,
                    "参数 cmd_id 必须是正整数",
                )];
            }
        };
        self.cancel_quiet(id, cmd_id)
    }

    /// 取消实现（cancel 方法与 admin op=cancel 共用）。
    fn cancel_quiet(&mut self, id: i64, cmd_id: u64) -> Vec<Json> {
        let Some(mut gateway) = self.gateway.clone() else {
            return vec![error_response(
                id,
                CODE_LINK_ERROR,
                "未连接：先 link tcp|stdio|mqtt",
            )];
        };
        let known = self
            .ledger
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .mark_cancelled(cmd_id);
        match gateway.cancel(cmd_id) {
            Ok(()) => vec![ok_response(
                id,
                Json::object(vec![
                    ("cmd_id", Json::from(cmd_id)),
                    ("cancelled", Json::Bool(true)),
                    ("known", Json::Bool(known)),
                ]),
            )],
            Err(error) => vec![error_response(id, &error.code, &error.message)],
        }
    }

    /// commands 方法：列出在途账本（cmd_id / worker / name / elapsed）。
    ///
    /// 合并网关侧诊断（含自动对时等内部命令）与 serve 自身账本（异步标记
    /// 更准）；同一 cmd_id 以 serve 账本为准。响应为标准信封
    /// `{id,ok,result:{count,commands}}`（与 status/policy.get 一致）。
    fn method_commands(&mut self) -> Json {
        let diagnostics = self.gateway_diagnostics();
        let ledger = self
            .ledger
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .commands_json();
        let ledger_ids: Vec<u64> = ledger
            .iter()
            .filter_map(|item| item.get("cmd_id").and_then(Json::as_u64))
            .collect();
        let mut commands: Vec<Json> = ledger;
        for command in diagnostics.inflight.iter() {
            if ledger_ids.contains(&command.cmd_id) {
                continue;
            }
            commands.push(Json::object(vec![
                ("cmd_id", Json::from(command.cmd_id)),
                ("worker_id", Json::string(command.worker_id.as_str())),
                ("name", Json::string(command.name.as_str())),
                ("elapsed_ms", Json::from(command.elapsed_ms)),
                ("async", Json::Bool(false)),
                ("cancelled", Json::Bool(false)),
            ]));
        }
        Json::object(vec![
            ("count", Json::from(commands.len())),
            ("commands", Json::Array(commands)),
        ])
    }

    // ---- jump / admin（B6/B7，均受 --allow-admin）----

    /// jump 方法：`{worker_id,target}` → `CommandKind::Jump`（后台等待最终事件）。
    fn method_jump(&mut self, id: i64, params: &Json) -> Step {
        if let Err(message) = self.require_admin() {
            return immediate(vec![error_response(
                id,
                tools::CODE_POLICY_DENIED,
                &message,
            )]);
        }
        if let Err(error) = tools::check_fields(params, &["worker_id", "target"]) {
            return immediate(vec![error_response(
                id,
                tools::CODE_INVALID_ARGUMENTS,
                &error.to_string(),
            )]);
        }
        let worker_id = match params.get("worker_id").and_then(Json::as_str) {
            Some(worker_id) if !worker_id.is_empty() => String::from(worker_id),
            _ => {
                return immediate(vec![error_response(
                    id,
                    tools::CODE_INVALID_ARGUMENTS,
                    "缺少必填参数 worker_id（字符串）",
                )]);
            }
        };
        let target = match params.get("target").and_then(Json::as_str) {
            Some(target) if !target.is_empty() => String::from(target),
            _ => {
                return immediate(vec![error_response(
                    id,
                    tools::CODE_INVALID_ARGUMENTS,
                    "缺少必填参数 target（字符串）",
                )]);
            }
        };
        let Some(mut gateway) = self.gateway.clone() else {
            return immediate(vec![error_response(
                id,
                CODE_LINK_ERROR,
                "未连接：先 link tcp|stdio|mqtt",
            )]);
        };
        let ledger = Arc::clone(&self.ledger);
        let worker_id_job = worker_id.clone();
        let target_job = target.clone();
        let deferred = Deferred::new(
            id,
            "jump",
            Box::new(move || {
                let request = CommandRequest::jump(worker_id_job.as_str(), target_job.as_str());
                match gateway.submit(request) {
                    Ok(receipt) => {
                        let cmd_id = receipt.cmd_id();
                        begin_ledger(
                            &ledger,
                            cmd_id,
                            &worker_id_job,
                            &target_job,
                            CommandKind::Jump,
                            false,
                        );
                        let outcome = receipt.wait();
                        finish_ledger(&ledger, cmd_id);
                        match outcome {
                            Ok(result) => {
                                vec![ok_response(id, tools::command_result_json(&result))]
                            }
                            Err(error) => vec![ok_response(
                                id,
                                tools::failure_result_json(
                                    &worker_id_job,
                                    Some(CommandKind::Jump),
                                    &target_job,
                                    &error.code,
                                    &error.message,
                                    retryable_gateway_code(&error.code),
                                ),
                            )],
                        }
                    }
                    Err(error) => vec![ok_response(
                        id,
                        tools::failure_result_json(
                            &worker_id_job,
                            Some(CommandKind::Jump),
                            &target_job,
                            &error.code,
                            &error.message,
                            retryable_gateway_code(&error.code),
                        ),
                    )],
                }
            }),
        );
        (Vec::new(), false, Some(deferred))
    }

    /// admin 方法（受 `--allow-admin`）：`set_time` / `cancel` / `workers.set`。
    ///
    /// - `{op:"set_time", worker_id, unix_ms?}` → sekai.set_time（8 字节大端）；
    /// - `{op:"cancel", cmd_id}` → 同 cancel 方法；
    /// - `{op:"workers.set", worker_id, workers:[{name,ip,port}...]}` →
    ///   JSON 转 DeviceList 二进制后下发 sekai.workers.set。
    ///
    /// 走 `submit_control`（绕过在途上限；自动对时不经过这里、也不受
    /// allow_admin 限制）。
    fn method_admin(&mut self, id: i64, params: &Json) -> Step {
        if let Err(message) = self.require_admin() {
            return immediate(vec![error_response(
                id,
                tools::CODE_POLICY_DENIED,
                &message,
            )]);
        }
        if let Err(error) =
            tools::check_fields(params, &["op", "worker_id", "unix_ms", "cmd_id", "workers"])
        {
            return immediate(vec![error_response(
                id,
                tools::CODE_INVALID_ARGUMENTS,
                &error.to_string(),
            )]);
        }
        let op = match params.get("op").and_then(Json::as_str) {
            Some(op) => String::from(op),
            None => {
                return immediate(vec![error_response(
                    id,
                    tools::CODE_INVALID_ARGUMENTS,
                    "缺少必填参数 op（set_time/cancel/workers.set）",
                )]);
            }
        };
        match op.as_str() {
            "cancel" => {
                let cmd_id = match params.get("cmd_id").and_then(Json::as_u64) {
                    Some(cmd_id) if cmd_id > 0 => cmd_id,
                    _ => {
                        return immediate(vec![error_response(
                            id,
                            tools::CODE_INVALID_ARGUMENTS,
                            "缺少必填参数 cmd_id（正整数）",
                        )]);
                    }
                };
                return immediate(self.cancel_quiet(id, cmd_id));
            }
            "set_time" => {
                let worker_id = match params.get("worker_id").and_then(Json::as_str) {
                    Some(worker_id) if !worker_id.is_empty() => String::from(worker_id),
                    _ => {
                        return immediate(vec![error_response(
                            id,
                            tools::CODE_INVALID_ARGUMENTS,
                            "缺少必填参数 worker_id（字符串）",
                        )]);
                    }
                };
                let unix_ms = match params.get("unix_ms") {
                    None | Some(Json::Null) => now_unix_ms(),
                    Some(value) => match value.as_u64() {
                        Some(value) => value,
                        None => {
                            return immediate(vec![error_response(
                                id,
                                tools::CODE_INVALID_ARGUMENTS,
                                "参数 unix_ms 必须是非负整数",
                            )]);
                        }
                    },
                };
                let payload = unix_ms.to_be_bytes().to_vec();
                self.spawn_control(id, worker_id, "sekai.set_time", payload, op)
            }
            "workers.set" => {
                let worker_id = match params.get("worker_id").and_then(Json::as_str) {
                    Some(worker_id) if !worker_id.is_empty() => String::from(worker_id),
                    _ => {
                        return immediate(vec![error_response(
                            id,
                            tools::CODE_INVALID_ARGUMENTS,
                            "缺少必填参数 worker_id（字符串）",
                        )]);
                    }
                };
                let list = match parse_device_list(params.get("workers")) {
                    Ok(list) => list,
                    Err(message) => {
                        return immediate(vec![error_response(
                            id,
                            tools::CODE_INVALID_ARGUMENTS,
                            &message,
                        )]);
                    }
                };
                let encoded = match list.encode() {
                    Ok(encoded) => encoded,
                    Err(error) => {
                        return immediate(vec![error_response(
                            id,
                            tools::CODE_INVALID_ARGUMENTS,
                            &format!("workers 编码失败：{error}"),
                        )]);
                    }
                };
                self.spawn_control(id, worker_id, "sekai.workers.set", encoded, op)
            }
            other => immediate(vec![error_response(
                id,
                tools::CODE_INVALID_ARGUMENTS,
                &format!("admin op 不支持 {other}（可选 set_time/cancel/workers.set）"),
            )]),
        }
    }

    /// 后台执行一条管理控制命令（`submit_control` + 等待结果）。
    fn spawn_control(
        &mut self,
        id: i64,
        worker_id: String,
        name: &'static str,
        payload: Vec<u8>,
        op: String,
    ) -> Step {
        let Some(mut gateway) = self.gateway.clone() else {
            return immediate(vec![error_response(
                id,
                CODE_LINK_ERROR,
                "未连接：先 link tcp|stdio|mqtt",
            )]);
        };
        let ledger = Arc::clone(&self.ledger);
        let deferred = Deferred::new(
            id,
            "admin",
            Box::new(move || {
                let request = CommandRequest {
                    worker_id: worker_id.clone(),
                    kind: CommandKind::Tool,
                    name: String::from(name),
                    args: payload,
                    ddl_ms: 0,
                    timeout_ms: None,
                };
                match gateway.submit_control(request) {
                    Ok(receipt) => {
                        let cmd_id = receipt.cmd_id();
                        begin_ledger(&ledger, cmd_id, &worker_id, name, CommandKind::Tool, false);
                        let outcome = receipt.wait();
                        finish_ledger(&ledger, cmd_id);
                        match outcome {
                            Ok(result) => vec![ok_response(
                                id,
                                Json::object(vec![
                                    ("op", Json::string(op.as_str())),
                                    ("cmd_id", Json::from(cmd_id)),
                                    ("result", tools::command_result_json(&result)),
                                ]),
                            )],
                            Err(error) => vec![ok_response(
                                id,
                                tools::failure_result_json(
                                    &worker_id,
                                    Some(CommandKind::Tool),
                                    name,
                                    &error.code,
                                    &error.message,
                                    retryable_gateway_code(&error.code),
                                ),
                            )],
                        }
                    }
                    Err(error) => vec![ok_response(
                        id,
                        tools::failure_result_json(
                            &worker_id,
                            Some(CommandKind::Tool),
                            name,
                            &error.code,
                            &error.message,
                            retryable_gateway_code(&error.code),
                        ),
                    )],
                }
            }),
        );
        (Vec::new(), false, Some(deferred))
    }

    /// 管理命令前置校验：`--allow-admin` 未开启时返回策略拒绝消息。
    fn require_admin(&self) -> Result<(), String> {
        if self.policy.allow_admin {
            Ok(())
        } else {
            Err(String::from(
                "管理命令未授权：启动时加 --allow-admin（或 policy.set allow_admin=true）",
            ))
        }
    }

    /// 网关诊断快照（无网关时返回默认值）。
    fn gateway_diagnostics(&mut self) -> GatewayDiagnostics {
        match self.gateway.clone() {
            Some(mut gateway) => gateway.diagnostics(),
            None => GatewayDiagnostics::default(),
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
        self.sync_prompt_hub();
        vec![ok_response(id, self.policy_json())]
    }

    // ---- prompt（v1.1 additive）----

    /// prompt 方法：参数解析与执行复用 [`crate::prompt_server`]（与 TCP
    /// ingress 同一核心）；hub 惰性构造，无 API key → `llm_unavailable`。
    ///
    /// LLM 调用可能持续数秒：拆到后台线程执行（B3），dispatcher 不阻塞。
    fn method_prompt(&mut self, id: i64, params: &Json) -> Step {
        if self.prompt_hub.is_none() {
            let hub = build_prompt_hub(&self.config, self.policy.clone());
            self.prompt_hub = Some(Arc::clone(&hub));
            // 首次构造后同步当前网关，使先前的 link/注入对 prompt 可见。
            if let Some(gateway) = self.gateway.clone() {
                hub.set_gateway(Some(gateway));
            }
        }
        let hub = self
            .prompt_hub
            .as_ref()
            .expect("上面已保证 prompt_hub 存在");
        let hub = Arc::clone(hub);
        let params = params.clone();
        let deferred = Deferred::new(
            id,
            "prompt",
            Box::new(
                move || match prompt_server::dispatch_prompt(&hub, &params) {
                    Ok(result) => vec![ok_response(id, result)],
                    Err(error) => vec![error_response(id, &error.code, &error.message)],
                },
            ),
        );
        (Vec::new(), false, Some(deferred))
    }

    // ---- status ----

    /// status 方法：链路描述 / 模式 / 就绪 / Worker 数 / 策略摘要 / 版本 / pid。
    ///
    /// v1.2 additive（B8）：`link_info{state,workers,last_error}`、
    /// `inflight[]`、`counters{late_results,dropped_events}`、
    /// `clock_synced`、`late_results[]`。旧 `link` 字符串字段保留。
    fn status_json(&mut self) -> Json {
        let (ready, worker_count) = match self.gateway.clone() {
            Some(mut gateway) => (gateway.is_ready(), gateway.list_workers().len()),
            None => (false, 0),
        };
        let diagnostics = self.gateway_diagnostics();
        let ledger = self
            .ledger
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut inflight: Vec<Json> = ledger.commands_json();
        let known: Vec<u64> = inflight
            .iter()
            .filter_map(|item| item.get("cmd_id").and_then(Json::as_u64))
            .collect();
        for command in diagnostics.inflight.iter().take(MAX_LISTED_INFLIGHT) {
            if known.contains(&command.cmd_id) {
                continue;
            }
            inflight.push(inflight_json(command));
        }
        // 晚到终态统一来自控制面诊断（serve 不再本地累积，B8）。
        let late: Vec<Json> = diagnostics
            .late_results
            .iter()
            .map(late_result_json)
            .collect();
        let late_count = diagnostics.counters.late_results;
        drop(ledger);
        let state = if self.gateway.is_none() {
            "off"
        } else if ready {
            "ready"
        } else {
            "down"
        };
        Json::object(vec![
            ("link", Json::string(self.link_desc.as_str())),
            ("link_desc", Json::string(self.link_desc.as_str())),
            (
                "link_info",
                Json::object(vec![
                    ("state", Json::string(state)),
                    ("workers", Json::from(worker_count)),
                    (
                        "last_error",
                        match &diagnostics.last_error {
                            Some(error) => Json::string(error.as_str()),
                            None => Json::Null,
                        },
                    ),
                ]),
            ),
            ("mode", Json::string(self.link_mode)),
            ("ready", Json::Bool(ready)),
            ("worker_count", Json::from(worker_count)),
            ("inflight", Json::Array(inflight)),
            (
                "counters",
                Json::object(vec![
                    ("late_results", Json::from(late_count)),
                    (
                        "dropped_events",
                        Json::from(diagnostics.counters.dropped_events),
                    ),
                ]),
            ),
            ("clock_synced", Json::Bool(diagnostics.clock_synced)),
            ("late_results", Json::Array(late)),
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
        view.desc = self.link_desc.clone();
        view.last_ready = match self.gateway.clone() {
            Some(gateway) => gateway.is_ready(),
            None => false,
        };
        view.baseline = match self.gateway.clone() {
            Some(mut gateway) => gateway
                .list_workers()
                .into_iter()
                .map(|worker| worker.worker_id)
                .collect(),
            None => Vec::new(),
        };
    }

    /// 把当前网关与策略同步进 prompt 共享核心（建链 / 关链 / 注入 /
    /// policy.set 后；hub 未构造时跳过，首次 prompt 请求构造时会重新快照）。
    fn sync_prompt_hub(&mut self) {
        if let Some(hub) = &self.prompt_hub {
            hub.set_gateway(self.gateway.clone());
            hub.set_policy(self.policy.clone());
            // 步数上限未显式指定时按当前链路重算：直连 PC Worker（tcp/stdio）
            // 不设上限、MQTT 保留默认 12；off 时保持配置值（prompt 本身会
            // 因 not_linked 失败）。
            if self.config.max_steps_auto
                && let Some(link) = config::LinkMode::parse(self.link_mode)
            {
                hub.set_max_steps(config::default_max_steps_for(link));
            }
        }
    }
}

/// 按 [`ServeConfig`] 构造 prompt 通道共享核心。
///
/// chat 客户端惰性构造（首次 prompt 请求才建）；无 API key 时构造器
/// 返回 None → `llm_unavailable`。
fn build_prompt_hub(config: &ServeConfig, policy: Policy) -> Arc<PromptCore<LlmClient>> {
    let api_key = config.api_key.clone().unwrap_or_default();
    let builder: Box<dyn Fn() -> Option<LlmClient> + Send + Sync> = if api_key.trim().is_empty() {
        Box::new(|| None::<LlmClient>)
    } else {
        let base_url = config.base_url.clone();
        let model = config.model.clone();
        let max_tokens = config.max_tokens;
        let temperature = config.temperature;
        Box::new(move || {
            let llm_config = LlmConfig {
                base_url: base_url.clone(),
                model: model.clone(),
                api_key: api_key.clone(),
                max_tokens,
                temperature,
                reasoning_effort: None,
                timeout: Duration::from_secs(llm::DEFAULT_TIMEOUT_SECS),
                retry_base_ms: llm::DEFAULT_RETRY_BASE_MS,
                max_retries: llm::DEFAULT_MAX_RETRIES,
            };
            Some(LlmClient::new(Box::new(StdHttpClient::new()), llm_config))
        })
    };
    let agent_config = AgentConfig {
        model: config.model.clone(),
        max_steps: config.max_steps,
        ddl_ms: config.default_ddl_ms,
        timeout_ms: config.tool_timeout_ms,
        temperature: config.temperature,
        max_tokens: config.max_tokens,
    };
    let trace = match TraceWriter::new(config.trace_path.as_deref()) {
        Ok(trace) => trace,
        Err(error) => {
            eprintln!("sekai-router: serve trace 打开失败，改为禁用：{error}");
            TraceWriter::disabled()
        }
    };
    Arc::new(PromptCore::lazy(builder, policy, agent_config, trace))
}

// ---------------------------------------------------------------------------
// 请求解析 / 后台任务 / 诊断辅助
// ---------------------------------------------------------------------------

/// 解析一行 JSONL 请求；失败时返回要回写的错误消息（跳行回错）。
fn parse_request(line: &str) -> Result<(i64, String, Json), Vec<Json>> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return Err(Vec::new());
    }
    let value = match Json::parse(trimmed) {
        Ok(value) => value,
        Err(error) => {
            // 畸形 JSON 取不到 id：以通知形式回 invalid_json
            // （JsonError 的 Display 自带「JSON 解析失败（偏移 N）：」前缀）。
            return Err(vec![invalid_json_notification(&error.to_string())]);
        }
    };
    if value.as_object().is_none() {
        return Err(vec![invalid_json_notification("请求必须是 JSON 对象")]);
    }
    let id = match value.get("id") {
        None => return Err(vec![invalid_json_notification("请求缺少 id")]),
        Some(id) => match id.as_i64() {
            Some(id) => id,
            None => return Err(vec![invalid_json_notification("请求 id 必须是整数")]),
        },
    };
    let method = match value.get("method") {
        Some(Json::String(method)) => method.clone(),
        None => {
            return Err(vec![error_response(
                id,
                tools::CODE_INVALID_ARGUMENTS,
                "缺少必填参数 method",
            )]);
        }
        Some(_) => {
            return Err(vec![error_response(
                id,
                tools::CODE_INVALID_ARGUMENTS,
                "参数 method 必须是字符串",
            )]);
        }
    };
    let params = match value.get("params") {
        None => Json::object(Vec::new()),
        Some(value) if value.as_object().is_some() => value.clone(),
        Some(_) => {
            return Err(vec![error_response(
                id,
                tools::CODE_INVALID_ARGUMENTS,
                "参数 params 必须是 JSON 对象",
            )]);
        }
    };
    Ok((id, method, params))
}

/// `params.async` 解析（非布尔 → 中文错误）。
fn parse_async_flag(params: &Json) -> Result<bool, String> {
    match params.get("async") {
        None | Some(Json::Null) => Ok(false),
        Some(Json::Bool(value)) => Ok(*value),
        Some(_) => Err(String::from("参数 async 必须是布尔值")),
    }
}

/// 去掉 `async` 字段后的参数对象（其余字段交给 tools::parse_call 严格校验）。
fn without_async_flag(params: &Json) -> Json {
    let Some(entries) = params.as_object() else {
        return params.clone();
    };
    let mut filtered = Json::object(Vec::new());
    for (key, value) in entries {
        if key != "async" {
            filtered.set(key.clone(), value.clone());
        }
    }
    filtered
}

/// 即时输出 + 后台任务的 [`Step`]。
fn immediate_with_deferred(messages: Vec<Json>, deferred: Deferred) -> Step {
    (messages, false, Some(deferred))
}

/// 登记在途命令（忽略 0 等未知 id）。
fn begin_ledger(
    ledger: &Arc<Mutex<ServeLedger>>,
    cmd_id: u64,
    worker_id: &str,
    name: &str,
    kind: CommandKind,
    async_mode: bool,
) {
    ledger
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .begin(cmd_id, worker_id, name, kind, async_mode);
}

/// 结束在途命令（返回账本项，用于读取 cancelled 标记）。
fn finish_ledger(ledger: &Arc<Mutex<ServeLedger>>, cmd_id: u64) -> Option<LedgerEntry> {
    ledger
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .finish(cmd_id)
}

/// 成功/失败结果 JSON + `cancelled` 标记（取消后到达的终态仍如实回报）。
fn command_result_json_with_cancel(
    result: &crate::control::CommandResult,
    cancelled: bool,
) -> Json {
    let mut value = tools::command_result_json(result);
    if cancelled {
        value.set("cancelled", Json::Bool(true));
    }
    value
}

/// 无 id 的异步完成通知：`{"method":"command_result","params":{...}}`。
fn command_result_notification(result: &crate::control::CommandResult, cancelled: bool) -> Json {
    Json::object(vec![
        ("method", Json::string("command_result")),
        ("params", command_result_json_with_cancel(result, cancelled)),
    ])
}

/// 无 id 的异步失败通知（网关层错误，无 CommandResult）。
#[allow(clippy::too_many_arguments)]
fn command_error_notification(
    worker_id: &str,
    kind: Option<CommandKind>,
    name: &str,
    code: &str,
    message: &str,
    retryable: bool,
    cancelled: bool,
) -> Json {
    let mut params = tools::failure_result_json(worker_id, kind, name, code, message, retryable);
    if cancelled {
        params.set("cancelled", Json::Bool(true));
    }
    Json::object(vec![
        ("method", Json::string("command_result")),
        ("params", params),
    ])
}

/// 在途命令转 JSON（`inflight[]` / `commands` 共用形状）。
fn inflight_json(command: &InflightCommand) -> Json {
    Json::object(vec![
        ("cmd_id", Json::from(command.cmd_id)),
        ("worker_id", Json::string(command.worker_id.as_str())),
        ("name", Json::string(command.name.as_str())),
        ("elapsed_ms", Json::from(command.elapsed_ms)),
        ("async", Json::Bool(false)),
        ("cancelled", Json::Bool(false)),
    ])
}

/// 晚到终态转 JSON。
fn late_result_json(item: &LateResult) -> Json {
    Json::object(vec![
        ("cmd_id", Json::from(item.cmd_id)),
        ("worker_id", Json::string(item.worker_id.as_str())),
        ("code", Json::string(item.code.as_str())),
        ("elapsed_ms", Json::from(item.elapsed_ms)),
    ])
}

/// `admin workers.set` 的 JSON 数组 → [`DeviceList`]。
fn parse_device_list(value: Option<&Json>) -> Result<DeviceList, String> {
    let Some(Json::Array(items)) = value else {
        return Err(String::from(
            "缺少必填参数 workers（数组：[{name,ip,port}]）",
        ));
    };
    let mut list = DeviceList::new();
    for (index, item) in items.iter().enumerate() {
        let name = item
            .get("name")
            .and_then(Json::as_str)
            .ok_or_else(|| format!("workers[{index}].name 必须是字符串"))?;
        let ip = item
            .get("ip")
            .and_then(Json::as_str)
            .ok_or_else(|| format!("workers[{index}].ip 必须是字符串"))?;
        let port = item
            .get("port")
            .and_then(Json::as_u64)
            .ok_or_else(|| format!("workers[{index}].port 必须是整数"))?;
        let port = u16::try_from(port).map_err(|_| format!("workers[{index}].port 超出范围"))?;
        let device = Device::new(name, ip, port)
            .map_err(|error| format!("workers[{index}] 非法：{error}"))?;
        list.insert(device)
            .map_err(|error| format!("workers[{index}] 无法加入列表：{error}"))?;
    }
    Ok(list)
}

/// 当前 Unix 毫秒（serve admin set_time 默认值）。
fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
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

    // prompt 通道共享核心：serve `prompt` 方法与 ingress 共用同一快照。
    let prompt_hub = build_prompt_hub(&config, policy.clone());
    // prompt ingress（--prompt-addr；serve 模式默认 127.0.0.1:7422，off 关闭）。
    // 监听失败（端口占用等）只打 stderr 中文错误并继续运行：JSONL 主流程
    // 不受影响（用户仍可经 serve prompt 方法使用 LLM 中继），这是本实现
    // 在「报错继续」与「非 0 退出」之间的明确取舍。
    if let Some(addr) = &config.prompt_addr {
        match prompt_server::start_ingress(Arc::clone(&prompt_hub), addr) {
            Ok(actual) => eprintln!("sekai-router: 提示通道已监听 {actual}"),
            Err(error) => {
                eprintln!("sekai-router: 提示通道监听失败（{addr}）：{error}；serve 继续运行");
            }
        }
    }

    let mut state = ServeState::new(config, policy);
    state.set_prompt_hub(prompt_hub);
    if let Some(injected) = injected {
        state.set_gateway(injected.handle, injected.mode, injected.desc);
    }
    let poll_view = state.poll_view();
    let (tx, rx) = mpsc::channel::<String>();
    let stop = Arc::new(AtomicBool::new(false));
    let jobs = Arc::new(JobTracker::default());

    // dispatcher：唯一状态所有者，串行处理请求；长方法拆成 Deferred 任务
    // 交给工作线程（B3），dispatcher 自身不阻塞。
    let dispatcher_writer = Arc::clone(&writer);
    let dispatcher_stop = Arc::clone(&stop);
    let dispatcher_jobs = Arc::clone(&jobs);
    let dispatcher = std::thread::Builder::new()
        .name(String::from("sekai-router-serve"))
        .spawn(move || {
            while let Ok(line) = rx.recv() {
                let (messages, exit, deferred) = state.step(&line);
                for message in &messages {
                    write_shared_line(&dispatcher_writer, message);
                }
                if let Some(deferred) = deferred {
                    let job_writer = Arc::clone(&dispatcher_writer);
                    let job_tracker = Arc::clone(&dispatcher_jobs);
                    let label = String::from(deferred.label());
                    let job_id = deferred.id();
                    job_tracker.begin();
                    let spawned = std::thread::Builder::new()
                        .name(format!("sekai-router-serve-{label}"))
                        .spawn(move || {
                            let messages = deferred.run();
                            for message in &messages {
                                write_shared_line(&job_writer, message);
                            }
                            job_tracker.end();
                        });
                    if let Err(error) = spawned {
                        eprintln!("sekai-router: 后台任务线程启动失败：{error}");
                        dispatcher_jobs.end();
                        write_shared_line(
                            &dispatcher_writer,
                            &error_response(
                                job_id,
                                "internal_error",
                                &format!("无法启动后台任务线程：{error}"),
                            ),
                        );
                    }
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

    // poll 线程：~200ms diff Worker 集合，发上线 / 离线通知；同时观测
    // 网关 ready 变化，true→false / false→true 时补发 link_state 通知（B11）。
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
                    let mut notifications =
                        worker_notifications(&mut view.baseline, &handle.list_workers());
                    let ready = handle.is_ready();
                    if ready != view.last_ready {
                        view.last_ready = ready;
                        notifications.push(link_state_notification(&view.desc, ready));
                    }
                    notifications
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
    // 等后台任务把结果行写完（最多 5s；shutdown 在测试里是 no-op）。
    jobs.wait_idle(Duration::from_secs(5));
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
