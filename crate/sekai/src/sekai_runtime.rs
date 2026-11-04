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
use crate::mqtt::packet::{Connack, Disconnect, Packet, Publish, Suback, Will};
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
    /// Outbox 载荷字节上限。
    pub outbox_bytes: usize,
    /// 去重 id 窗口。
    pub dedup_window: usize,
    /// 最终事件缓存条数。
    pub final_cache: usize,
    /// Jump 排空发件箱的最长等待（毫秒）。
    pub jump_drain_ms: u64,
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
            clean_start: false,
            keep_alive: 30,
            connect_timeout: 5_000,
            session_expiry_interval: Some(300),
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
            dedup_window: 32,
            final_cache: 8,
            jump_drain_ms: 1_500,
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
            || self.dedup_window == 0
            || self.final_cache == 0
        {
            return Err(RuntimeError::InvalidConfig("运行时上限不能为 0"));
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
            mqtt.session_expiry_interval = Some(300);
        }
        if config.will_offline {
            let offline = Stdout::new(
                config.boot_id,
                0,
                0,
                Phase::Offline,
                clock.unix_ms().unwrap_or(0),
                Vec::new(),
            )?;
            mqtt.will = Some(Will {
                topic: topics.state.clone(),
                payload: offline.encode()?,
                qos: QoS::AtLeastOnce,
                retain: true,
                properties: stdout_properties(),
            });
        }
        let client = Client::new(mqtt);
        let outbox = Outbox::new(config.max_outbox, config.outbox_bytes);
        let dedup = Dedup::new(config.dedup_window, config.final_cache);
        Ok(Self {
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
            link: LinkState::default(),
            jump: JumpState::Idle,
            link_actions: VecDeque::new(),
            expect_link_close: false,
            metrics: Metrics::default(),
        })
    }

    // ---- 链路生命周期（start 层调用） ----

    /// TCP 连接建立后调用：记录端点并发起 MQTT CONNECT。
    pub fn link_up(&mut self, endpoint: Endpoint) -> Result<(), RuntimeError> {
        if self.client.state() != State::Disconnected {
            return Err(RuntimeError::InvalidConfig("链路已连接"));
        }
        self.link.current = Some(endpoint);
        let now = self.clock.monotonic_ms();
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

    /// TCP 连接失败（尚未发出 MQTT CONNECT）；如果目标是 Jump 目标则回退。
    pub fn link_failed(&mut self, endpoint: Endpoint) {
        if let JumpState::Switching { target, .. } = &self.jump
            && *target == endpoint
        {
            let now = self.clock.monotonic_ms();
            let _ = self.begin_fallback(now);
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
            if matches!(&self.jump, JumpState::Switching { .. }) {
                self.begin_fallback(now)?;
            } else {
                return Err(RuntimeError::Client(error));
            }
        }
        drain_result?;

        if !self.jump.is_active() {
            self.process_inbox(now)?;
        }
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
        if let JumpState::Draining { until, .. } = &self.jump {
            deadline = fold_deadline(deadline, *until);
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
            outbox_bytes: self.outbox.bytes,
            pending_acks: self.outbox.pending_acks.len(),
            subscribed: self.subscribed,
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
                Event::Suback(suback) => self.on_suback(&suback),
                Event::SessionReset { lost_packet_ids } => {
                    self.outbox.on_session_reset(&lost_packet_ids);
                }
                Event::Pubrec(_) | Event::Pubcomp(_) | Event::Unsuback(_) | Event::Auth(_) => {}
            }
        }
        Ok(())
    }

    fn on_connected(&mut self, connack: Connack, now: u64) -> Result<(), RuntimeError> {
        if let Some(current) = self.link.current.clone() {
            self.link.last_good = Some(current);
        }
        let was_jump = core::mem::replace(&mut self.jump, JumpState::Idle);
        let jump_completed = match was_jump {
            JumpState::Switching { cmd_id, target, .. } => Some((cmd_id, target)),
            _ => None,
        };
        if connack.session_present {
            self.subscribed = true;
        }
        if !self.subscribed {
            let options = SubscriptionOptions::new(QoS::AtLeastOnce, true, false, 0)
                .map_err(RuntimeError::InvalidConfig)?;
            let subscription = Subscription {
                filter: self.topics.input.clone(),
                options,
            };
            // 订阅失败（例如服务端拒绝）不致命；下一次 Connected 会重试。
            let _ = self
                .client
                .subscribe(vec![subscription], Properties::new(), now);
        }
        self.emit_hello(now)?;
        if let Some((cmd_id, target)) = jump_completed
            && cmd_id != 0
        {
            let payload = JumpOutcome::new(target).encode()?;
            self.emit(Phase::Completed, cmd_id, payload)?;
        }
        self.expect_link_close = false;
        Ok(())
    }

    fn on_connection_refused(&mut self, now: u64) -> Result<(), RuntimeError> {
        if matches!(&self.jump, JumpState::Switching { .. }) {
            self.begin_fallback(now)?;
        }
        Ok(())
    }

    fn on_suback(&mut self, suback: &Suback) {
        self.subscribed = suback.reason_codes.iter().any(|code| !code.is_error());
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
        if self.jump.is_active() {
            // Jump 期间不接受新命令，避免污染发件箱排空。
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
            self.outbox.enqueue(final_stdout);
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
        let should = match &self.jump {
            JumpState::Draining { until, .. } => {
                (self.outbox.entries.is_empty() && self.outbox.pending_acks.is_empty())
                    || now >= *until
            }
            _ => false,
        };
        if should {
            self.begin_disconnect(now)?;
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

    fn begin_fallback(&mut self, now: u64) -> Result<(), RuntimeError> {
        let JumpState::Switching {
            cmd_id,
            target,
            previous,
        } = core::mem::replace(&mut self.jump, JumpState::Idle)
        else {
            return Ok(());
        };
        if cmd_id != 0 {
            self.emit_failed(cmd_id, ErrorCode::JumpUnreachable, "跳转目标不可达，已回退")?;
        }
        self.jump = JumpState::FallingBack {
            cmd_id,
            failed: target,
            home: previous.clone(),
        };
        self.link_actions.push_back(LinkAction::Switch {
            endpoint: previous,
            reason: SwitchReason::Fallback,
        });
        let _ = now;
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
        self.outbox.enqueue(stdout);
        Ok(())
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
    bytes: usize,
    limit_entries: usize,
    limit_bytes: usize,
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
    fn new(limit_entries: usize, limit_bytes: usize) -> Self {
        Self {
            entries: BTreeMap::new(),
            queued: BTreeSet::new(),
            pending_acks: BTreeMap::new(),
            bytes: 0,
            limit_entries,
            limit_bytes,
        }
    }

    fn enqueue(&mut self, stdout: Stdout) -> bool {
        let seq = stdout.seq;
        if self.entries.contains_key(&seq) {
            return false;
        }
        let len = stdout.payload.len();
        while self.entries.len() >= self.limit_entries
            || self.bytes.saturating_add(len) > self.limit_bytes
        {
            if !self.evict_one() {
                return false;
            }
        }
        let target = Target::for_phase(stdout.phase);
        self.bytes = self.bytes.saturating_add(len);
        self.entries.insert(
            seq,
            OutboxEntry {
                stdout,
                target,
                state: EntryState::Queued,
            },
        );
        self.queued.insert(seq);
        true
    }

    fn evict_one(&mut self) -> bool {
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
                    .find(|(_, entry)| matches!(entry.state, EntryState::Queued))
                    .map(|(seq, _)| *seq)
            })
            .or_else(|| self.entries.keys().next().copied());
        match victim {
            Some(seq) => self.remove(seq),
            None => false,
        }
    }

    fn remove(&mut self, seq: u32) -> bool {
        let Some(entry) = self.entries.remove(&seq) else {
            return false;
        };
        self.bytes = self.bytes.saturating_sub(entry.stdout.payload.len());
        if let EntryState::InFlight(packet_id) = entry.state {
            self.pending_acks.remove(&packet_id);
        }
        self.queued.remove(&seq);
        true
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
            let payload_len = entry.stdout.payload.len();
            let topic = entry.target.topic(topics).clone();
            let retain = entry.target == Target::State;
            let frame = match entry.stdout.encode() {
                Ok(frame) => frame,
                Err(_) => {
                    self.bytes = self.bytes.saturating_sub(payload_len);
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
                    self.bytes = self.bytes.saturating_sub(payload_len);
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
                            self.bytes = self.bytes.saturating_sub(payload_len);
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
                            self.bytes = self.bytes.saturating_sub(payload_len);
                        }
                        Err(ClientError::NotConnected) => {
                            self.entries.insert(seq, entry);
                            self.queued.insert(seq);
                            break;
                        }
                        Err(_) => {
                            self.bytes = self.bytes.saturating_sub(payload_len);
                            dropped = dropped.saturating_add(1);
                        }
                    }
                }
                Err(_) => {
                    self.bytes = self.bytes.saturating_sub(payload_len);
                    dropped = dropped.saturating_add(1);
                }
            }
        }
        dropped
    }
}
