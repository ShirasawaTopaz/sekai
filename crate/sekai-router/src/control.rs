//! 控制平面：MQTT 客户端状态机之上的命令生命周期。
//!
//! ControlPlane 不读时钟、不做 I/O：TCP 读写与定时由 gateway/测试 harness 驱动。
//! 两个时间基准：
//!
//! - now_unix_ms：MQTT 客户端状态机（CONNECT/Keep Alive）使用；
//! - now_mono_ms：命令超时使用（单调、不受校时影响）。
//!
//! 关键不变量（协议 §8）：
//! - 只有 Completed/Failed 是最终事件；
//! - 每Worker同一时刻最多 max_inflight_per_worker 条在途命令；
//! - 超时/断链不自动重发；
//! - 按 (worker_id, boot_id, seq) 去重；
//! - Worker重启/离线时失败其所有在途命令。
//!
//! ## 管理面（Router 侧）
//!
//! - [`ControlPlane::submit_control`]：管理工具（`sekai.set_time` /
//!   `sekai.workers.set` / `sekai.cancel`）等控制命令，校验与 [`submit`]
//!   一致但**绕过**每Worker在途上限；
//! - [`ControlPlane::cancel`]：按 cmd_id 找到属主Worker后下发 `sekai.cancel`；
//! - [`ControlPlane::diagnostics`]：在途账本 / 晚到终态环 / 时钟状态快照；
//! - 首个 Hello 时钟偏差 >2s 或 `unix_ms==0` 时自动下发 `sekai.set_time`。

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::fmt;

use sekai::mqtt::client::{Client, ClientConfig, Event, PublishOptions, State};
use sekai::mqtt::packet::{Packet, Publish};
use sekai::mqtt::property::Properties;
use sekai::mqtt::qos::QoS;
use sekai::mqtt::topic::{Subscription, SubscriptionOptions, TopicFilter, TopicName};
use sekai::sekai_device_list::Endpoint;
use sekai::sekai_runtime::{CONTENT_TYPE_STDIN, CONTENT_TYPE_STDOUT};
use sekai::sekai_stdin::{JumpBody, Stdin, TaskBody, ToolBody};
use sekai::sekai_stdout::{ErrorPayload, Hello, Phase, Stdout};
use sekai::sekai_wire::ErrorCode;

use crate::topic::{TopicRoot, parse_incoming};
use crate::worker::{FrameAcceptance, RegistryEvent, WorkerInfo, WorkerRegistry, phase_name};

/// 每Worker默认在途命令上限（保守默认；产品链路由 `link_builder` 提升到 4，
/// 允许 prompt 内层在 Task 在途时继续调用同一 Worker 的工具）。
pub const DEFAULT_MAX_INFLIGHT_PER_WORKER: usize = 1;
/// Tool 命令的额外超时余量（毫秒）。
///
/// 默认 DDL 10s + 5s 余量 = 15s 控制端预算；覆盖 `pc.exec` 自身的 10s 上限，
/// 避免 3–10s 的真实命令被 Router 先判超时（B4.1）。
pub const TOOL_TIMEOUT_EXTRA_MS: u64 = 5_000;
/// Task 命令的额外超时余量（毫秒）。
pub const TASK_TIMEOUT_EXTRA_MS: u64 = 15_000;
/// 自动对时的允许偏差（毫秒）：|Worker unix_ms - 本机 now| 超过即校时。
pub const CLOCK_SYNC_MAX_SKEW_MS: u64 = 2_000;
/// 晚到终态环形缓冲容量。
pub const LATE_RESULTS_CAPACITY: usize = 16;

/// 命令类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandKind {
    /// ToolCall。
    Tool,
    /// Task。
    Task,
    /// Jump（链路迁移；payload 为 [`JumpBody`] 编码）。
    Jump,
}

impl CommandKind {
    /// 稳定字符串。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Tool => "tool",
            Self::Task => "task",
            Self::Jump => "jump",
        }
    }

    /// 对应的 Stdin Kind。
    pub fn stdin_kind(self) -> sekai::sekai_stdin::Kind {
        match self {
            Self::Tool => sekai::sekai_stdin::Kind::ToolCall,
            Self::Task => sekai::sekai_stdin::Kind::Task,
            Self::Jump => sekai::sekai_stdin::Kind::Jump,
        }
    }

    /// 从稳定字符串解析。
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "tool" => Some(Self::Tool),
            "task" => Some(Self::Task),
            "jump" => Some(Self::Jump),
            _ => None,
        }
    }
}

// ---- 诊断快照（gateway 层转交给 serve status / REPL /status）----

/// 控制面诊断快照（`serve status` 的 link/inflight/counters/clock 数据源）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GatewayDiagnostics {
    /// 最近一次链路/协议错误。
    pub last_error: Option<String>,
    /// 当前在途命令账本。
    pub inflight: Vec<InflightCommand>,
    /// 最近晚到的终态（环形，最旧在前）。
    pub late_results: Vec<LateResult>,
    /// 计数。
    pub counters: GatewayCounters,
    /// 所有在线Worker时钟是否已校准。
    pub clock_synced: bool,
}

/// 在途命令账本项。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InflightCommand {
    /// 命令 id。
    pub cmd_id: u64,
    /// Worker id。
    pub worker_id: String,
    /// 工具/任务名（Jump 为目标）。
    pub name: String,
    /// 已运行毫秒（由调用方传入的 now_mono 计算）。
    pub elapsed_ms: u64,
}

/// 晚到终态记录（超时/取消后 Worker 才回的最终事件）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LateResult {
    /// 原命令 id。
    pub cmd_id: u64,
    /// Worker id。
    pub worker_id: String,
    /// 晚到的终态：`completed` / `failed` / `ignored`。
    pub code: String,
    /// 该命令从提交到被判定晚到的毫秒数；未知时 0。
    pub elapsed_ms: u64,
}

/// 控制面计数。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GatewayCounters {
    /// 晚到终态累计条数。
    pub late_results: u64,
    /// 被丢弃的事件数（当前 ControlPlane 不丢事件，恒 0；由 gateway 合并）。
    pub dropped_events: u64,
}

/// 一次命令提交请求。
#[derive(Debug, Clone)]
pub struct CommandRequest {
    /// 目标Worker id。
    pub worker_id: String,
    /// 命令类型。
    pub kind: CommandKind,
    /// 工具/任务名（Worker Hello 中通告的名称）。
    pub name: String,
    /// UTF-8 参数文本。
    pub args: Vec<u8>,
    /// Worker端执行预算；0 表示Worker默认值。
    pub ddl_ms: u32,
    /// 控制端超时覆盖（毫秒）；None 使用计算值。
    pub timeout_ms: Option<u64>,
}

impl CommandRequest {
    /// 构造 Tool 请求。
    pub fn tool(
        worker_id: impl Into<String>,
        name: impl Into<String>,
        args: impl Into<Vec<u8>>,
    ) -> Self {
        Self {
            worker_id: worker_id.into(),
            kind: CommandKind::Tool,
            name: name.into(),
            args: args.into(),
            ddl_ms: 0,
            timeout_ms: None,
        }
    }

    /// 构造 Task 请求。
    pub fn task(
        worker_id: impl Into<String>,
        name: impl Into<String>,
        args: impl Into<Vec<u8>>,
    ) -> Self {
        Self {
            worker_id: worker_id.into(),
            kind: CommandKind::Task,
            name: name.into(),
            args: args.into(),
            ddl_ms: 0,
            timeout_ms: None,
        }
    }

    /// 构造 Jump 请求；`target` 为 Worker 端 DeviceList 名称或允许的 `host:port`。
    ///
    /// 复用 `name` 字段承载目标（展示 / 日志用），`args` 保持空；
    /// 实际 Stdin 载荷在 [`encode_payload`] 里编码为 [`JumpBody`]。
    pub fn jump(worker_id: impl Into<String>, target: impl Into<String>) -> Self {
        Self {
            worker_id: worker_id.into(),
            kind: CommandKind::Jump,
            name: target.into(),
            args: Vec::new(),
            ddl_ms: 0,
            timeout_ms: None,
        }
    }

    /// 设置 DDL。
    pub fn with_ddl(mut self, ddl_ms: u32) -> Self {
        self.ddl_ms = ddl_ms;
        self
    }

    /// 设置控制端超时。
    pub fn with_timeout(mut self, timeout_ms: u64) -> Self {
        self.timeout_ms = Some(timeout_ms);
        self
    }
}

/// 命令失败信息。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandFailure {
    /// 稳定服务端错误码（worker_failed / timeout / link_lost / worker_offline /
    /// worker_rebooted / protocol_error）。
    pub code: String,
    /// Worker返回的协议错误码（仅 worker_failed）。
    pub worker_code: Option<ErrorCode>,
    /// 可读消息。
    pub message: String,
    /// 是否建议模型重试（仅提示性字段）。
    pub retryable: bool,
}

impl CommandFailure {
    /// 服务端本地失败。
    pub fn server(code: impl Into<String>, message: impl Into<String>) -> Self {
        let code = code.into();
        let retryable = matches!(
            code.as_str(),
            "timeout" | "link_lost" | "worker_offline" | "worker_rebooted" | "worker_busy"
        );
        Self {
            code,
            worker_code: None,
            message: message.into(),
            retryable,
        }
    }

    /// Worker Failed 事件。
    pub fn worker(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code: String::from("worker_failed"),
            worker_code: Some(code),
            message: message.into(),
            retryable: false,
        }
    }
}

/// 命令结果。
#[derive(Debug, Clone)]
pub struct CommandResult {
    /// 命令 id。
    pub cmd_id: u64,
    /// Worker id。
    pub worker_id: String,
    /// 命令类型。
    pub kind: CommandKind,
    /// 工具/任务名。
    pub name: String,
    /// 结果载荷（Completed 事件原样返回）。
    pub payload: Vec<u8>,
    /// 实际观察到的阶段（按到达顺序，含最终阶段）。
    pub phases: Vec<Phase>,
    /// 端到端耗时（单调毫秒）。
    pub elapsed_ms: u64,
    /// 失败信息；成功为 None。
    pub failure: Option<CommandFailure>,
}

impl CommandResult {
    /// 是否成功。
    pub fn ok(&self) -> bool {
        self.failure.is_none()
    }

    /// 阶段名称列表。
    pub fn phase_names(&self) -> Vec<&'static str> {
        self.phases.iter().copied().map(phase_name).collect()
    }
}

/// 控制平面事件。
#[derive(Debug, Clone)]
pub enum ControlEvent {
    /// 链路/就绪状态变化。
    LinkState {
        /// MQTT 是否已连接。
        connected: bool,
        /// 两个订阅是否都已就绪。
        ready: bool,
        /// 最近一次错误。
        error: Option<String>,
    },
    /// Worker信息更新。
    WorkerUpdated(WorkerInfo),
    /// 命令结束（成功或失败）。
    CommandFinished(CommandResult),
}

/// 控制平面配置。
#[derive(Debug, Clone)]
pub struct ControlPlaneConfig {
    /// MQTT client id。
    pub client_id: String,
    /// MQTT 用户名。
    pub username: Option<String>,
    /// MQTT 密码。
    pub password: Option<String>,
    /// 主题根。
    pub topic_root: TopicRoot,
    /// Keep Alive 秒。
    pub keep_alive: u16,
    /// CONNECT 超时毫秒。
    pub connect_timeout_ms: u64,
    /// Receive Maximum。
    pub receive_maximum: u16,
    /// Maximum Packet Size。
    pub maximum_packet_size: u32,
    /// 每Worker在途命令上限。
    pub max_inflight_per_worker: usize,
    /// Tool 超时额外余量。
    pub tool_timeout_extra_ms: u64,
    /// Task 超时额外余量。
    pub task_timeout_extra_ms: u64,
    /// 默认 DDL（request.ddl_ms == 0 时用于计算超时）。
    pub default_ddl_ms: u32,
    /// 去重窗口。
    pub dedup_window: usize,
}

impl ControlPlaneConfig {
    /// 用 client id 与主题根构造默认配置。
    pub fn new(client_id: impl Into<String>, topic_root: TopicRoot) -> Self {
        Self {
            client_id: client_id.into(),
            username: None,
            password: None,
            topic_root,
            keep_alive: 30,
            connect_timeout_ms: 5_000,
            receive_maximum: 8,
            maximum_packet_size: 16_384,
            max_inflight_per_worker: DEFAULT_MAX_INFLIGHT_PER_WORKER,
            tool_timeout_extra_ms: TOOL_TIMEOUT_EXTRA_MS,
            task_timeout_extra_ms: TASK_TIMEOUT_EXTRA_MS,
            default_ddl_ms: 500,
            dedup_window: crate::worker::DEFAULT_DEDUP_WINDOW,
        }
    }
}

/// 控制平面错误。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ControlError {
    /// 配置错误。
    Config(String),
    /// 链路未就绪。
    NotReady,
    /// Worker未知。
    WorkerNotFound(String),
    /// Worker离线。
    WorkerOffline(String),
    /// 名称未通告。
    NotAdvertised {
        /// Worker id。
        worker_id: String,
        /// 名称。
        name: String,
    },
    /// Worker在途命令已满。
    WorkerBusy(String),
    /// 取消/查询时命令 id 不存在（已结束或从未提交）。
    UnknownCommand(u64),
    /// 协议错误。
    Protocol(String),
    /// 链路错误。
    Link(String),
}

impl ControlError {
    /// 稳定错误码（供 tool 结果 JSON 使用）。
    pub fn code(&self) -> &'static str {
        match self {
            Self::Config(_) => "protocol_error",
            Self::NotReady => "link_lost",
            Self::WorkerNotFound(_) => "worker_not_found",
            Self::WorkerOffline(_) => "worker_offline",
            Self::NotAdvertised { .. } => "not_advertised",
            Self::WorkerBusy(_) => "worker_busy",
            Self::UnknownCommand(_) => "unknown_command",
            Self::Protocol(_) => "protocol_error",
            Self::Link(_) => "link_lost",
        }
    }
}

impl fmt::Display for ControlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Config(message) => write!(f, "控制平面配置错误：{message}"),
            Self::NotReady => write!(f, "MQTT 链路未就绪"),
            Self::WorkerNotFound(id) => write!(f, "Worker {id} 不存在"),
            Self::WorkerOffline(id) => write!(f, "Worker {id} 离线"),
            Self::NotAdvertised { worker_id, name } => {
                write!(f, "Worker {worker_id} 未通告 {name}")
            }
            Self::WorkerBusy(id) => write!(f, "Worker {id} 仍有在途命令"),
            Self::UnknownCommand(cmd_id) => write!(f, "命令 {cmd_id} 不在在途账本中"),
            Self::Protocol(message) => write!(f, "协议错误：{message}"),
            Self::Link(message) => write!(f, "链路错误：{message}"),
        }
    }
}

impl std::error::Error for ControlError {}

struct PendingCommand {
    cmd_id: u64,
    worker_id: String,
    boot_id: u64,
    kind: CommandKind,
    name: String,
    phases: Vec<Phase>,
    submit_mono_ms: u64,
    deadline_mono_ms: u64,
    /// 静默命令（自动对时 / 内部管理）：终态不产生 `CommandFinished` 事件。
    silent: bool,
}

impl PendingCommand {
    fn push_phase(&mut self, phase: Phase) {
        if self.phases.last() != Some(&phase) {
            self.phases.push(phase);
        }
    }
}

/// 控制平面纯状态机。
pub struct ControlPlane {
    config: ControlPlaneConfig,
    client: Client,
    registry: WorkerRegistry,
    pending: BTreeMap<u64, PendingCommand>,
    inflight_by_worker: HashMap<String, usize>,
    worker_last_cmd_id: HashMap<String, u64>,
    events: VecDeque<ControlEvent>,
    ready: bool,
    connected: bool,
    last_error: Option<String>,
    subscribe_packet_id: Option<u16>,
    next_cmd_id: u64,
    last_mono_ms: u64,
    /// 晚到终态环（容量 [`LATE_RESULTS_CAPACITY`]，最旧在前）。
    late_results: VecDeque<LateResult>,
    /// 晚到终态累计条数。
    late_result_count: u64,
    /// 每Worker时钟是否已校准（最近一次 Hello 判定）。
    clock_synced: HashMap<String, bool>,
    /// 已对时检查过的 (worker_id, boot_id)；每次（重）连首个 Hello 才自动校时。
    clock_checked: HashSet<(String, u64)>,
}

impl ControlPlane {
    /// 构造控制平面；此时未连接。
    pub fn new(config: ControlPlaneConfig) -> Result<Self, ControlError> {
        if config.client_id.is_empty() {
            return Err(ControlError::Config(String::from("client_id 不能为空")));
        }
        if config.receive_maximum == 0 {
            return Err(ControlError::Config(String::from(
                "receive_maximum 不能为 0",
            )));
        }
        if config.maximum_packet_size == 0 {
            return Err(ControlError::Config(String::from(
                "maximum_packet_size 不能为 0",
            )));
        }
        if config.max_inflight_per_worker == 0 {
            return Err(ControlError::Config(String::from(
                "max_inflight_per_worker 不能为 0",
            )));
        }
        let client_config = ClientConfig {
            client_id: config.client_id.clone(),
            clean_start: true,
            keep_alive: config.keep_alive,
            connect_timeout: config.connect_timeout_ms,
            session_expiry_interval: Some(0),
            receive_maximum: Some(config.receive_maximum),
            maximum_packet_size: Some(config.maximum_packet_size),
            topic_alias_maximum: None,
            request_response_information: None,
            request_problem_information: None,
            user_properties: Vec::new(),
            will: None,
            username: config.username.clone(),
            password: config.password.clone().map(String::into_bytes),
            authentication_method: None,
            authentication_data: None,
            use_topic_alias: false,
        };
        let registry = WorkerRegistry::new(config.dedup_window);
        Ok(Self {
            config,
            client: Client::new(client_config),
            registry,
            pending: BTreeMap::new(),
            inflight_by_worker: HashMap::new(),
            worker_last_cmd_id: HashMap::new(),
            events: VecDeque::new(),
            ready: false,
            connected: false,
            last_error: None,
            subscribe_packet_id: None,
            next_cmd_id: 0,
            last_mono_ms: 0,
            late_results: VecDeque::new(),
            late_result_count: 0,
            clock_synced: HashMap::new(),
            clock_checked: HashSet::new(),
        })
    }

    /// 发起 MQTT 连接（CONNECT 入队）。
    pub fn link_up(&mut self, _endpoint: Endpoint, now_unix_ms: u64) -> Result<(), ControlError> {
        if self.client.state() != State::Disconnected {
            return Err(ControlError::Link(String::from("客户端已处于连接流程中")));
        }
        self.connected = false;
        self.ready = false;
        self.subscribe_packet_id = None;
        self.last_error = None;
        self.client
            .connect(now_unix_ms)
            .map_err(|error| ControlError::Link(error.to_string()))
    }

    /// 链路异常断开。
    pub fn link_lost(&mut self, now_unix_ms: u64) {
        self.client.transport_closed(now_unix_ms);
        self.handle_disconnect("link_lost", "MQTT 链路断开");
    }

    /// 链路正常关闭（同样失败在途命令，不自动重发）。
    pub fn link_closed(&mut self, now_unix_ms: u64) {
        self.client.transport_closed(now_unix_ms);
        self.handle_disconnect("link_lost", "MQTT 链路关闭");
    }

    /// 喂入一条已解码 MQTT 报文。
    pub fn feed(&mut self, packet: Packet, now_unix_ms: u64) -> Result<(), ControlError> {
        if let Err(error) = self.client.handle_packet(packet, now_unix_ms) {
            self.last_error = Some(error.to_string());
            self.client.transport_closed(now_unix_ms);
            self.handle_disconnect("protocol_error", &error.to_string());
            return Err(ControlError::Protocol(error.to_string()));
        }
        self.drain_client_events(now_unix_ms)
    }

    /// 推进定时器与命令超时。
    pub fn poll(&mut self, now_unix_ms: u64, now_mono_ms: u64) -> Result<(), ControlError> {
        self.last_mono_ms = now_mono_ms;
        if let Err(error) = self.client.poll(now_unix_ms) {
            self.last_error = Some(error.to_string());
            self.client.transport_closed(now_unix_ms);
            self.handle_disconnect("link_lost", &error.to_string());
            return Ok(());
        }
        self.drain_client_events(now_unix_ms)?;
        self.expire_pending(now_mono_ms);
        Ok(())
    }

    /// 取出下一条待发送 MQTT 报文。
    pub fn next_outgoing(&mut self, now_unix_ms: u64) -> Option<Packet> {
        self.client.next_outgoing(now_unix_ms)
    }

    /// 下一次命令超时的单调毫秒。
    pub fn next_deadline(&self) -> Option<u64> {
        self.pending
            .values()
            .map(|pending| pending.deadline_mono_ms)
            .min()
    }

    /// 提交命令并返回 cmd_id。
    pub fn submit(
        &mut self,
        request: CommandRequest,
        now_unix_ms: u64,
        now_mono_ms: u64,
    ) -> Result<u64, ControlError> {
        self.submit_with(request, now_unix_ms, now_mono_ms, false, false)
    }

    /// 提交管理/控制命令（`sekai.set_time` / `sekai.workers.set` / `sekai.cancel`）。
    ///
    /// 校验与 [`ControlPlane::submit`] 相同（ready / 在线 / 已通告），但
    /// **绕过**每Worker在途命令上限：管理命令必须能在长 Task 在途时送达。
    pub fn submit_control(
        &mut self,
        worker_id: &str,
        name: &str,
        payload: Vec<u8>,
        now_unix_ms: u64,
        now_mono_ms: u64,
    ) -> Result<u64, ControlError> {
        let request = CommandRequest {
            worker_id: String::from(worker_id),
            kind: CommandKind::Tool,
            name: String::from(name),
            args: payload,
            ddl_ms: 0,
            timeout_ms: None,
        };
        self.submit_with(request, now_unix_ms, now_mono_ms, true, false)
    }

    /// 取消一条在途命令：查在途账本找到属主Worker，下发 `sekai.cancel`
    /// （8 字节大端 cmd_id）。
    ///
    /// 返回 `sekai.cancel` 自身的 cmd_id；原命令的最终事件（通常是
    /// `Failed`/取消）仍由Worker回传并经 [`ControlEvent::CommandFinished`]
    /// 交给原等待者。
    pub fn cancel(
        &mut self,
        cmd_id: u64,
        now_unix_ms: u64,
        now_mono_ms: u64,
    ) -> Result<u64, ControlError> {
        let worker_id = self
            .pending
            .get(&cmd_id)
            .map(|pending| pending.worker_id.clone())
            .ok_or(ControlError::UnknownCommand(cmd_id))?;
        self.submit_control(
            &worker_id,
            "sekai.cancel",
            cmd_id.to_be_bytes().to_vec(),
            now_unix_ms,
            now_mono_ms,
        )
    }

    /// 控制面诊断快照（gateway 层转交 serve `status` / REPL `/status`）。
    ///
    /// `now_mono_ms` 只用于计算在途命令的已运行时长；调用方传自己的单调钟。
    pub fn diagnostics(&self, now_mono_ms: u64) -> GatewayDiagnostics {
        let inflight = self
            .pending
            .values()
            .map(|pending| InflightCommand {
                cmd_id: pending.cmd_id,
                worker_id: pending.worker_id.clone(),
                name: pending.name.clone(),
                elapsed_ms: now_mono_ms.saturating_sub(pending.submit_mono_ms),
            })
            .collect();
        GatewayDiagnostics {
            last_error: self.last_error.clone(),
            inflight,
            late_results: self.late_results.iter().cloned().collect(),
            counters: GatewayCounters {
                late_results: self.late_result_count,
                dropped_events: 0,
            },
            clock_synced: self.all_clocks_synced(),
        }
    }

    /// 所有在线Worker时钟是否已校准（无在线Worker时为 false）。
    pub fn clock_synced(&self) -> bool {
        self.all_clocks_synced()
    }

    fn all_clocks_synced(&self) -> bool {
        let mut online = 0usize;
        for worker in self.registry.workers() {
            if !worker.online {
                continue;
            }
            online += 1;
            if !self
                .clock_synced
                .get(&worker.worker_id)
                .copied()
                .unwrap_or(false)
            {
                return false;
            }
        }
        online > 0
    }

    /// 提交实现：`bypass_inflight` 供管理命令跳过在途上限；`silent` 让
    /// 内部命令（自动对时）的终态不产生事件。
    fn submit_with(
        &mut self,
        request: CommandRequest,
        now_unix_ms: u64,
        now_mono_ms: u64,
        bypass_inflight: bool,
        silent: bool,
    ) -> Result<u64, ControlError> {
        self.last_mono_ms = now_mono_ms;
        if !self.ready || self.client.state() != State::Connected {
            return Err(ControlError::NotReady);
        }
        let worker = self
            .registry
            .get(&request.worker_id)
            .ok_or_else(|| ControlError::WorkerNotFound(request.worker_id.clone()))?;
        if !worker.online {
            return Err(ControlError::WorkerOffline(request.worker_id.clone()));
        }
        let advertised = match request.kind {
            CommandKind::Tool => worker.has_tool(&request.name),
            CommandKind::Task => worker.has_task(&request.name),
            // Jump 不是 Worker 通告的能力：目标由 Worker 端 DeviceList 解析。
            CommandKind::Jump => true,
        };
        if !advertised {
            return Err(ControlError::NotAdvertised {
                worker_id: request.worker_id.clone(),
                name: request.name.clone(),
            });
        }
        if !bypass_inflight {
            let inflight = self
                .inflight_by_worker
                .get(&request.worker_id)
                .copied()
                .unwrap_or(0);
            if inflight >= self.config.max_inflight_per_worker {
                return Err(ControlError::WorkerBusy(request.worker_id.clone()));
            }
        }

        let effective_ddl = if request.ddl_ms == 0 {
            self.config.default_ddl_ms
        } else {
            request.ddl_ms
        };
        let extra = match request.kind {
            CommandKind::Tool | CommandKind::Jump => self.config.tool_timeout_extra_ms,
            CommandKind::Task => self.config.task_timeout_extra_ms,
        };
        let timeout_ms = request
            .timeout_ms
            .unwrap_or_else(|| u64::from(effective_ddl).saturating_add(extra))
            .max(1);

        let payload = encode_payload(request.kind, &request.name, &request.args)?;

        let last = self
            .worker_last_cmd_id
            .get(&request.worker_id)
            .copied()
            .unwrap_or(0);
        let cmd_id = last
            .max(self.next_cmd_id)
            .saturating_add(1)
            .max(now_unix_ms)
            .max(1);
        self.next_cmd_id = cmd_id;
        self.worker_last_cmd_id
            .insert(request.worker_id.clone(), cmd_id);

        let stdin = Stdin::new(
            cmd_id,
            request.kind.stdin_kind(),
            now_unix_ms,
            0,
            request.ddl_ms,
            payload,
        )
        .map_err(|error| ControlError::Protocol(error.to_string()))?;
        let frame = stdin
            .encode()
            .map_err(|error| ControlError::Protocol(error.to_string()))?;
        let topic_text = self.config.topic_root.worker_in(&request.worker_id);
        let topic = TopicName::new(topic_text)
            .map_err(|error| ControlError::Config(format!("in 主题非法：{error}")))?;
        let mut properties = Properties::new();
        properties.content_type = Some(String::from(CONTENT_TYPE_STDIN));
        let options = PublishOptions {
            qos: QoS::AtLeastOnce,
            retain: false,
            properties,
        };
        self.client
            .publish(topic, frame, options, now_unix_ms)
            .map_err(|error| ControlError::Link(error.to_string()))?;

        let pending = PendingCommand {
            cmd_id,
            worker_id: request.worker_id.clone(),
            boot_id: worker.boot_id,
            kind: request.kind,
            name: request.name,
            phases: Vec::new(),
            submit_mono_ms: now_mono_ms,
            deadline_mono_ms: now_mono_ms.saturating_add(timeout_ms),
            silent,
        };
        self.pending.insert(cmd_id, pending);
        *self
            .inflight_by_worker
            .entry(request.worker_id)
            .or_insert(0) += 1;
        Ok(cmd_id)
    }

    /// 取走累积事件。
    pub fn take_events(&mut self) -> Vec<ControlEvent> {
        self.events.drain(..).collect()
    }

    /// Worker快照。
    pub fn workers(&self) -> Vec<WorkerInfo> {
        self.registry.workers()
    }

    /// 单个Worker快照。
    pub fn worker(&self, worker_id: &str) -> Option<WorkerInfo> {
        self.registry.get(worker_id)
    }

    /// 订阅是否就绪（收到两个成功 SUBACK）。
    pub fn is_ready(&self) -> bool {
        self.ready
    }

    /// MQTT 是否已连接。
    pub fn is_connected(&self) -> bool {
        self.connected
    }

    /// 最近一次错误描述。
    pub fn last_error(&self) -> Option<&str> {
        self.last_error.as_deref()
    }

    // ---- 内部 ----

    fn note_error(&mut self, message: String) {
        self.last_error = Some(message);
    }

    fn emit_link_state(&mut self) {
        self.events.push_back(ControlEvent::LinkState {
            connected: self.connected,
            ready: self.ready,
            error: self.last_error.clone(),
        });
    }

    fn drain_client_events(&mut self, now_unix_ms: u64) -> Result<(), ControlError> {
        while let Some(event) = self.client.next_event() {
            self.handle_client_event(event, now_unix_ms)?;
        }
        if self.client.state() != State::Connected && self.connected {
            self.handle_disconnect("link_lost", "MQTT 连接已断开");
        }
        Ok(())
    }

    fn handle_client_event(&mut self, event: Event, now_unix_ms: u64) -> Result<(), ControlError> {
        match event {
            Event::Connected(_) => {
                self.connected = true;
                self.ready = false;
                self.subscribe_packet_id = None;
                self.last_error = None;
                // 新连接上重新检查时钟（Worker 可能已重启或未校准）。
                self.clock_checked.clear();
                self.subscribe();
                self.emit_link_state();
                // 新连接上的 retained Hello/Offline 是权威状态：清空去重窗口，
                // 否则断链前见过的 (boot_id, seq) 会让 retained Hello 被当成重复。
                self.registry.reset_dedup();
                // 重新连接后，旧Worker快照先标记离线，等 retained Hello 覆盖。
                let offline_events = self.registry.mark_all_offline(self.last_mono_ms);
                for event in offline_events {
                    self.emit_registry_event(event);
                }
            }
            Event::ConnectionRefused(connack) => {
                self.connected = false;
                self.ready = false;
                let message = format!("CONNACK 拒绝：{}", connack.reason_code);
                self.note_error(message);
                self.emit_link_state();
            }
            Event::Disconnected(disconnect) => {
                let message = format!("服务端 DISCONNECT：{}", disconnect.reason_code);
                self.note_error(message);
                self.handle_disconnect("link_lost", "服务端主动断开");
                self.emit_link_state();
            }
            Event::Publish(publish) => {
                self.handle_publish(publish, now_unix_ms)?;
            }
            Event::Suback(suback) => {
                if Some(suback.packet_id) == self.subscribe_packet_id {
                    let accepted = !suback.reason_codes.is_empty()
                        && suback.reason_codes.iter().all(|reason| !reason.is_error());
                    if accepted {
                        self.ready = true;
                        self.last_error = None;
                    } else {
                        self.ready = false;
                        self.note_error(format!(
                            "SUBACK 拒绝订阅：{} 个原因码",
                            suback.reason_codes.len()
                        ));
                    }
                    self.emit_link_state();
                }
            }
            Event::SessionReset { .. } | Event::Auth(_) => {}
            Event::Puback(_) | Event::Pubrec(_) | Event::Pubcomp(_) | Event::Unsuback(_) => {}
        }
        Ok(())
    }

    fn subscribe(&mut self) {
        let options = match SubscriptionOptions::new(QoS::AtLeastOnce, false, true, 0) {
            Ok(options) => options,
            Err(error) => {
                self.note_error(format!("订阅选项非法：{error}"));
                return;
            }
        };
        let mut filters = Vec::new();
        for filter in [
            self.config.topic_root.out_filter(),
            self.config.topic_root.state_filter(),
        ] {
            match TopicFilter::new(filter) {
                Ok(filter) => filters.push(Subscription { filter, options }),
                Err(error) => {
                    self.note_error(format!("订阅过滤器非法：{error}"));
                    return;
                }
            }
        }
        match self
            .client
            .subscribe(filters, Properties::new(), self.last_mono_ms)
        {
            Ok(packet_id) => self.subscribe_packet_id = Some(packet_id),
            Err(error) => {
                self.ready = false;
                self.note_error(format!("SUBSCRIBE 失败：{error}"));
            }
        }
    }

    fn handle_disconnect(&mut self, code: &str, message: &str) {
        let was_connected = self.connected;
        let was_ready = self.ready;
        self.connected = false;
        self.ready = false;
        self.subscribe_packet_id = None;
        self.fail_all_pending(code, message);
        let offline_events = self.registry.mark_all_offline(self.last_mono_ms);
        for event in offline_events {
            self.emit_registry_event(event);
        }
        self.note_error(String::from(message));
        if was_connected || was_ready {
            self.emit_link_state();
        }
    }

    fn emit_registry_event(&mut self, event: RegistryEvent) {
        match event {
            RegistryEvent::Updated(info) => {
                self.events.push_back(ControlEvent::WorkerUpdated(info));
            }
            RegistryEvent::Offline { worker_id } => {
                self.fail_worker_pending(&worker_id, "worker_offline", "Worker离线");
            }
            RegistryEvent::Rebooted { worker_id, .. } => {
                self.fail_worker_pending(&worker_id, "worker_rebooted", "Worker重启");
            }
        }
    }

    fn fail_all_pending(&mut self, code: &str, message: &str) {
        let pending = std::mem::take(&mut self.pending);
        self.inflight_by_worker.clear();
        for (_, command) in pending {
            if command.silent {
                continue;
            }
            let result = Self::failure_result(command, self.last_mono_ms, code, message);
            self.events.push_back(ControlEvent::CommandFinished(result));
        }
    }

    fn fail_worker_pending(&mut self, worker_id: &str, code: &str, message: &str) {
        let ids: Vec<u64> = self
            .pending
            .iter()
            .filter(|(_, pending)| pending.worker_id == worker_id)
            .map(|(id, _)| *id)
            .collect();
        for id in ids {
            if let Some(command) = self.pending.remove(&id) {
                let silent = command.silent;
                let result = Self::failure_result(command, self.last_mono_ms, code, message);
                if !silent {
                    self.events.push_back(ControlEvent::CommandFinished(result));
                }
            }
            if let Some(count) = self.inflight_by_worker.get_mut(worker_id) {
                *count = count.saturating_sub(1);
                if *count == 0 {
                    self.inflight_by_worker.remove(worker_id);
                }
            }
        }
    }

    fn expire_pending(&mut self, now_mono_ms: u64) {
        let expired: Vec<u64> = self
            .pending
            .iter()
            .filter(|(_, pending)| pending.deadline_mono_ms <= now_mono_ms)
            .map(|(id, _)| *id)
            .collect();
        for id in expired {
            if let Some(command) = self.pending.remove(&id) {
                let worker_id = command.worker_id.clone();
                let silent = command.silent;
                let result =
                    Self::failure_result(command, now_mono_ms, "timeout", "等待Worker最终事件超时");
                if !silent {
                    self.events.push_back(ControlEvent::CommandFinished(result));
                }
                self.decrement_inflight(&worker_id);
            }
        }
    }

    /// 记录一条晚到终态（超时/取消后 Worker 才回）：计数 + 16 条环形缓冲。
    fn note_late_result(&mut self, cmd_id: u64, worker_id: &str, code: &str) {
        self.late_result_count = self.late_result_count.saturating_add(1);
        if self.late_results.len() >= LATE_RESULTS_CAPACITY {
            self.late_results.pop_front();
        }
        self.late_results.push_back(LateResult {
            cmd_id,
            worker_id: String::from(worker_id),
            code: String::from(code),
            elapsed_ms: 0,
        });
    }

    fn failure_result(
        command: PendingCommand,
        now_mono_ms: u64,
        code: &str,
        message: &str,
    ) -> CommandResult {
        CommandResult {
            cmd_id: command.cmd_id,
            worker_id: command.worker_id,
            kind: command.kind,
            name: command.name,
            payload: Vec::new(),
            phases: command.phases,
            elapsed_ms: now_mono_ms.saturating_sub(command.submit_mono_ms),
            failure: Some(CommandFailure::server(code, message)),
        }
    }

    fn decrement_inflight(&mut self, worker_id: &str) {
        if let Some(count) = self.inflight_by_worker.get_mut(worker_id) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                self.inflight_by_worker.remove(worker_id);
            }
        }
    }

    fn handle_publish(&mut self, publish: Publish, now_unix_ms: u64) -> Result<(), ControlError> {
        let Some(topic) = publish.topic.as_ref() else {
            self.note_error(String::from("忽略无主题的 PUBLISH"));
            return Ok(());
        };
        let topic_text = String::from(topic.as_str());
        let Some(incoming) = parse_incoming(&topic_text) else {
            self.note_error(format!("忽略无法解析的主题：{topic_text}"));
            return Ok(());
        };
        if let Some(content_type) = publish.properties.content_type.as_deref()
            && content_type != CONTENT_TYPE_STDOUT
        {
            self.note_error(format!("忽略 content_type 不匹配的报文：{content_type}"));
            return Ok(());
        }
        let stdout = match Stdout::decode(&publish.payload) {
            Ok(stdout) => stdout,
            Err(error) => {
                self.note_error(format!(
                    "Stdout 解码失败（Worker {}）：{error}",
                    incoming.worker_id
                ));
                return Ok(());
            }
        };
        let worker_id = incoming.worker_id;
        match self
            .registry
            .accept_frame(&worker_id, stdout.boot_id, stdout.seq)
        {
            FrameAcceptance::Duplicate => {
                self.registry.mark_seen(&worker_id, self.last_mono_ms);
                return Ok(());
            }
            FrameAcceptance::New => {
                self.registry.mark_seen(&worker_id, self.last_mono_ms);
            }
        }
        match stdout.phase {
            Phase::Hello => self.handle_hello(&worker_id, &stdout, now_unix_ms),
            Phase::Offline => self.handle_offline(&worker_id, &stdout),
            Phase::Accepted | Phase::Started => {
                self.note_phase(&worker_id, &stdout);
                Ok(())
            }
            Phase::Progress => Ok(()),
            Phase::Completed | Phase::Failed => {
                self.finish_from_stdout(&worker_id, &stdout);
                Ok(())
            }
        }
    }

    fn handle_hello(
        &mut self,
        worker_id: &str,
        stdout: &Stdout,
        now_unix_ms: u64,
    ) -> Result<(), ControlError> {
        let hello = match Hello::decode(&stdout.payload) {
            Ok(hello) => hello,
            Err(error) => {
                self.note_error(format!("Hello 解码失败（Worker {worker_id}）：{error}"));
                return Ok(());
            }
        };
        if hello.worker_id != worker_id {
            self.note_error(format!(
                "Hello Worker id 与主题不一致：topic={worker_id}, hello={}",
                hello.worker_id
            ));
            return Ok(());
        }
        match self.registry.apply_hello(
            worker_id,
            &hello,
            stdout.boot_id,
            stdout.seq,
            self.last_mono_ms,
        ) {
            Ok(events) => {
                for event in events {
                    self.emit_registry_event(event);
                }
            }
            Err(error) => self.note_error(error.to_string()),
        }
        self.note_clock(worker_id, stdout.boot_id, hello.unix_ms, now_unix_ms);
        Ok(())
    }

    /// 时钟校准（B6）：每次（重）连后首个 Hello 若 `unix_ms==0` 或与本机
    /// 偏差 > [`CLOCK_SYNC_MAX_SKEW_MS`]，自动下发 `sekai.set_time`
    /// （8 字节大端 Unix 毫秒，走 [`ControlPlane::submit_control`]——不受
    /// `--allow-admin` 与在途上限限制）；结果静默消费，不进事件流。
    fn note_clock(&mut self, worker_id: &str, boot_id: u64, worker_unix_ms: u64, now_unix_ms: u64) {
        let skew = if worker_unix_ms == 0 {
            None
        } else {
            Some(worker_unix_ms.abs_diff(now_unix_ms))
        };
        let synced = skew
            .map(|value| value <= CLOCK_SYNC_MAX_SKEW_MS)
            .unwrap_or(false);
        self.clock_synced.insert(String::from(worker_id), synced);
        if synced {
            return;
        }
        let key = (String::from(worker_id), boot_id);
        if !self.clock_checked.insert(key) {
            // 同一启动周期只自动校时一次（避免 retained Hello 反复触发）。
            return;
        }
        let payload = now_unix_ms.to_be_bytes().to_vec();
        if let Err(error) = self.submit_control(
            worker_id,
            "sekai.set_time",
            payload,
            now_unix_ms,
            self.last_mono_ms,
        ) {
            self.note_error(format!("自动对时失败（Worker {worker_id}）：{error}"));
        }
    }

    fn handle_offline(&mut self, worker_id: &str, stdout: &Stdout) -> Result<(), ControlError> {
        match self
            .registry
            .apply_offline(worker_id, stdout.boot_id, stdout.seq, self.last_mono_ms)
        {
            Ok(events) => {
                for event in events {
                    self.emit_registry_event(event);
                }
            }
            Err(error) => self.note_error(error.to_string()),
        }
        Ok(())
    }

    fn note_phase(&mut self, worker_id: &str, stdout: &Stdout) {
        let mut error = None;
        match self.pending.get_mut(&stdout.cmd_id) {
            None => {
                error = Some(format!("忽略未知 cmd_id {} 的阶段事件", stdout.cmd_id));
            }
            Some(pending) => {
                if pending.worker_id != worker_id {
                    error = Some(format!(
                        "cmd {} 的Worker不匹配：期望 {}，收到 {worker_id}",
                        stdout.cmd_id, pending.worker_id
                    ));
                } else if pending.boot_id == stdout.boot_id {
                    pending.push_phase(stdout.phase);
                }
            }
        }
        if let Some(message) = error {
            self.note_error(message);
        }
    }

    fn finish_from_stdout(&mut self, worker_id: &str, stdout: &Stdout) {
        let Some(mut pending) = self.pending.remove(&stdout.cmd_id) else {
            // 晚到终态（已超时/取消）：进环形 late_results 并在 status 可见，
            // 不再污染 last_error（B8）。
            let code = match stdout.phase {
                Phase::Completed => "completed",
                Phase::Failed => "failed",
                _ => "ignored",
            };
            self.note_late_result(stdout.cmd_id, worker_id, code);
            return;
        };
        if pending.worker_id != worker_id {
            self.note_error(format!(
                "cmd {} 的Worker不匹配：期望 {}，收到 {worker_id}",
                stdout.cmd_id, pending.worker_id
            ));
            self.pending.insert(pending.cmd_id, pending);
            return;
        }
        if pending.boot_id != stdout.boot_id {
            self.note_error(format!(
                "cmd {} 的 boot_id 不匹配：Worker可能已重启但未收到新 Hello",
                stdout.cmd_id
            ));
            let silent = pending.silent;
            let result = Self::failure_result(
                pending,
                self.last_mono_ms,
                "worker_rebooted",
                "Worker boot_id 变化",
            );
            if !silent {
                self.events.push_back(ControlEvent::CommandFinished(result));
            }
            self.decrement_inflight(worker_id);
            return;
        }
        pending.push_phase(stdout.phase);
        let elapsed_ms = self.last_mono_ms.saturating_sub(pending.submit_mono_ms);
        let failure = match stdout.phase {
            Phase::Completed => None,
            Phase::Failed => Some(match ErrorPayload::decode(&stdout.payload) {
                Ok(error) => CommandFailure::worker(error.code, error.message),
                Err(error) => {
                    CommandFailure::server("protocol_error", format!("Failed 载荷非法：{error}"))
                }
            }),
            _ => Some(CommandFailure::server(
                "protocol_error",
                "非最终阶段进入最终事件处理",
            )),
        };
        let payload = if failure.is_none() {
            stdout.payload.clone()
        } else {
            Vec::new()
        };
        let silent = pending.silent;
        let result = CommandResult {
            cmd_id: pending.cmd_id,
            worker_id: pending.worker_id,
            kind: pending.kind,
            name: pending.name,
            payload,
            phases: pending.phases,
            elapsed_ms,
            failure,
        };
        if !silent {
            self.events.push_back(ControlEvent::CommandFinished(result));
        }
        self.decrement_inflight(worker_id);
    }

    // 供测试/诊断读取内部状态。
    #[cfg(test)]
    fn pending_count(&self) -> usize {
        self.pending.len()
    }
}

fn encode_payload(kind: CommandKind, name: &str, args: &[u8]) -> Result<Vec<u8>, ControlError> {
    let payload = match kind {
        CommandKind::Tool => ToolBody::new(name, args.to_vec())
            .map_err(|error| ControlError::Protocol(error.to_string()))?
            .encode()
            .map_err(|error| ControlError::Protocol(error.to_string()))?,
        CommandKind::Task => TaskBody::new(name, args.to_vec())
            .map_err(|error| ControlError::Protocol(error.to_string()))?
            .encode()
            .map_err(|error| ControlError::Protocol(error.to_string()))?,
        // Jump 复用 name 承载目标（见 [`CommandRequest::jump`]）。
        CommandKind::Jump => JumpBody::new(name)
            .map_err(|error| ControlError::Protocol(error.to_string()))?
            .encode()
            .map_err(|error| ControlError::Protocol(error.to_string()))?,
    };
    Ok(payload)
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::topic::TopicRoot;

    fn config() -> ControlPlaneConfig {
        ControlPlaneConfig::new(
            "sekai-router-test",
            TopicRoot::new("sekai/v1").expect("topic root"),
        )
    }

    #[test]
    fn new_validates_config() {
        let mut bad = config();
        bad.client_id.clear();
        assert!(matches!(
            ControlPlane::new(bad),
            Err(ControlError::Config(_))
        ));
        let mut bad = config();
        bad.max_inflight_per_worker = 0;
        assert!(matches!(
            ControlPlane::new(bad),
            Err(ControlError::Config(_))
        ));
    }

    #[test]
    fn submit_requires_ready() {
        let mut plane = ControlPlane::new(config()).expect("plane");
        let error = plane
            .submit(
                CommandRequest::tool("dev", "gpio.read", "IO4"),
                1_790_000_000_000,
                0,
            )
            .unwrap_err();
        assert_eq!(error, ControlError::NotReady);
        assert_eq!(plane.pending_count(), 0);
    }

    #[test]
    fn error_codes_are_stable() {
        assert_eq!(ControlError::NotReady.code(), "link_lost");
        assert_eq!(
            ControlError::WorkerOffline(String::from("x")).code(),
            "worker_offline"
        );
        assert_eq!(
            ControlError::NotAdvertised {
                worker_id: String::from("x"),
                name: String::from("y")
            }
            .code(),
            "not_advertised"
        );
        assert_eq!(
            ControlError::WorkerBusy(String::from("x")).code(),
            "worker_busy"
        );
    }

    #[test]
    fn command_failure_classification() {
        let failure = CommandFailure::server("timeout", "slow");
        assert!(failure.retryable);
        let failure = CommandFailure::server("policy_denied", "no");
        assert!(!failure.retryable);
        let failure = CommandFailure::worker(ErrorCode::InvalidArgument, "bad pin");
        assert_eq!(failure.code, "worker_failed");
        assert!(!failure.retryable);
    }

    #[test]
    fn command_kind_roundtrip() {
        for kind in [CommandKind::Tool, CommandKind::Task, CommandKind::Jump] {
            assert_eq!(CommandKind::parse(kind.as_str()), Some(kind));
        }
        assert_eq!(CommandKind::parse("jump"), Some(CommandKind::Jump));
        // Jump 走 Stdin Kind::Jump，且目标编码为 JumpBody 载荷。
        assert_eq!(
            CommandKind::Jump.stdin_kind(),
            sekai::sekai_stdin::Kind::Jump
        );
        let payload = encode_payload(CommandKind::Jump, "peer-1", &[]).expect("Jump 载荷");
        let body = JumpBody::decode(&payload).expect("JumpBody 解码");
        assert_eq!(body.target, "peer-1");
    }

    #[test]
    fn jump_request_uses_target_as_name() {
        let request = CommandRequest::jump("dev", "peer-1");
        assert_eq!(request.kind, CommandKind::Jump);
        assert_eq!(request.name, "peer-1");
        assert!(request.args.is_empty());
    }

    #[test]
    fn cancel_unknown_command_is_stable_error() {
        let mut plane = ControlPlane::new(config()).expect("plane");
        let error = plane.cancel(42, 1_790_000_000_000, 0).unwrap_err();
        assert_eq!(error, ControlError::UnknownCommand(42));
        assert_eq!(error.code(), "unknown_command");
    }

    #[test]
    fn diagnostics_default_is_empty() {
        let plane = ControlPlane::new(config()).expect("plane");
        let diagnostics = plane.diagnostics(0);
        assert!(diagnostics.inflight.is_empty());
        assert!(diagnostics.late_results.is_empty());
        assert_eq!(diagnostics.counters.late_results, 0);
        assert_eq!(diagnostics.counters.dropped_events, 0);
        assert!(!diagnostics.clock_synced);
        assert!(diagnostics.last_error.is_none());
    }

    #[test]
    fn timeout_constants_match_budget() {
        // B4.1：默认 DDL 10s + 5s 余量，覆盖 pc.exec 自身 10s 上限。
        assert_eq!(TOOL_TIMEOUT_EXTRA_MS, 5_000);
        assert!(u64::from(30_000u32) + TOOL_TIMEOUT_EXTRA_MS >= 10_000);
    }
}
