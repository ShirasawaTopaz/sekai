//! Worker控制平面运行时。
//!
//! `Runtime` 把 MQTT 客户端、Stdin/Stdout 协议、Task/Tool 注册表和Worker列表
//! 组合成一个无 I/O、无睡眠的状态机：
//!
//! - 调用方通过 [`Runtime::feed`] 喂入 MQTT 报文；
//! - 调用方通过 [`Runtime::poll`] 推进定时器、任务和跳转；
//! - 调用方通过 [`Runtime::next_outgoing`] 取出待发送报文；
//! - Jump 需要切换传输端点时，通过 [`Runtime::take_link_action`] 取走动作。
//!
//! 所有时间都来自注入的 [`Clock`]；所有出站事件都先进入有界的 Outbox，
//! 只有收到 PUBACK 或成功以 QoS0 发出后才删除，从而在断线/跳转时按
//! `(boot_id, seq)` 重放。
//!
//! 运行时本身不打印日志、不分配线程，也不依赖 `std`。

use alloc::collections::{BTreeMap, BTreeSet, VecDeque};
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;
use core::fmt;

use crate::mqtt::client::{Client, ClientConfig, ClientError, Event, PublishOptions, State};
use crate::mqtt::error::EncodeError;
use crate::mqtt::packet::{Auth, Connack, Disconnect, Packet, Publish, Suback, Will};
use crate::mqtt::property::Properties;
use crate::mqtt::qos::QoS;
use crate::mqtt::reason::ReasonCode;
use crate::mqtt::topic::{Subscription, SubscriptionOptions, TopicFilter, TopicName};
use crate::sekai_clock::Clock;
use crate::sekai_device_list::{DeviceList, Endpoint};
use crate::sekai_stdin::{JumpBody, Kind, Stdin, TaskBody, ToolBody};
use crate::sekai_stdout::{ActiveTask, ErrorPayload, Hello, JumpOutcome, Phase, Stdout};
use crate::sekai_task::{TaskContext, TaskPoll, TaskRegistry};
use crate::sekai_tool::{ToolContext, ToolRegistry};
use crate::sekai_wire::{
    CommandError, ErrorCode, MAX_ACTIVE_TASKS, MAX_DDL_MS, MAX_DELAY_MS, MAX_FIRMWARE_LEN,
    MAX_PAYLOAD, MAX_TASKS, MAX_TOOLS, ProtocolEncodeError, ProtocolError, validate_worker_id,
};

/// Router → Worker 的 PUBLISH `content_type`。
pub const CONTENT_TYPE_STDIN: &str = "application/vnd.sekai.stdin.v1";
/// Worker → Router 的 PUBLISH `content_type`。
pub const CONTENT_TYPE_STDOUT: &str = "application/vnd.sekai.stdout.v1";

/// 内置工具名；这些名字不进入用户 [`ToolRegistry`]，由运行时直接处理。
pub const BUILTIN_TOOLS: &[&str] = &[
    "sekai.cancel",
    "sekai.workers",
    "sekai.workers.set",
    "sekai.info",
    "sekai.set_time",
    "sekai.time",
];

/// 最终事件去重缓存的总字节上限。
const FINAL_CACHE_BYTES: usize = 4 * 1024;

/// 订阅失败后的重试退避下限（毫秒）；之后按 2 倍递增。
const SUBSCRIBE_RETRY_MIN_MS: u64 = 1_000;
/// 订阅失败后的重试退避上限（毫秒）。
const SUBSCRIBE_RETRY_MAX_MS: u64 = 30_000;
/// 两次 `sekai.set_time` 校准之间的最小间隔（单调毫秒）。
///
/// [`Clock::calibrate_unix_ms`](crate::sekai_clock::Clock::calibrate_unix_ms)
/// 的契约是「运行时负责限制校准频率」；这里取 1 秒，既允许 Router 重连后
/// 立即对时，又避免同一批命令反复回拨墙上时间。
const MIN_CALIBRATION_INTERVAL_MS: u64 = 1_000;

/// 运行时配置。
#[derive(Debug, Clone)]
pub struct RuntimeConfig {
    /// Worker id，同时作为 MQTT Client ID。
    pub worker_id: String,
    /// 固件版本，写入 Hello。
    pub firmware: String,
    /// 本启动周期 id，必须非 0。
    pub boot_id: u64,
    /// 主题根，默认 `sekai/v1`。
    pub topic_root: String,
    /// MQTT 客户端配置；`client_id` 会被 worker_id 覆盖。
    ///
    /// 默认走 Clean Start + `session_expiry_interval = 0`（K4）：Worker 不依赖
    /// broker 侧会话，停机期间 broker 不会缓存/重放 `in` 主题的 Stdin。
    pub mqtt: ClientConfig,
    /// `ddl_ms == 0` 时的默认执行预算（毫秒）。
    pub default_ddl_ms: u32,
    /// 允许的最大 `ddl_ms`。
    pub max_ddl_ms: u32,
    /// 允许的最大 `delay_ms`。
    pub max_delay_ms: u32,
    /// 命令最大年龄（Unix 毫秒）；`None` 表示不检查。
    pub max_command_age_ms: Option<u64>,
    /// 并发运行的 Task 上限。
    pub max_running_tasks: usize,
    /// 等待执行的命令上限（含延迟 Task/Tool/Jump）。
    pub max_scheduled_tasks: usize,
    /// 收件箱上限。
    pub max_inbox: usize,
    /// Outbox 条目上限。
    pub max_outbox: usize,
    /// Outbox 载荷字节上限（只统计非最终事件）。
    pub outbox_bytes: usize,
    /// Outbox 中**最终事件**的独立条目预算。
    ///
    /// 最终事件不与该预算之外的中间事件互相挤占：中间事件写满时只能淘汰
    /// 中间事件，最终事件写满自己的预算时只淘汰最老的排队最终事件（见
    /// `Outbox`）。因此 Router 不能假设中间事件完整，但应能拿到每条命令的
    /// 最终事件（协议 §9.6）。
    pub max_outbox_final: usize,
    /// Outbox 中**最终事件**的独立字节预算。
    ///
    /// 与 [`Self::outbox_bytes`] 分开计数，默认至少能容纳一条满额
    /// （4096 字节）最终事件；更多大结果会把更老的排队最终事件挤出去。
    pub final_outbox_bytes: usize,
    /// 去重 id 窗口。
    pub dedup_window: usize,
    /// 最终事件缓存条数。
    pub final_cache: usize,
    /// Jump 排空发件箱的最长等待（毫秒）。
    pub jump_drain_ms: u64,
    /// Jump 回退（`FallingBack`）的最长等待（毫秒）。
    ///
    /// 目标端点不可达时先回退原端点；若回退链路也连不上/不回 CONNACK，
    /// 超过该时限必须结束 Jump 状态并给原命令补发 `JumpUnreachable` 终态，
    /// 避免「Jump 一直活跃 → 后续命令全部被拒」的死锁（审计 K3）。
    pub jump_fallback_ms: u64,
    /// 连续服务器迁移上限。
    pub max_consecutive_server_moves: u8,
    /// 是否允许 Jump 目标直接写 `host:port`。
    pub allow_arbitrary_jump_targets: bool,
    /// 连接时是否设置 retained Offline 遗嘱。
    pub will_offline: bool,
}

impl RuntimeConfig {
    /// 用合理的控制平面默认值创建配置。
    pub fn new(worker_id: impl Into<String>, boot_id: u64) -> Self {
        let mqtt = ClientConfig {
            // K4：Worker 不依赖 broker 侧会话。Clean Start + session expiry 0
            // 保证 broker 在 Worker 停机期间不会缓存/重放 `in` 主题上的
            // Stdin（cmd_id 去重窗口只是内存态，重启后为空，重放会重复执行）。
            // 重连后由运行时重新 SUBSCRIBE 并发布 Hello，无需会话恢复。
            clean_start: true,
            keep_alive: 30,
            connect_timeout: 5_000,
            session_expiry_interval: Some(0),
            receive_maximum: Some(8),
            maximum_packet_size: Some(16 * 1024),
            ..ClientConfig::default()
        };
        Self {
            worker_id: worker_id.into(),
            firmware: String::from(option_env!("CARGO_PKG_VERSION").unwrap_or("0.1.0")),
            boot_id,
            topic_root: String::from("sekai/v1"),
            mqtt,
            default_ddl_ms: crate::sekai_wire::DEFAULT_DDL_MS,
            max_ddl_ms: MAX_DDL_MS,
            max_delay_ms: MAX_DELAY_MS,
            max_command_age_ms: Some(300_000),
            max_running_tasks: 4,
            max_scheduled_tasks: 4,
            max_inbox: 4,
            max_outbox: 8,
            outbox_bytes: 16 * 1024,
            max_outbox_final: 4,
            final_outbox_bytes: 8 * 1024,
            dedup_window: 32,
            final_cache: 8,
            jump_drain_ms: 1_500,
            jump_fallback_ms: 5_000,
            max_consecutive_server_moves: 3,
            allow_arbitrary_jump_targets: false,
            will_offline: true,
        }
    }

    fn validate(&self) -> Result<(), RuntimeError> {
        validate_worker_id(&self.worker_id).map_err(RuntimeError::InvalidConfig)?;
        if self.boot_id == 0 {
            return Err(RuntimeError::InvalidConfig("boot_id 不能为 0"));
        }
        if self.firmware.is_empty() {
            return Err(RuntimeError::InvalidConfig("固件版本不能为空"));
        }
        if self.firmware.len() > MAX_FIRMWARE_LEN {
            return Err(RuntimeError::InvalidConfig("固件版本超过 64 字节"));
        }
        if self.topic_root.is_empty()
            || self.topic_root.len() > 128
            || self.topic_root.starts_with('/')
            || self.topic_root.ends_with('/')
            || self.topic_root.contains('+')
            || self.topic_root.contains('#')
            || self.topic_root.as_bytes().contains(&0)
        {
            return Err(RuntimeError::InvalidConfig("topic_root 非法"));
        }
        if self.default_ddl_ms == 0 || self.default_ddl_ms > self.max_ddl_ms {
            return Err(RuntimeError::InvalidConfig("default_ddl_ms 非法"));
        }
        if self.max_ddl_ms > MAX_DDL_MS {
            return Err(RuntimeError::InvalidConfig("max_ddl_ms 超过协议上限"));
        }
        if self.max_delay_ms > MAX_DELAY_MS {
            return Err(RuntimeError::InvalidConfig("max_delay_ms 超过协议上限"));
        }
        if self.max_running_tasks == 0
            || self.max_scheduled_tasks == 0
            || self.max_inbox == 0
            || self.max_outbox == 0
            || self.outbox_bytes == 0
            || self.max_outbox_final == 0
            || self.final_outbox_bytes == 0
            || self.dedup_window == 0
            || self.final_cache == 0
        {
            return Err(RuntimeError::InvalidConfig("运行时上限不能为 0"));
        }
        if self.jump_fallback_ms == 0 {
            return Err(RuntimeError::InvalidConfig("jump_fallback_ms 不能为 0"));
        }
        // K4：Clean Start 为假且会话不过期时，broker 可能在 Worker 停机期间
        // 缓存 `in` 主题的 Stdin 并在重连后重放；而运行时去重窗口是纯内存的，
        // 重启后为空 → 命令会被重复执行。默认配置走 Clean Start；调用方若
        // 坚持会话恢复，必须先自行把去重状态持久化。
        if !self.mqtt.clean_start && self.mqtt.session_expiry_interval.unwrap_or(0) > 0 {
            return Err(RuntimeError::InvalidConfig(
                "clean_start=false 且 session_expiry>0 会让 broker 重放停机期间的 Stdin",
            ));
        }
        if self.mqtt.receive_maximum == Some(0) {
            return Err(RuntimeError::InvalidConfig("receive_maximum 不能为 0"));
        }
        if self.mqtt.maximum_packet_size == Some(0) {
            return Err(RuntimeError::InvalidConfig("maximum_packet_size 不能为 0"));
        }
        Ok(())
    }
}

/// 运行时错误；单条命令的错误不会走这里，而是变成 `Failed` 事件。
#[derive(Debug)]
pub enum RuntimeError {
    /// MQTT 客户端状态机错误。
    Client(ClientError),
    /// MQTT 报文编码错误。
    Encode(EncodeError),
    /// 控制平面载荷编码错误。
    ProtocolEncode(ProtocolEncodeError),
    /// 控制平面帧/字段错误。
    Protocol(ProtocolError),
    /// 配置非法。
    InvalidConfig(&'static str),
    /// 主题构造失败。
    Topic(&'static str),
}

impl fmt::Display for RuntimeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Client(error) => write!(f, "MQTT 客户端错误：{error}"),
            Self::Encode(error) => write!(f, "MQTT 编码错误：{error}"),
            Self::ProtocolEncode(error) => write!(f, "控制平面编码错误：{error}"),
            Self::Protocol(error) => write!(f, "控制平面协议错误：{error}"),
            Self::InvalidConfig(message) => write!(f, "配置错误：{message}"),
            Self::Topic(message) => write!(f, "主题错误：{message}"),
        }
    }
}

impl core::error::Error for RuntimeError {}

impl From<ClientError> for RuntimeError {
    fn from(error: ClientError) -> Self {
        Self::Client(error)
    }
}

impl From<EncodeError> for RuntimeError {
    fn from(error: EncodeError) -> Self {
        Self::Encode(error)
    }
}

impl From<ProtocolEncodeError> for RuntimeError {
    fn from(error: ProtocolEncodeError) -> Self {
        Self::ProtocolEncode(error)
    }
}

impl From<ProtocolError> for RuntimeError {
    fn from(error: ProtocolError) -> Self {
        Self::Protocol(error)
    }
}

/// 链路动作；由 start 层执行。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkAction {
    /// 关闭当前链路并连接新端点。
    Switch {
        /// 目标端点。
        endpoint: Endpoint,
        /// 触发原因。
        reason: SwitchReason,
    },
}

/// 链路切换原因。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SwitchReason {
    /// 收到 Jump 命令。
    JumpCommand,
    /// MQTT 服务器迁移（0x9C/0x9D + server_reference）。
    ServerMoved,
    /// 迁移失败，回退。
    Fallback,
}

/// Jump 当前阶段。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JumpPhase {
    /// 等待发件箱排空。
    Draining,
    /// 等待新链路 CONNACK。
    Switching,
    /// 等待回退链路 CONNACK。
    FallingBack,
}

/// 运行时观测快照。
#[derive(Debug, Clone)]
pub struct RuntimeStatus {
    /// MQTT 客户端状态。
    pub mqtt_state: State,
    /// 当前链路端点。
    pub endpoint: Option<Endpoint>,
    /// Jump 阶段。
    pub jump: Option<JumpPhase>,
    /// 收件箱条目数。
    pub inbox: usize,
    /// 等待执行的命令数。
    pub scheduled: usize,
    /// 运行中的 Task 数。
    pub running: usize,
    /// Outbox 条目数。
    pub outbox: usize,
    /// Outbox 载荷字节数。
    pub outbox_bytes: usize,
    /// 等待 PUBACK 的事件数。
    pub pending_acks: usize,
    /// 是否已订阅输入主题。
    pub subscribed: bool,
    /// 最近一次 SUBACK 的错误码；`None` 表示最近一次订阅成功（或尚未收到
    /// SUBACK）。供 `status`/日志观测「订阅被 broker 拒绝」（审计 K5）。
    pub subscribe_error: Option<ReasonCode>,
    /// 订阅尝试次数：SUBACK 错误与客户端本地发送失败都计数（K5）。
    pub subscribe_attempts: u32,
    /// 当前 Unix 毫秒。
    pub clock_unix_ms: Option<u64>,
    /// 被丢弃的出站事件数。
    pub dropped_events: u64,
    /// 被拒绝/丢弃的命令数。
    pub rejected_commands: u64,
    /// 连续服务器迁移次数。
    pub consecutive_moves: u8,
}

/// Worker控制平面运行时。
pub struct Runtime<C: Clock> {
    clock: C,
    config: RuntimeConfig,
    client: Client,
    topics: Topics,
    workers: DeviceList,
    tasks: TaskRegistry,
    tools: ToolRegistry,
    inbox: VecDeque<Command>,
    scheduled: Vec<Scheduled>,
    running: Vec<Running>,
    outbox: Outbox,
    dedup: Dedup,
    next_seq: u32,
    subscribed: bool,
    /// 下一次重试 SUBSCRIBE 的单调毫秒时刻（K5）。
    subscribe_retry_at: Option<u64>,
    /// 最近一次成功发起对时的单调毫秒时刻（K8 限频）。
    last_calibration: Option<u64>,
    link: LinkState,
    jump: JumpState,
    link_actions: VecDeque<LinkAction>,
    expect_link_close: bool,
    metrics: Metrics,
}

impl<C: Clock> Runtime<C> {
    /// 构造运行时并校验配置/注册表上限。
    pub fn new(
        config: RuntimeConfig,
        clock: C,
        workers: DeviceList,
        tasks: TaskRegistry,
        tools: ToolRegistry,
    ) -> Result<Self, RuntimeError> {
        config.validate()?;
        if tools.len() + BUILTIN_TOOLS.len() > MAX_TOOLS {
            return Err(RuntimeError::InvalidConfig("工具数量超过 16"));
        }
        if tasks.len() > MAX_TASKS {
            return Err(RuntimeError::InvalidConfig("任务数量超过 16"));
        }
        let topics = Topics::new(&config.topic_root, &config.worker_id)?;
        let mut mqtt = config.mqtt.clone();
        mqtt.client_id = config.worker_id.clone();
        if mqtt.connect_timeout == 0 {
            mqtt.connect_timeout = 5_000;
        }
        if mqtt.receive_maximum.is_none() {
            mqtt.receive_maximum = Some(8);
        }
        if mqtt.maximum_packet_size.is_none() {
            mqtt.maximum_packet_size = Some(16 * 1024);
        }
        if mqtt.session_expiry_interval.is_none() {
            // 未显式配置时按 MQTT 默认值 0（会话随连接结束）；Clean Start
            // 语义下不会出现「旧会话补发消息」（K4）。
            mqtt.session_expiry_interval = Some(0);
        }
        let client = Client::new(mqtt);
        let outbox = Outbox::new(
            config.max_outbox,
            config.outbox_bytes,
            config.max_outbox_final,
            config.final_outbox_bytes,
        );
        let dedup = Dedup::new(config.dedup_window, config.final_cache);
        let mut runtime = Self {
            clock,
            config,
            client,
            topics,
            workers,
            tasks,
            tools,
            inbox: VecDeque::new(),
            scheduled: Vec::new(),
            running: Vec::new(),
            outbox,
            dedup,
            next_seq: 1,
            subscribed: false,
            subscribe_retry_at: None,
            last_calibration: None,
            link: LinkState::default(),
            jump: JumpState::Idle,
            link_actions: VecDeque::new(),
            expect_link_close: false,
            metrics: Metrics::default(),
        };
        // 构造时先按当前时钟生成一份 LWT；每次 CONNECT 前会在 link_up 中重建
        // （K6：时间戳取 CONNECT 时刻，而不是 Runtime 创建时刻）。
        runtime.refresh_will()?;
        Ok(runtime)
    }

    /// 按当前时钟重建 LWT `Offline` 载荷（K6）。
    ///
    /// 遗嘱消息在 CONNECT 时固定，因此必须在每次 `connect` 之前重建：
    /// 时间戳取「这一次 CONNECT」的墙上时间（未校准为 0），`seq = 0`。
    fn refresh_will(&mut self) -> Result<(), RuntimeError> {
        if !self.config.will_offline {
            return Ok(());
        }
        let offline = Stdout::lwt_offline(self.config.boot_id, self.clock.unix_ms().unwrap_or(0))?;
        let will = Will {
            topic: self.topics.state.clone(),
            payload: offline.encode()?,
            qos: QoS::AtLeastOnce,
            retain: true,
            properties: stdout_properties(),
        };
        self.client.set_will(Some(will))?;
        Ok(())
    }

    // ---- 链路生命周期（start 层调用） ----

    /// TCP 连接建立后调用：记录端点、重建 LWT 并发起 MQTT CONNECT。
    pub fn link_up(&mut self, endpoint: Endpoint) -> Result<(), RuntimeError> {
        if self.client.state() != State::Disconnected {
            return Err(RuntimeError::InvalidConfig("链路已连接"));
        }
        self.link.current = Some(endpoint);
        let now = self.clock.monotonic_ms();
        self.refresh_will()?;
        self.client.connect(now).map_err(RuntimeError::Client)?;
        Ok(())
    }

    /// 由 [`LinkAction::Switch`] 触发的正常关闭；不触发 Jump 回退。
    pub fn link_closed(&mut self) {
        self.expect_link_close = false;
        let now = self.clock.monotonic_ms();
        self.client.transport_closed(now);
        let _ = self.drain_events();
    }

    /// 非预期断开（EOF/读写错误）；如果正在切换则回退。
    pub fn link_lost(&mut self) {
        let expected = self.expect_link_close;
        self.expect_link_close = false;
        let was_draining = matches!(&self.jump, JumpState::Draining { .. });
        let was_switching = matches!(&self.jump, JumpState::Switching { .. });
        let now = self.clock.monotonic_ms();
        self.client.transport_closed(now);
        let _ = self.drain_events();
        if expected {
            return;
        }
        if was_draining {
            let _ = self.switch_from_draining();
        } else if was_switching {
            let _ = self.begin_fallback(now);
        }
    }

    /// TCP 连接失败（尚未发出 MQTT CONNECT）。
    ///
    /// - 目标是 Jump 目标（`Switching`）→ 回退原端点；
    /// - 目标是回退端点（`FallingBack`）→ 原端点也不可达，Jump 以
    ///   `JumpUnreachable` 终态收场（K3）。
    pub fn link_failed(&mut self, endpoint: Endpoint) {
        let now = self.clock.monotonic_ms();
        let switching_to_target = matches!(
            &self.jump,
            JumpState::Switching { target, .. } if *target == endpoint
        );
        let failing_back_to_home = matches!(
            &self.jump,
            JumpState::FallingBack { home, .. } if *home == endpoint
        );
        if switching_to_target {
            let _ = self.begin_fallback(now);
        } else if failing_back_to_home {
            let cmd_id = self.jump.cmd_id();
            let _ = self.finish_fallback(cmd_id);
        }
    }

    // ---- MQTT 报文与推进 ----

    /// 喂入一条已解码的 MQTT 报文。
    pub fn feed(&mut self, packet: Packet) -> Result<(), RuntimeError> {
        let now = self.clock.monotonic_ms();
        let result = self.client.handle_packet(packet, now);
        let drain = self.drain_events();
        drain?;
        result.map_err(RuntimeError::Client)
    }

    /// 推进 MQTT 定时器、收件箱、任务、发件箱和 Jump。
    pub fn poll(&mut self) -> Result<(), RuntimeError> {
        let now = self.clock.monotonic_ms();
        let poll_result = self.client.poll(now);
        let drain_result = self.drain_events();
        if let Err(error) = poll_result {
            match self.jump.phase() {
                Some(JumpPhase::Switching) => self.begin_fallback(now)?,
                // 回退连接也超时/报错：Jump 必须有终态，不能一直吞掉错误（K3）。
                Some(JumpPhase::FallingBack) => {
                    let cmd_id = self.jump.cmd_id();
                    self.finish_fallback(cmd_id)?;
                }
                _ => return Err(RuntimeError::Client(error)),
            }
        }
        drain_result?;

        if !self.jump.is_active() {
            self.process_inbox(now)?;
        }
        self.retry_subscribe_if_due(now);
        self.start_due(now)?;
        self.step_tasks(now)?;
        self.flush_outbox(now)?;
        self.advance_jump(now)?;
        Ok(())
    }

    /// 取出下一条待发送 MQTT 报文。
    pub fn next_outgoing(&mut self) -> Option<Packet> {
        let now = self.clock.monotonic_ms();
        self.client.next_outgoing(now)
    }

    /// 下一次需要 poll 的单调毫秒时刻。
    pub fn next_deadline(&self) -> Option<u64> {
        let mut deadline = self.client.next_deadline();
        // 只有队首命令可启动时才需要为等待队列设闹钟；否则由运行中 Task 的
        // next_poll_at/DDL 唤醒，避免在 running 已满时按过去的 start_at 忙等。
        let first_schedulable = self
            .scheduled
            .first()
            .map(|scheduled| {
                let is_task = matches!(&scheduled.command.kind, CommandKind::Task { .. });
                !is_task || self.running.len() < self.config.max_running_tasks
            })
            .unwrap_or(false);
        if first_schedulable && let Some(scheduled) = self.scheduled.first() {
            deadline = fold_deadline(deadline, scheduled.start_at);
        }
        for running in &self.running {
            deadline = fold_deadline(deadline, running.next_poll_at);
            deadline = fold_deadline(
                deadline,
                running.started_at.saturating_add(u64::from(running.ddl_ms)),
            );
        }
        if let Some(until) = self.jump.until() {
            deadline = fold_deadline(deadline, until);
        }
        if self.client.state() == State::Connected
            && !self.subscribed
            && let Some(retry_at) = self.subscribe_retry_at
        {
            deadline = fold_deadline(deadline, retry_at);
        }
        if !self.jump.is_active() && !self.inbox.is_empty() {
            deadline = fold_deadline(deadline, self.clock.monotonic_ms());
        }
        deadline
    }

    /// 入站最大报文长度（来自 MQTT 配置），供传输层分配缓冲。
    pub fn max_incoming_packet_size(&self) -> Option<u32> {
        self.client.max_incoming_packet_size()
    }

    /// 取出一个链路动作。
    pub fn take_link_action(&mut self) -> Option<LinkAction> {
        self.link_actions.pop_front()
    }

    /// 当前观测快照。
    pub fn status(&self) -> RuntimeStatus {
        RuntimeStatus {
            mqtt_state: self.client.state(),
            endpoint: self.link.current.clone(),
            jump: self.jump.phase(),
            inbox: self.inbox.len(),
            scheduled: self.scheduled.len(),
            running: self.running.len(),
            outbox: self.outbox.entries.len(),
            outbox_bytes: self.outbox.bytes_total(),
            pending_acks: self.outbox.pending_acks.len(),
            subscribed: self.subscribed,
            subscribe_error: self.metrics.last_suback_error,
            subscribe_attempts: self.metrics.subscribe_attempts,
            clock_unix_ms: self.clock.unix_ms(),
            dropped_events: self.metrics.dropped_events,
            rejected_commands: self.metrics.rejected_commands,
            consecutive_moves: self.metrics.consecutive_moves,
        }
    }

    // ---- MQTT 事件 ----

    fn drain_events(&mut self) -> Result<(), RuntimeError> {
        while let Some(event) = self.client.next_event() {
            let now = self.clock.monotonic_ms();
            match event {
                Event::Connected(connack) => self.on_connected(connack, now)?,
                Event::ConnectionRefused(_) => self.on_connection_refused(now)?,
                Event::Disconnected(disconnect) => self.on_disconnected(disconnect)?,
                Event::Publish(publish) => self.on_publish(publish)?,
                Event::Puback(ack) => self.outbox.on_puback(ack.packet_id),
                Event::Suback(suback) => self.on_suback(&suback, now),
                Event::SessionReset { lost_packet_ids } => {
                    self.outbox.on_session_reset(&lost_packet_ids);
                }
                Event::Auth(auth) => self.on_auth(auth)?,
                Event::Pubrec(_) | Event::Pubcomp(_) | Event::Unsuback(_) => {}
            }
        }
        Ok(())
    }

    /// MQTT 5 增强认证应答（K2）。
    ///
    /// 运行时没有逐轮计算凭据的能力：对服务端的 challenge/re-auth 直接回送
    /// CONNECT 中配置的 `authentication_method/data`。对静态令牌型认证这足以
    /// 完成握手；需要客户端按 challenge 计算的方案会在后续轮次被 broker
    /// 拒绝（连接以 CONNACK/DISCONNECT 错误收场），而不是像以前那样把 AUTH
    /// 事件丢掉、一直挂到 CONNECT 超时。未配置认证方式时忽略（服务端本不应
    /// 在客户端未请求时发 AUTH）。
    fn on_auth(&mut self, auth: Auth) -> Result<(), RuntimeError> {
        let Some(method) = self.config.mqtt.authentication_method.clone() else {
            return Ok(());
        };
        // 只有服务端继续挑战/要求重认证时才需要客户端再发 AUTH；Success 或
        // 错误原因码意味着该轮认证已结束。
        if !matches!(
            auth.reason_code,
            ReasonCode::ContinueAuthentication | ReasonCode::ReAuthenticate
        ) {
            return Ok(());
        }
        let mut properties = Properties::new();
        properties.authentication_method = Some(method);
        properties.authentication_data = self.config.mqtt.authentication_data.clone();
        let response = Auth {
            reason_code: ReasonCode::ContinueAuthentication,
            properties,
        };
        self.client.auth(response, self.clock.monotonic_ms())?;
        Ok(())
    }

    fn on_connected(&mut self, connack: Connack, now: u64) -> Result<(), RuntimeError> {
        if let Some(current) = self.link.current.clone() {
            self.link.last_good = Some(current);
        }
        let was_jump = core::mem::replace(&mut self.jump, JumpState::Idle);
        let jump_completed = match &was_jump {
            JumpState::Switching { cmd_id, target, .. } => Some((*cmd_id, target.clone())),
            _ => None,
        };
        // 回退链路重新连上：Jump 以失败收场；失败帧与 Hello 一起进发件箱，
        // 重连后按 seq 顺序送达原 Router（K3）。
        let fallback_cmd = match &was_jump {
            JumpState::FallingBack { cmd_id, .. } => Some(*cmd_id),
            _ => None,
        };
        if connack.session_present {
            self.subscribed = true;
            self.subscribe_retry_at = None;
        }
        if !self.subscribed {
            let subscription = self.input_subscription()?;
            self.try_subscribe(subscription, now);
        }
        self.emit_hello(now)?;
        if let Some((cmd_id, target)) = jump_completed
            && cmd_id != 0
        {
            let payload = JumpOutcome::new(target).encode()?;
            self.emit(Phase::Completed, cmd_id, payload)?;
        }
        if let Some(cmd_id) = fallback_cmd
            && cmd_id != 0
            && self.dedup.final_for(cmd_id).is_none()
        {
            self.emit_failed(cmd_id, ErrorCode::JumpUnreachable, "跳转目标不可达，已回退")?;
        }
        self.expect_link_close = false;
        Ok(())
    }

    fn on_connection_refused(&mut self, now: u64) -> Result<(), RuntimeError> {
        match self.jump.phase() {
            Some(JumpPhase::Switching) => self.begin_fallback(now)?,
            // 回退端点也拒绝连接：原端点不可达，Jump 立即以失败终态收场。
            Some(JumpPhase::FallingBack) => {
                let cmd_id = self.jump.cmd_id();
                self.finish_fallback(cmd_id)?;
            }
            _ => {}
        }
        Ok(())
    }

    /// 构造 `in` 主题订阅（QoS1；`no_local=false`、`retain_as_published=false`）。
    fn input_subscription(&self) -> Result<Subscription, RuntimeError> {
        let options = SubscriptionOptions::new(QoS::AtLeastOnce, true, false, 0)
            .map_err(RuntimeError::InvalidConfig)?;
        Ok(Subscription {
            filter: self.topics.input.clone(),
            options,
        })
    }

    /// 发送 SUBSCRIBE；本地发送失败时记录并安排退避重试（K5）。
    fn try_subscribe(&mut self, subscription: Subscription, now: u64) {
        match self
            .client
            .subscribe(vec![subscription], Properties::new(), now)
        {
            Ok(_) => {
                self.subscribe_retry_at = None;
            }
            Err(_) => self.record_subscribe_failure(None, now),
        }
    }

    /// 记录一次订阅失败（SUBACK 错误码或本地发送错误）并安排下次退避重试。
    fn record_subscribe_failure(&mut self, error: Option<ReasonCode>, now: u64) {
        self.metrics.subscribe_attempts = self.metrics.subscribe_attempts.saturating_add(1);
        if let Some(code) = error {
            self.metrics.last_suback_error = Some(code);
        }
        let delay = subscribe_backoff_ms(self.metrics.subscribe_attempts);
        self.subscribe_retry_at = Some(now.saturating_add(delay));
    }

    /// 到达退避时刻后重订阅（只在已连接状态尝试）。
    fn retry_subscribe_if_due(&mut self, now: u64) {
        if self.subscribed || self.client.state() != State::Connected {
            return;
        }
        let Some(retry_at) = self.subscribe_retry_at else {
            return;
        };
        if now < retry_at {
            return;
        }
        if let Ok(subscription) = self.input_subscription() {
            self.try_subscribe(subscription, now);
        }
    }

    /// 处理 SUBACK：记录错误码并安排退避重订阅（K5）。
    ///
    /// 之前只把 `subscribed` 设为「任一 non-error」，订阅被拒后错误码被吞掉，
    /// 只能等下一次 CONNECT 重试。现在错误码进 `status().subscribe_error`
    /// 并在 `(attempts)` 退避后重发 SUBSCRIBE。
    fn on_suback(&mut self, suback: &Suback, now: u64) {
        let error = suback
            .reason_codes
            .iter()
            .copied()
            .find(|code| code.is_error());
        self.subscribed = error.is_none();
        match error {
            Some(code) => self.record_subscribe_failure(Some(code), now),
            None => {
                self.metrics.last_suback_error = None;
                self.subscribe_retry_at = None;
            }
        }
    }

    fn on_disconnected(&mut self, disconnect: Disconnect) -> Result<(), RuntimeError> {
        if !matches!(
            disconnect.reason_code,
            ReasonCode::UseAnotherServer | ReasonCode::ServerMoved
        ) {
            return Ok(());
        }
        if !matches!(self.jump, JumpState::Idle) {
            return Ok(());
        }
        if self.metrics.consecutive_moves >= self.config.max_consecutive_server_moves {
            return Ok(());
        }
        let Some(reference) = disconnect.properties.server_reference.as_deref() else {
            return Ok(());
        };
        let target =
            match Endpoint::parse(reference).or_else(|_| self.workers.resolve(reference, true)) {
                Ok(target) => target,
                Err(_) => return Ok(()),
            };
        let Some(previous) = self
            .link
            .current
            .clone()
            .or_else(|| self.link.last_good.clone())
        else {
            return Ok(());
        };
        if target == previous {
            return Ok(());
        }
        self.metrics.consecutive_moves = self.metrics.consecutive_moves.saturating_add(1);
        self.jump = JumpState::Switching {
            cmd_id: 0,
            target: target.clone(),
            previous,
        };
        self.expect_link_close = true;
        self.link_actions.push_back(LinkAction::Switch {
            endpoint: target,
            reason: SwitchReason::ServerMoved,
        });
        Ok(())
    }

    fn on_publish(&mut self, publish: Publish) -> Result<(), RuntimeError> {
        if publish.topic.as_ref().map(TopicName::as_str) != Some(self.topics.input.as_str()) {
            return Ok(());
        }
        if publish.retain {
            // `in` 主题禁止 retained；否则Worker重连/重新订阅会重复执行旧命令。
            self.metrics.rejected_commands = self.metrics.rejected_commands.saturating_add(1);
            return Ok(());
        }
        let now = self.clock.monotonic_ms();
        let stdin = match Stdin::decode(&publish.payload) {
            Ok(stdin) => stdin,
            Err(error) => {
                self.metrics.rejected_commands = self.metrics.rejected_commands.saturating_add(1);
                if let Some(id) = error.cmd_id.filter(|id| *id != 0) {
                    self.fail_parse(id, ErrorCode::InvalidCommand, error.error.to_string())?;
                }
                return Ok(());
            }
        };
        if let Some(content_type) = publish.properties.content_type.as_deref()
            && content_type != CONTENT_TYPE_STDIN
        {
            self.metrics.rejected_commands = self.metrics.rejected_commands.saturating_add(1);
            self.fail_parse(stdin.id, ErrorCode::InvalidCommand, "content_type 不匹配")?;
            return Ok(());
        }
        let kind = match stdin.kind {
            Kind::Task => match TaskBody::decode(&stdin.payload) {
                Ok(body) => CommandKind::Task {
                    name: body.name,
                    args: body.args,
                },
                Err(error) => {
                    self.metrics.rejected_commands =
                        self.metrics.rejected_commands.saturating_add(1);
                    self.fail_parse(stdin.id, ErrorCode::InvalidArgument, error.to_string())?;
                    return Ok(());
                }
            },
            Kind::ToolCall => match ToolBody::decode(&stdin.payload) {
                Ok(body) => CommandKind::Tool {
                    name: body.name,
                    args: body.args,
                },
                Err(error) => {
                    self.metrics.rejected_commands =
                        self.metrics.rejected_commands.saturating_add(1);
                    self.fail_parse(stdin.id, ErrorCode::InvalidArgument, error.to_string())?;
                    return Ok(());
                }
            },
            Kind::Jump => match JumpBody::decode(&stdin.payload) {
                Ok(body) => CommandKind::Jump {
                    target: body.target,
                },
                Err(error) => {
                    self.metrics.rejected_commands =
                        self.metrics.rejected_commands.saturating_add(1);
                    self.fail_parse(stdin.id, ErrorCode::InvalidArgument, error.to_string())?;
                    return Ok(());
                }
            },
        };
        let command = Command {
            id: stdin.id,
            kind,
            received_at: now,
            delay_ms: stdin.delay_ms,
            ddl_ms: stdin.ddl_ms,
            timestamp: stdin.timestamp,
        };
        if self.jump.is_active() {
            // K3：Jump 期间不接受新命令，但**不能静默丢弃**——立即回一帧
            // `Failed(Busy)` 并记住该 id，Router 才能明确知道命令未被受理。
            // （旧行为是只计内部拒绝计数、不回包，审计 K3 已指出其后果。）
            // 走 `fail_parse` 而不是直接 `remember_fail`：若 id 恰是正在
            // Jump 的命令（重复投递），按去重规则忽略，避免同一命令出现
            // 两个最终事件。
            self.metrics.rejected_commands = self.metrics.rejected_commands.saturating_add(1);
            return self.fail_parse(command.id, ErrorCode::Busy, "跳转进行中，命令未被受理");
        }
        self.push_inbound(command)
    }

    fn push_inbound(&mut self, command: Command) -> Result<(), RuntimeError> {
        if self.inbox.len() >= self.config.max_inbox {
            self.metrics.rejected_commands = self.metrics.rejected_commands.saturating_add(1);
            self.fail_parse(command.id, ErrorCode::Busy, "收件箱已满")?;
            return Ok(());
        }
        self.inbox.push_back(command);
        Ok(())
    }

    // ---- 命令处理 ----

    fn process_inbox(&mut self, now: u64) -> Result<(), RuntimeError> {
        let count = self.inbox.len().min(self.config.max_inbox);
        for _ in 0..count {
            let Some(command) = self.inbox.pop_front() else {
                break;
            };
            self.handle_command(command, now)?;
        }
        Ok(())
    }

    fn handle_command(&mut self, mut command: Command, now: u64) -> Result<(), RuntimeError> {
        let id = command.id;
        if self.dedup.contains(id) || self.is_active(id) || self.dedup.final_for(id).is_some() {
            return self.replay_or_ignore(id);
        }

        if let (Some(max_age), Some(now_unix)) =
            (self.config.max_command_age_ms, self.clock.unix_ms())
            && command.timestamp != 0
        {
            if command.timestamp > now_unix.saturating_add(300_000) {
                return self.remember_fail(id, ErrorCode::InvalidCommand, "时间戳在未来");
            }
            if now_unix.saturating_sub(command.timestamp) > max_age {
                return self.remember_fail(id, ErrorCode::Expired, "命令已过期");
            }
        }

        let ddl = if command.ddl_ms == 0 {
            self.config.default_ddl_ms
        } else {
            command.ddl_ms
        };
        if ddl > self.config.max_ddl_ms {
            return self.remember_fail(id, ErrorCode::InvalidArgument, "ddl 超过上限");
        }
        command.ddl_ms = ddl;
        if command.delay_ms > self.config.max_delay_ms {
            return self.remember_fail(id, ErrorCode::InvalidArgument, "delay 超过上限");
        }

        match &command.kind {
            CommandKind::Task { name, .. } => {
                if !self.tasks.contains(name) {
                    return self.remember_fail(id, ErrorCode::UnknownTask, "任务未注册");
                }
            }
            CommandKind::Tool { name, .. } => {
                if !self.tool_exists(name) {
                    return self.remember_fail(id, ErrorCode::UnknownTool, "工具未注册");
                }
            }
            CommandKind::Jump { target } => {
                let jump_pending = !self.jump.is_idle()
                    || self.scheduled.iter().any(|scheduled| {
                        matches!(&scheduled.command.kind, CommandKind::Jump { .. })
                    });
                if jump_pending {
                    return self.remember_fail(id, ErrorCode::Busy, "已有跳转进行中");
                }
                let Some(current) = self
                    .link
                    .current
                    .clone()
                    .or_else(|| self.link.last_good.clone())
                else {
                    return self.remember_fail(id, ErrorCode::Busy, "当前没有可用链路");
                };
                match self
                    .workers
                    .resolve(target, self.config.allow_arbitrary_jump_targets)
                {
                    Ok(endpoint) if endpoint == current => {
                        return self.remember_fail(
                            id,
                            ErrorCode::JumpInvalidTarget,
                            "目标与当前链路相同",
                        );
                    }
                    Ok(_) => {}
                    Err(_) => {
                        return self.remember_fail(
                            id,
                            ErrorCode::JumpInvalidTarget,
                            "跳转目标非法",
                        );
                    }
                }
            }
        }

        if self.scheduled.len() >= self.config.max_scheduled_tasks {
            return self.remember_fail(id, ErrorCode::Busy, "等待队列已满");
        }

        self.dedup.remember(id);
        self.metrics.consecutive_moves = 0;
        let start_at = command
            .received_at
            .saturating_add(u64::from(command.delay_ms));
        self.scheduled.push(Scheduled { command, start_at });
        self.scheduled
            .sort_by_key(|scheduled| (scheduled.start_at, scheduled.command.id));
        self.emit(Phase::Accepted, id, Vec::new())?;
        // 如果已经到期，下一次 start_due 会启动；这里不直接执行，保证事件顺序。
        let _ = now;
        Ok(())
    }

    fn replay_or_ignore(&mut self, id: u64) -> Result<(), RuntimeError> {
        if self.is_active(id) {
            self.metrics.rejected_commands = self.metrics.rejected_commands.saturating_add(1);
            return Ok(());
        }
        if let Some(final_stdout) = self.dedup.final_for(id).cloned() {
            self.enqueue_event(final_stdout);
        } else {
            self.metrics.rejected_commands = self.metrics.rejected_commands.saturating_add(1);
        }
        Ok(())
    }

    fn is_active(&self, id: u64) -> bool {
        self.scheduled
            .iter()
            .any(|scheduled| scheduled.command.id == id)
            || self.running.iter().any(|running| running.id == id)
            || match &self.jump {
                JumpState::Draining { cmd_id, .. }
                | JumpState::Switching { cmd_id, .. }
                | JumpState::FallingBack { cmd_id, .. } => *cmd_id == id,
                JumpState::Idle => false,
            }
    }

    fn remember_fail(
        &mut self,
        id: u64,
        code: ErrorCode,
        message: impl Into<String>,
    ) -> Result<(), RuntimeError> {
        self.dedup.remember(id);
        self.emit_failed(id, code, message)
    }

    /// 解析阶段的失败：如果 id 已被处理过，按去重规则重放/忽略，避免同一 id 产生两个最终事件。
    fn fail_parse(
        &mut self,
        id: u64,
        code: ErrorCode,
        message: impl Into<String>,
    ) -> Result<(), RuntimeError> {
        if self.dedup.contains(id) || self.is_active(id) || self.dedup.final_for(id).is_some() {
            return self.replay_or_ignore(id);
        }
        self.remember_fail(id, code, message)
    }

    fn tool_exists(&self, name: &str) -> bool {
        is_builtin_tool(name) || self.tools.contains(name)
    }

    // ---- 调度与执行 ----

    fn start_due(&mut self, now: u64) -> Result<(), RuntimeError> {
        while let Some((start_at, is_task)) = self.scheduled.first().map(|scheduled| {
            (
                scheduled.start_at,
                matches!(&scheduled.command.kind, CommandKind::Task { .. }),
            )
        }) {
            if start_at > now {
                break;
            }
            if is_task && self.running.len() >= self.config.max_running_tasks {
                break;
            }
            let scheduled = self.scheduled.remove(0);
            self.start_command(scheduled.command, now)?;
        }
        Ok(())
    }

    fn start_command(&mut self, command: Command, now: u64) -> Result<(), RuntimeError> {
        let Command {
            id, kind, ddl_ms, ..
        } = command;
        match kind {
            CommandKind::Task { name, args } => {
                let handler_result = match self.tasks.get(&name) {
                    Some(factory) => factory(&args),
                    None => Err(CommandError::from(ErrorCode::UnknownTask)),
                };
                let handler = match handler_result {
                    Ok(handler) => handler,
                    Err(error) => {
                        return self.emit_failed(id, error.code, error.message);
                    }
                };
                self.emit(Phase::Started, id, Vec::new())?;
                self.running.push(Running {
                    id,
                    name,
                    started_at: now,
                    next_poll_at: now,
                    ddl_ms,
                    handler,
                });
                Ok(())
            }
            CommandKind::Tool { name, args } => {
                let now_unix = self.clock.unix_ms();
                let result = if is_builtin_tool(&name) {
                    self.handle_builtin(&name, &args, now)
                        .unwrap_or_else(|| Err(CommandError::from(ErrorCode::UnknownTool)))
                } else {
                    let mut context = ToolContext {
                        args: &args,
                        now_mono_ms: now,
                        now_unix_ms: now_unix,
                        workers: &self.workers,
                    };
                    match self.tools.get_mut(&name) {
                        Some(tool) => tool.call(&mut context),
                        None => Err(CommandError::from(ErrorCode::UnknownTool)),
                    }
                };
                let elapsed = self.clock.monotonic_ms().saturating_sub(now);
                if elapsed >= u64::from(ddl_ms) {
                    return self.emit_failed(id, ErrorCode::Timeout, "工具执行超时");
                }
                match result {
                    Ok(payload) => {
                        if payload.len() > MAX_PAYLOAD {
                            self.emit_failed(id, ErrorCode::ResultTooLarge, "结果过大")
                        } else {
                            self.emit(Phase::Completed, id, payload)
                        }
                    }
                    Err(error) => self.emit_failed(id, error.code, error.message),
                }
            }
            CommandKind::Jump { target } => {
                if !self.jump.is_idle() {
                    return self.emit_failed(id, ErrorCode::Busy, "已有跳转进行中");
                }
                let Some(previous) = self
                    .link
                    .current
                    .clone()
                    .or_else(|| self.link.last_good.clone())
                else {
                    return self.emit_failed(id, ErrorCode::Busy, "当前没有可用链路");
                };
                let endpoint = match self
                    .workers
                    .resolve(&target, self.config.allow_arbitrary_jump_targets)
                {
                    Ok(endpoint) => endpoint,
                    Err(_) => {
                        return self.emit_failed(id, ErrorCode::JumpInvalidTarget, "跳转目标非法");
                    }
                };
                if endpoint == previous {
                    return self.emit_failed(
                        id,
                        ErrorCode::JumpInvalidTarget,
                        "目标与当前链路相同",
                    );
                }
                self.begin_jump(id, endpoint, previous, now)
            }
        }
    }

    fn step_tasks(&mut self, now: u64) -> Result<(), RuntimeError> {
        let now_unix = self.clock.unix_ms();
        let mut index = 0;
        while index < self.running.len() {
            if self.running[index].next_poll_at > now {
                index += 1;
                continue;
            }
            let deadline = self.running[index]
                .started_at
                .saturating_add(u64::from(self.running[index].ddl_ms));
            if now >= deadline {
                let running = self.running.remove(index);
                self.emit_failed(running.id, ErrorCode::Timeout, "任务执行超时")?;
                continue;
            }
            let outcome = {
                let running = &mut self.running[index];
                let workers = &self.workers;
                let mut context = TaskContext {
                    now_mono_ms: now,
                    deadline_mono_ms: deadline,
                    now_unix_ms: now_unix,
                    workers,
                };
                running.handler.poll(&mut context)
            };
            let after = self.clock.monotonic_ms();
            if after >= deadline {
                let running = self.running.remove(index);
                self.emit_failed(running.id, ErrorCode::Timeout, "任务执行超时")?;
                continue;
            }
            match outcome {
                TaskPoll::Pending { wake_after_ms } => {
                    let remaining = deadline.saturating_sub(after).min(u64::from(u32::MAX)) as u32;
                    let wake = wake_after_ms.min(remaining).max(1);
                    self.running[index].next_poll_at = after.saturating_add(u64::from(wake));
                    index += 1;
                }
                TaskPoll::Done(payload) => {
                    let running = self.running.remove(index);
                    if payload.len() > MAX_PAYLOAD {
                        self.emit_failed(running.id, ErrorCode::ResultTooLarge, "任务结果过大")?;
                    } else {
                        self.emit(Phase::Completed, running.id, payload)?;
                    }
                }
                TaskPoll::Failed(error) => {
                    let running = self.running.remove(index);
                    self.emit_failed(running.id, error.code, error.message)?;
                }
            }
        }
        Ok(())
    }

    // ---- 内置工具 ----

    fn handle_builtin(
        &mut self,
        name: &str,
        args: &[u8],
        now: u64,
    ) -> Option<Result<Vec<u8>, CommandError>> {
        match name {
            "sekai.time" => {
                let mut out = Vec::with_capacity(16);
                out.extend_from_slice(&self.clock.unix_ms().unwrap_or(0).to_be_bytes());
                out.extend_from_slice(&now.to_be_bytes());
                Some(Ok(out))
            }
            "sekai.set_time" => {
                if args.len() != 8 {
                    return Some(Err(CommandError::new(
                        ErrorCode::InvalidArgument,
                        "参数必须为 8 字节 Unix 毫秒",
                    )));
                }
                let mut bytes = [0u8; 8];
                bytes.copy_from_slice(args);
                let value = u64::from_be_bytes(bytes);
                if !(1_600_000_000_000..=4_000_000_000_000).contains(&value) {
                    return Some(Err(CommandError::new(
                        ErrorCode::InvalidArgument,
                        "时间超出允许范围",
                    )));
                }
                let previous = self.clock.unix_ms().unwrap_or(0);
                // K8：实现 `Clock::calibrate_unix_ms` 契约里的「运行时限频」，
                // 避免同一批命令把墙上时间反复回拨/前跳。
                if let Some(last) = self.last_calibration
                    && now.saturating_sub(last) < MIN_CALIBRATION_INTERVAL_MS
                {
                    return Some(Err(CommandError::new(
                        ErrorCode::InvalidArgument,
                        "校准过于频繁：两次 sekai.set_time 至少间隔 1 秒",
                    )));
                }
                self.last_calibration = Some(now);
                self.clock.calibrate_unix_ms(value);
                Some(Ok(previous.to_be_bytes().to_vec()))
            }
            "sekai.info" => {
                let hello = match self.build_hello(now) {
                    Ok(hello) => hello,
                    Err(_) => {
                        return Some(Err(CommandError::from(ErrorCode::Internal)));
                    }
                };
                Some(
                    hello
                        .encode()
                        .map_err(|_| CommandError::from(ErrorCode::Internal)),
                )
            }
            "sekai.workers" => Some(
                self.workers
                    .encode()
                    .map_err(|_| CommandError::from(ErrorCode::Internal)),
            ),
            "sekai.workers.set" => {
                let list = match DeviceList::decode(args) {
                    Ok(list) => list,
                    Err(error) => {
                        return Some(Err(CommandError::new(
                            ErrorCode::InvalidArgument,
                            error.to_string(),
                        )));
                    }
                };
                self.workers = list;
                match self.emit_hello(now) {
                    Ok(()) => Some(Ok(Vec::new())),
                    Err(_) => Some(Err(CommandError::from(ErrorCode::Internal))),
                }
            }
            "sekai.cancel" => {
                if args.len() != 8 {
                    return Some(Err(CommandError::new(
                        ErrorCode::InvalidArgument,
                        "参数必须为 8 字节命令 id",
                    )));
                }
                let mut bytes = [0u8; 8];
                bytes.copy_from_slice(args);
                let id = u64::from_be_bytes(bytes);
                if let Some(index) = self
                    .scheduled
                    .iter()
                    .position(|scheduled| scheduled.command.id == id)
                {
                    self.scheduled.remove(index);
                    return Some(
                        match self.emit_failed(id, ErrorCode::Cancelled, "命令已取消") {
                            Ok(()) => Ok(Vec::new()),
                            Err(_) => Err(CommandError::from(ErrorCode::Internal)),
                        },
                    );
                }
                if let Some(index) = self.running.iter().position(|running| running.id == id) {
                    self.running.remove(index);
                    return Some(
                        match self.emit_failed(id, ErrorCode::Cancelled, "命令已取消") {
                            Ok(()) => Ok(Vec::new()),
                            Err(_) => Err(CommandError::from(ErrorCode::Internal)),
                        },
                    );
                }
                Some(Err(CommandError::new(
                    ErrorCode::InvalidArgument,
                    "命令不存在或已完成",
                )))
            }
            _ => None,
        }
    }

    // ---- Jump ----

    fn begin_jump(
        &mut self,
        cmd_id: u64,
        target: Endpoint,
        previous: Endpoint,
        now: u64,
    ) -> Result<(), RuntimeError> {
        self.jump = JumpState::Draining {
            cmd_id,
            target,
            previous,
            until: now.saturating_add(self.config.jump_drain_ms),
        };
        if self.client.state() != State::Connected {
            return self.begin_disconnect(now);
        }
        self.emit(Phase::Offline, 0, Vec::new())?;
        self.advance_jump(now)
    }

    fn advance_jump(&mut self, now: u64) -> Result<(), RuntimeError> {
        match self.jump.phase() {
            Some(JumpPhase::Draining) => {
                let until = self.jump.until().unwrap_or(u64::MAX);
                let drained = self.outbox.entries.is_empty() && self.outbox.pending_acks.is_empty();
                if drained || now >= until {
                    self.begin_disconnect(now)?;
                }
            }
            // 回退重连也超时：结束 Jump 并补发失败终态（K3）。
            Some(JumpPhase::FallingBack) => {
                let until = self.jump.until().unwrap_or(u64::MAX);
                if now >= until {
                    let cmd_id = self.jump.cmd_id();
                    self.finish_fallback(cmd_id)?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn begin_disconnect(&mut self, now: u64) -> Result<(), RuntimeError> {
        let JumpState::Draining {
            cmd_id,
            target,
            previous,
            ..
        } = core::mem::replace(&mut self.jump, JumpState::Idle)
        else {
            return Ok(());
        };
        let mut properties = Properties::new();
        properties.session_expiry_interval = Some(0);
        let _ = self.client.disconnect(ReasonCode::Success, properties, now);
        self.jump = JumpState::Switching {
            cmd_id,
            target: target.clone(),
            previous,
        };
        self.expect_link_close = true;
        self.link_actions.push_back(LinkAction::Switch {
            endpoint: target,
            reason: SwitchReason::JumpCommand,
        });
        Ok(())
    }

    fn switch_from_draining(&mut self) -> Result<(), RuntimeError> {
        let JumpState::Draining {
            cmd_id,
            target,
            previous,
            ..
        } = core::mem::replace(&mut self.jump, JumpState::Idle)
        else {
            return Ok(());
        };
        self.jump = JumpState::Switching {
            cmd_id,
            target: target.clone(),
            previous,
        };
        self.link_actions.push_back(LinkAction::Switch {
            endpoint: target,
            reason: SwitchReason::JumpCommand,
        });
        Ok(())
    }

    /// 目标端点不可达：切回原端点，进入 `FallingBack` 等待重连（K3）。
    ///
    /// 失败帧**不在这里发**：等回退重连成功（`on_connected`）、再次失败
    /// （`link_failed`/`on_connection_refused`）或超时（`advance_jump`）时
    /// 才补发 `JumpUnreachable` 终态，保证一条命令至多一个最终事件，且
    /// 回退也失败时状态不会永远卡在 `FallingBack`。
    fn begin_fallback(&mut self, now: u64) -> Result<(), RuntimeError> {
        let JumpState::Switching {
            cmd_id,
            target,
            previous,
        } = core::mem::replace(&mut self.jump, JumpState::Idle)
        else {
            return Ok(());
        };
        self.jump = JumpState::FallingBack {
            cmd_id,
            failed: target,
            home: previous.clone(),
            until: now.saturating_add(self.config.jump_fallback_ms),
        };
        self.link_actions.push_back(LinkAction::Switch {
            endpoint: previous,
            reason: SwitchReason::Fallback,
        });
        Ok(())
    }

    /// 结束失败的 Jump 回退：清空状态，并为 Jump 命令补发失败终态（幂等）。
    ///
    /// 去重缓存里已有最终事件时不再补发（协议要求一条命令至多一个最终帧）。
    fn finish_fallback(&mut self, cmd_id: u64) -> Result<(), RuntimeError> {
        if !matches!(&self.jump, JumpState::FallingBack { .. }) {
            return Ok(());
        }
        self.jump = JumpState::Idle;
        if cmd_id != 0 && self.dedup.final_for(cmd_id).is_none() {
            self.emit_failed(cmd_id, ErrorCode::JumpUnreachable, "跳转目标不可达，已回退")?;
        }
        Ok(())
    }

    // ---- 出站事件 ----

    fn emit(&mut self, phase: Phase, cmd_id: u64, payload: Vec<u8>) -> Result<(), RuntimeError> {
        let seq = self.next_seq;
        let stdout = Stdout::new(
            self.config.boot_id,
            cmd_id,
            seq,
            phase,
            self.clock.unix_ms().unwrap_or(0),
            payload,
        )?;
        self.next_seq = if seq == u32::MAX { 1 } else { seq + 1 };
        if stdout.is_final() {
            self.dedup.remember_final(&stdout);
        }
        self.enqueue_event(stdout);
        Ok(())
    }

    /// 入发件箱并把淘汰/拒收计入 `dropped_events`（K7）。
    ///
    /// 以前 `emit`（以及重复命令重放）忽略 `Outbox::enqueue` 的返回值，
    /// 发件箱满时事件静默消失且没有计数；现在每一次淘汰或拒收都记到
    /// [`RuntimeStatus::dropped_events`]，供 Router/运维判断事件完整性。
    fn enqueue_event(&mut self, stdout: Stdout) {
        let outcome = self.outbox.enqueue(stdout);
        let rejected: u64 = if outcome.accepted { 0 } else { 1 };
        self.metrics.dropped_events = self
            .metrics
            .dropped_events
            .saturating_add(outcome.evicted)
            .saturating_add(rejected);
    }

    fn emit_failed(
        &mut self,
        cmd_id: u64,
        code: ErrorCode,
        message: impl Into<String>,
    ) -> Result<(), RuntimeError> {
        let error = ErrorPayload::new(code, message);
        let payload = error.encode().unwrap_or_else(|_| {
            let mut payload = Vec::new();
            payload.extend_from_slice(&code.as_u16().to_be_bytes());
            payload.extend_from_slice(&0u16.to_be_bytes());
            payload
        });
        self.emit(Phase::Failed, cmd_id, payload)
    }

    fn emit_hello(&mut self, now: u64) -> Result<(), RuntimeError> {
        let hello = self.build_hello(now)?;
        let payload = hello.encode()?;
        self.emit(Phase::Hello, 0, payload)
    }

    fn build_hello(&self, now: u64) -> Result<Hello, RuntimeError> {
        let tasks: Vec<String> = self.tasks.names().map(String::from).collect();
        let mut tools: Vec<String> = BUILTIN_TOOLS
            .iter()
            .map(|name| String::from(*name))
            .collect();
        tools.extend(self.tools.names().map(String::from));
        tools.sort();
        tools.dedup();
        if tools.len() > MAX_TOOLS {
            return Err(RuntimeError::InvalidConfig("工具数量超过 16"));
        }
        let active_tasks: Vec<ActiveTask> = self
            .running
            .iter()
            .take(MAX_ACTIVE_TASKS)
            .map(|running| ActiveTask {
                cmd_id: running.id,
                name: running.name.clone(),
                elapsed_ms: now.saturating_sub(running.started_at),
            })
            .collect();
        Ok(Hello {
            worker_id: self.config.worker_id.clone(),
            firmware: self.config.firmware.clone(),
            uptime_ms: now,
            unix_ms: self.clock.unix_ms().unwrap_or(0),
            tasks,
            tools,
            workers: self.workers.clone(),
            active_tasks,
        })
    }

    fn flush_outbox(&mut self, now: u64) -> Result<(), RuntimeError> {
        let dropped = {
            let Self {
                outbox,
                client,
                topics,
                ..
            } = self;
            outbox.flush(client, topics, now)
        };
        self.metrics.dropped_events = self.metrics.dropped_events.saturating_add(dropped);
        Ok(())
    }
}

fn fold_deadline(current: Option<u64>, candidate: u64) -> Option<u64> {
    Some(current.map_or(candidate, |value| value.min(candidate)))
}

/// 订阅失败的退避时长：1s 起步按 2 倍递增，30s 封顶。
fn subscribe_backoff_ms(attempts: u32) -> u64 {
    let shift = attempts.saturating_sub(1).min(5);
    SUBSCRIBE_RETRY_MIN_MS
        .saturating_mul(1u64 << shift)
        .min(SUBSCRIBE_RETRY_MAX_MS)
}

fn is_builtin_tool(name: &str) -> bool {
    BUILTIN_TOOLS.contains(&name)
}

fn stdout_properties() -> Properties {
    let mut properties = Properties::new();
    properties.content_type = Some(String::from(CONTENT_TYPE_STDOUT));
    properties
}

// ---- 内部类型 ----

#[derive(Debug, Clone)]
struct Topics {
    input: TopicFilter,
    output: TopicName,
    state: TopicName,
}

impl Topics {
    fn new(root: &str, worker_id: &str) -> Result<Self, RuntimeError> {
        let input =
            TopicFilter::new(format!("{root}/dev/{worker_id}/in")).map_err(RuntimeError::Topic)?;
        let output =
            TopicName::new(format!("{root}/dev/{worker_id}/out")).map_err(RuntimeError::Topic)?;
        let state =
            TopicName::new(format!("{root}/dev/{worker_id}/state")).map_err(RuntimeError::Topic)?;
        Ok(Self {
            input,
            output,
            state,
        })
    }
}

#[derive(Debug, Clone)]
struct Command {
    id: u64,
    kind: CommandKind,
    received_at: u64,
    delay_ms: u32,
    ddl_ms: u32,
    timestamp: u64,
}

#[derive(Debug, Clone)]
enum CommandKind {
    Task { name: String, args: Vec<u8> },
    Tool { name: String, args: Vec<u8> },
    Jump { target: String },
}

#[derive(Debug)]
struct Scheduled {
    command: Command,
    start_at: u64,
}

struct Running {
    id: u64,
    name: String,
    started_at: u64,
    next_poll_at: u64,
    ddl_ms: u32,
    handler: crate::sekai_task::BoxedTask,
}

#[derive(Debug, Default, Clone, Copy)]
struct Metrics {
    dropped_events: u64,
    rejected_commands: u64,
    consecutive_moves: u8,
    /// 订阅尝试总次数（含 SUBACK 失败与本地发送失败）。
    subscribe_attempts: u32,
    /// 最近一次 SUBACK 错误码（成功后清空）。
    last_suback_error: Option<ReasonCode>,
}

#[derive(Debug, Default, Clone)]
struct LinkState {
    current: Option<Endpoint>,
    last_good: Option<Endpoint>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum JumpState {
    Idle,
    Draining {
        cmd_id: u64,
        target: Endpoint,
        previous: Endpoint,
        until: u64,
    },
    Switching {
        cmd_id: u64,
        target: Endpoint,
        previous: Endpoint,
    },
    FallingBack {
        cmd_id: u64,
        failed: Endpoint,
        home: Endpoint,
        /// 回退期限（单调毫秒）。回退链路也不可达时必须在此之前给出终态。
        until: u64,
    },
}

impl JumpState {
    fn is_idle(&self) -> bool {
        matches!(self, Self::Idle)
    }

    fn is_active(&self) -> bool {
        !self.is_idle()
    }

    fn phase(&self) -> Option<JumpPhase> {
        match self {
            Self::Idle => None,
            Self::Draining { .. } => Some(JumpPhase::Draining),
            Self::Switching { .. } => Some(JumpPhase::Switching),
            Self::FallingBack { .. } => Some(JumpPhase::FallingBack),
        }
    }

    /// 当前 Jump 关联的命令 id；没有 Jump 时为 0。
    fn cmd_id(&self) -> u64 {
        match self {
            Self::Idle => 0,
            Self::Draining { cmd_id, .. }
            | Self::Switching { cmd_id, .. }
            | Self::FallingBack { cmd_id, .. } => *cmd_id,
        }
    }

    /// 当前阶段的截止时刻（`Draining` 排空上限 / `FallingBack` 回退上限）。
    fn until(&self) -> Option<u64> {
        match self {
            Self::Draining { until, .. } | Self::FallingBack { until, .. } => Some(*until),
            Self::Idle | Self::Switching { .. } => None,
        }
    }
}

#[derive(Debug)]
struct Dedup {
    recent: BTreeSet<u64>,
    order: VecDeque<u64>,
    finals: VecDeque<Stdout>,
    window: usize,
    finals_limit: usize,
    finals_bytes: usize,
}

impl Dedup {
    fn new(window: usize, finals_limit: usize) -> Self {
        Self {
            recent: BTreeSet::new(),
            order: VecDeque::new(),
            finals: VecDeque::new(),
            window,
            finals_limit,
            finals_bytes: 0,
        }
    }

    fn contains(&self, id: u64) -> bool {
        self.recent.contains(&id)
    }

    fn remember(&mut self, id: u64) {
        if !self.recent.insert(id) {
            return;
        }
        self.order.push_back(id);
        while self.order.len() > self.window {
            if let Some(old) = self.order.pop_front() {
                self.recent.remove(&old);
            }
        }
    }

    fn final_for(&self, id: u64) -> Option<&Stdout> {
        self.finals.iter().find(|stdout| stdout.cmd_id == id)
    }

    fn remember_final(&mut self, stdout: &Stdout) {
        if self
            .finals
            .iter()
            .any(|existing| existing.cmd_id == stdout.cmd_id)
        {
            return;
        }
        let len = stdout.payload.len();
        if len > FINAL_CACHE_BYTES {
            return;
        }
        while self.finals.len() >= self.finals_limit
            || self.finals_bytes.saturating_add(len) > FINAL_CACHE_BYTES
        {
            let Some(old) = self.finals.pop_front() else {
                break;
            };
            self.finals_bytes = self.finals_bytes.saturating_sub(old.payload.len());
        }
        self.finals_bytes = self.finals_bytes.saturating_add(len);
        self.finals.push_back(stdout.clone());
    }
}

#[derive(Debug)]
struct Outbox {
    entries: BTreeMap<u32, OutboxEntry>,
    queued: BTreeSet<u32>,
    pending_acks: BTreeMap<u16, u32>,
    /// 非最终事件的条目/字节占用。
    non_final_entries: usize,
    non_final_bytes: usize,
    /// 最终事件的条目/字节占用（独立预算，K7）。
    final_entries: usize,
    final_bytes: usize,
    limit_entries: usize,
    limit_bytes: usize,
    final_limit_entries: usize,
    final_limit_bytes: usize,
}

/// [`Outbox::enqueue`] 的结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct EnqueueOutcome {
    /// 事件是否真的进入了发件箱。
    accepted: bool,
    /// 为腾出预算而淘汰的事件数（这些事件会丢，计入 `dropped_events`）。
    evicted: u64,
}

#[derive(Debug)]
struct OutboxEntry {
    stdout: Stdout,
    target: Target,
    state: EntryState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EntryState {
    Queued,
    InFlight(u16),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Target {
    Output,
    State,
}

impl Target {
    fn for_phase(phase: Phase) -> Self {
        match phase {
            Phase::Hello | Phase::Offline => Self::State,
            _ => Self::Output,
        }
    }

    fn topic(self, topics: &Topics) -> &TopicName {
        match self {
            Self::Output => &topics.output,
            Self::State => &topics.state,
        }
    }
}

impl Outbox {
    fn new(
        limit_entries: usize,
        limit_bytes: usize,
        final_limit_entries: usize,
        final_limit_bytes: usize,
    ) -> Self {
        Self {
            entries: BTreeMap::new(),
            queued: BTreeSet::new(),
            pending_acks: BTreeMap::new(),
            non_final_entries: 0,
            non_final_bytes: 0,
            final_entries: 0,
            final_bytes: 0,
            limit_entries,
            limit_bytes,
            final_limit_entries,
            final_limit_bytes,
        }
    }

    /// 发件箱当前占用的载荷字节数（非最终 + 最终）。
    fn bytes_total(&self) -> usize {
        self.non_final_bytes.saturating_add(self.final_bytes)
    }

    /// 入队一个事件；必要时淘汰同预算内的旧事件。
    ///
    /// 预算分层（K7）：
    /// - 中间事件（含 Hello/Offline）用 `limit_entries/limit_bytes`；
    /// - 最终事件用 `final_limit_entries/final_limit_bytes`，两者互不挤占，
    ///   因此中间事件的洪峰不会把最终事件挤掉；
    /// - 只淘汰 `Queued` 条目：in-flight 条目正等 PUBACK，淘汰会破坏 QoS 记账；
    /// - 淘汰/拒收都会体现在返回值里，由调用方计入 `dropped_events`。
    fn enqueue(&mut self, stdout: Stdout) -> EnqueueOutcome {
        let seq = stdout.seq;
        if self.entries.contains_key(&seq) {
            return EnqueueOutcome {
                accepted: false,
                evicted: 0,
            };
        }
        let len = stdout.payload.len();
        let mut evicted = 0u64;
        if stdout.is_final() {
            while self.final_entries >= self.final_limit_entries
                || self.final_bytes.saturating_add(len) > self.final_limit_bytes
            {
                if !self.evict_queued_final() {
                    return EnqueueOutcome {
                        accepted: false,
                        evicted,
                    };
                }
                evicted = evicted.saturating_add(1);
            }
            self.final_entries = self.final_entries.saturating_add(1);
            self.final_bytes = self.final_bytes.saturating_add(len);
        } else {
            while self.non_final_entries >= self.limit_entries
                || self.non_final_bytes.saturating_add(len) > self.limit_bytes
            {
                if !self.evict_queued_non_final() {
                    return EnqueueOutcome {
                        accepted: false,
                        evicted,
                    };
                }
                evicted = evicted.saturating_add(1);
            }
            self.non_final_entries = self.non_final_entries.saturating_add(1);
            self.non_final_bytes = self.non_final_bytes.saturating_add(len);
        }
        let target = Target::for_phase(stdout.phase);
        self.entries.insert(
            seq,
            OutboxEntry {
                stdout,
                target,
                state: EntryState::Queued,
            },
        );
        self.queued.insert(seq);
        EnqueueOutcome {
            accepted: true,
            evicted,
        }
    }

    /// 淘汰一条排队中的中间事件。
    ///
    /// 优先级：非最终、非 Hello/Offline 的最老事件优先；其次淘汰旧
    /// Hello/Offline（下一次 Hello 或重连会重建它们）。**不会**淘汰最终事件
    /// 或 in-flight 事件。
    fn evict_queued_non_final(&mut self) -> bool {
        let victim = self
            .entries
            .iter()
            .find(|(_, entry)| {
                matches!(entry.state, EntryState::Queued)
                    && !entry.stdout.is_final()
                    && !matches!(entry.stdout.phase, Phase::Hello | Phase::Offline)
            })
            .map(|(seq, _)| *seq)
            .or_else(|| {
                self.entries
                    .iter()
                    .find(|(_, entry)| {
                        matches!(entry.state, EntryState::Queued) && !entry.stdout.is_final()
                    })
                    .map(|(seq, _)| *seq)
            });
        match victim {
            Some(seq) => self.remove(seq),
            None => false,
        }
    }

    /// 淘汰最老的排队最终事件（仅当最终事件自己的预算已满，K7）。
    fn evict_queued_final(&mut self) -> bool {
        let victim = self
            .entries
            .iter()
            .find(|(_, entry)| matches!(entry.state, EntryState::Queued) && entry.stdout.is_final())
            .map(|(seq, _)| *seq);
        match victim {
            Some(seq) => self.remove(seq),
            None => false,
        }
    }

    /// 从发件箱移除一个条目并更新预算计数。
    fn remove(&mut self, seq: u32) -> bool {
        let Some(entry) = self.entries.remove(&seq) else {
            return false;
        };
        self.account_removed(&entry);
        if let EntryState::InFlight(packet_id) = entry.state {
            self.pending_acks.remove(&packet_id);
        }
        self.queued.remove(&seq);
        true
    }

    /// 扣减已从 `entries` 取出的条目的预算计数（`remove` 与 `flush` 共用）。
    fn account_removed(&mut self, entry: &OutboxEntry) {
        let len = entry.stdout.payload.len();
        if entry.stdout.is_final() {
            self.final_entries = self.final_entries.saturating_sub(1);
            self.final_bytes = self.final_bytes.saturating_sub(len);
        } else {
            self.non_final_entries = self.non_final_entries.saturating_sub(1);
            self.non_final_bytes = self.non_final_bytes.saturating_sub(len);
        }
    }

    fn on_puback(&mut self, packet_id: u16) {
        let Some(seq) = self.pending_acks.remove(&packet_id) else {
            return;
        };
        self.remove(seq);
    }

    fn on_session_reset(&mut self, lost_packet_ids: &[u16]) {
        for packet_id in lost_packet_ids {
            let Some(seq) = self.pending_acks.remove(packet_id) else {
                continue;
            };
            if let Some(entry) = self.entries.get_mut(&seq) {
                entry.state = EntryState::Queued;
                self.queued.insert(seq);
            }
        }
    }

    fn flush(&mut self, client: &mut Client, topics: &Topics, now: u64) -> u64 {
        let mut dropped = 0u64;
        while let Some(seq) = self.queued.iter().next().copied() {
            self.queued.remove(&seq);
            let Some(mut entry) = self.entries.remove(&seq) else {
                continue;
            };
            if !matches!(entry.state, EntryState::Queued) {
                self.entries.insert(seq, entry);
                continue;
            }
            if client.state() != State::Connected {
                self.entries.insert(seq, entry);
                self.queued.insert(seq);
                break;
            }
            let topic = entry.target.topic(topics).clone();
            let retain = entry.target == Target::State;
            let frame = match entry.stdout.encode() {
                Ok(frame) => frame,
                Err(_) => {
                    self.account_removed(&entry);
                    dropped = dropped.saturating_add(1);
                    continue;
                }
            };
            let options = PublishOptions {
                qos: QoS::AtLeastOnce,
                retain,
                properties: stdout_properties(),
            };
            match client.publish(topic.clone(), frame, options, now) {
                Ok(Some(packet_id)) => {
                    entry.state = EntryState::InFlight(packet_id);
                    self.pending_acks.insert(packet_id, seq);
                    self.entries.insert(seq, entry);
                }
                Ok(None) => {
                    self.account_removed(&entry);
                }
                Err(ClientError::NotConnected) => {
                    self.entries.insert(seq, entry);
                    self.queued.insert(seq);
                    break;
                }
                Err(ClientError::RetainNotSupported) if retain => {
                    entry.target = Target::Output;
                    self.entries.insert(seq, entry);
                    self.queued.insert(seq);
                }
                Err(ClientError::QosNotSupported(_)) => {
                    let frame = match entry.stdout.encode() {
                        Ok(frame) => frame,
                        Err(_) => {
                            self.account_removed(&entry);
                            dropped = dropped.saturating_add(1);
                            continue;
                        }
                    };
                    let options = PublishOptions {
                        qos: QoS::AtMostOnce,
                        retain,
                        properties: stdout_properties(),
                    };
                    match client.publish(topic, frame, options, now) {
                        Ok(_) => {
                            self.account_removed(&entry);
                        }
                        Err(ClientError::NotConnected) => {
                            self.entries.insert(seq, entry);
                            self.queued.insert(seq);
                            break;
                        }
                        Err(_) => {
                            self.account_removed(&entry);
                            dropped = dropped.saturating_add(1);
                        }
                    }
                }
                Err(_) => {
                    self.account_removed(&entry);
                    dropped = dropped.saturating_add(1);
                }
            }
        }
        dropped
    }
}

#[cfg(test)]
mod tests {
    use super::{EnqueueOutcome, EntryState, Outbox, Phase, Stdout};
    use alloc::vec;
    use alloc::vec::Vec;

    /// 构造一个非最终事件（Accepted/Started/Hello/Offline 之一）。
    fn non_final(seq: u32, phase: Phase) -> Stdout {
        let cmd_id = if matches!(phase, Phase::Hello | Phase::Offline) {
            0
        } else {
            1
        };
        Stdout::new(7, cmd_id, seq, phase, 0, Vec::new()).unwrap()
    }

    /// 构造一个最终事件（Completed），载荷长度可调。
    fn final_event(seq: u32, payload_len: usize) -> Stdout {
        Stdout::new(
            7,
            seq as u64,
            seq,
            Phase::Completed,
            0,
            vec![0x5A; payload_len],
        )
        .unwrap()
    }

    #[test]
    fn outbox_keeps_final_events_under_non_final_pressure() {
        let mut outbox = Outbox::new(1, 64, 2, 256);
        // 中间事件预算 1 条：第二条淘汰第一条。
        assert_eq!(
            outbox.enqueue(non_final(1, Phase::Accepted)),
            EnqueueOutcome {
                accepted: true,
                evicted: 0
            }
        );
        let outcome = outbox.enqueue(non_final(2, Phase::Started));
        assert!(outcome.accepted);
        assert_eq!(outcome.evicted, 1);

        // 最终事件走独立预算：不会被中间事件的旧事件挤掉。
        let outcome = outbox.enqueue(final_event(3, 8));
        assert!(outcome.accepted);
        assert_eq!(outcome.evicted, 0);
        assert!(outbox.entries.contains_key(&3));

        // 再来中间事件只淘汰中间事件，最终事件仍在。
        let outcome = outbox.enqueue(non_final(4, Phase::Accepted));
        assert!(outcome.accepted);
        assert_eq!(outcome.evicted, 1);
        assert!(
            outbox.entries.contains_key(&3),
            "最终事件被中间事件的预算压力挤掉"
        );
        assert!(!outbox.entries.contains_key(&2));
        assert_eq!(outbox.final_entries, 1);
        assert_eq!(outbox.non_final_entries, 1);
    }

    #[test]
    fn outbox_evicts_oldest_queued_final_only_when_final_budget_full() {
        let mut outbox = Outbox::new(4, 1024, 1, 4096);
        assert!(outbox.enqueue(final_event(10, 4)).accepted);
        let outcome = outbox.enqueue(final_event(11, 4));
        assert!(outcome.accepted);
        assert_eq!(
            outcome.evicted, 1,
            "最终事件预算满时应淘汰最老的排队最终事件"
        );
        assert!(!outbox.entries.contains_key(&10));
        assert!(outbox.entries.contains_key(&11));
        assert_eq!(outbox.final_entries, 1);

        // 超出字节预算同样淘汰最老的排队最终事件。
        let mut outbox = Outbox::new(4, 1024, 4, 6);
        assert!(outbox.enqueue(final_event(20, 4)).accepted);
        let outcome = outbox.enqueue(final_event(21, 4));
        assert!(outcome.accepted);
        assert_eq!(outcome.evicted, 1);
        assert_eq!(outbox.final_bytes, 4);
    }

    #[test]
    fn outbox_does_not_evict_in_flight_final_for_new_final() {
        let mut outbox = Outbox::new(2, 64, 1, 4096);
        assert!(outbox.enqueue(final_event(30, 4)).accepted);
        // 模拟该最终事件已发出、等 PUBACK：in-flight 不可淘汰。
        let entry = outbox.entries.get_mut(&30).expect("最终事件应存在");
        entry.state = EntryState::InFlight(99);
        outbox.pending_acks.insert(99, 30);
        outbox.queued.remove(&30);

        let outcome = outbox.enqueue(final_event(31, 4));
        assert!(!outcome.accepted, "in-flight 最终事件不应被淘汰");
        assert_eq!(outcome.evicted, 0);
        assert!(outbox.entries.contains_key(&30));
        assert!(outbox.pending_acks.contains_key(&99));
    }
}
