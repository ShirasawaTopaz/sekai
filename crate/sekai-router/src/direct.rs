//! Router 侧直连端点：把 [`ControlPlane`]（MQTT 客户端状态机）桥接到
//! PC Worker 的直连 [`Link`]（长度前缀 V1 帧），不经 MQTT broker。
//!
//! ## 原理（与 `crate/sekai-worker/src/host.rs` 完全对称）
//!
//! [`ControlPlane`] 的命令生命周期、Worker 注册表、`(boot_id, seq)` 去重、
//! 超时与事件回收全部长在 MQTT 客户端状态机内部。协议文档 §1.2 明确
//! 「MQTT 只是一种载体」；直连链路就是替代载体：本模块在 Router 进程内
//! 充当**最小虚拟 broker 适配器**，让 ControlPlane 原样运转，而链路上
//! 只出现 V1 Stdin/Stdout 帧（外加真正跨链路的 MQTT 保活报文）：
//!
//! - 下行（ControlPlane → 链路）：ControlPlane 出站的 PUBLISH（发往
//!   `{root}/dev/{worker_id}/in` 的 Stdin 帧）剥出载荷写链路，并就地
//!   PUBACK 确认；CONNECT/SUBSCRIBE 由适配器就地回 CONNACK/SUBACK，
//!   **PINGREQ 则作为保活报文真正写链路**（P10），由 Worker 侧回 PINGRESP；
//! - 上行（链路 → ControlPlane）：收到的 V1 Stdout 帧按阶段包装成
//!   `out`（命令事件）或 `state`（Hello/Offline，对应 retained 语义）
//!   主题的 QoS1 PUBLISH 喂给 `ControlPlane::feed`，Worker id 取自
//!   最近一次 Hello（首帧 Hello 自带 worker id）；收到的 PINGREQ 由适配器
//!   回 PINGRESP，收到的 PINGRESP 喂回 ControlPlane 刷新保活等待。
//!
//! 读空闲判定：超过 1.5 × `keep_alive` 没有收到任何链路帧（含保活报文）
//! 即视为对端静默死亡（拔网线/休眠），端点转 `link_lost` 并按既有退避
//! 语义等待 Worker 重连（P10）。
//!
//! ## 传输实现
//!
//! - [`StdioWorkerLink`]：子进程模式。Router 拉起 `sekai-worker
//!   --mode stdio`，其 stdin/stdout 即链路；
//! - [`AcceptedTcpLink`] / [`DirectListener`]：TCP listen 模式。Router
//!   监听端口接受 Worker 连接（Worker 侧用 `sekai_worker::TcpLink`
//!   主动连接）；
//! - 测试注入：`sekai_worker::memory_pair` 的一端 + [`DirectWorkerEndpoint`]。
//!
//! 读语义遵循 [`Link`] 约定：`Ok(None)` 对端有序关闭、`Err(Timeout)`
//! 空闲 tick（非致命）、其余错误视为链路故障（端点转 `link_lost`，
//! 在途命令按现有语义失败）。

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use sekai::mqtt::packet::{Ack, Connack, Packet, Publish, Suback};
use sekai::mqtt::property::Properties;
use sekai::mqtt::qos::QoS;
use sekai::mqtt::reason::ReasonCode;
use sekai::mqtt::topic::TopicName;
use sekai::sekai_device_list::Endpoint;
use sekai::sekai_runtime::CONTENT_TYPE_STDOUT;
use sekai::sekai_stdout::{Hello, Phase, Stdout};
use sekai_worker::link::{
    KeepAlive, Link, LinkError, MAX_FRAME_LEN, decode_keepalive, encode_keepalive,
    read_frame_stream, write_frame_stream,
};

use crate::control::{
    CommandRequest, ControlError, ControlEvent, ControlPlane, ControlPlaneConfig,
};
use crate::topic::TopicRoot;

/// 直连端点错误。
#[derive(Debug)]
pub enum DirectError {
    /// ControlPlane 构造/首泵失败。
    Plane(ControlError),
    /// 链路建立或首帧写出失败。
    Link(LinkError),
}

impl std::fmt::Display for DirectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Plane(error) => write!(f, "控制平面错误：{error}"),
            Self::Link(error) => write!(f, "直连链路错误：{error}"),
        }
    }
}

impl std::error::Error for DirectError {}

impl From<ControlError> for DirectError {
    fn from(error: ControlError) -> Self {
        Self::Plane(error)
    }
}

impl From<LinkError> for DirectError {
    fn from(error: LinkError) -> Self {
        Self::Link(error)
    }
}

/// Router 侧直连端点：`ControlPlane` + 直连 `Link` 的桥接。
///
/// 用法：
///
/// 1. 链路建立后 [`DirectWorkerEndpoint::new`]（内部完成虚拟
///    CONNECT/SUBSCRIBE 握手，返回时通常已 `is_ready`）；
/// 2. 主循环里周期调 [`DirectWorkerEndpoint::pump`]（读帧入站、写帧出站、
///    推进超时）；`Err(Timeout)` 的读当作一次 tick；
/// 3. 派发命令 [`DirectWorkerEndpoint::submit`]，结果经
///    [`DirectWorkerEndpoint::take_events`] 回收（与 MQTT 网关完全一致）。
pub struct DirectWorkerEndpoint<L: Link> {
    plane: ControlPlane,
    link: L,
    topic_root: TopicRoot,
    fallback_worker_id: String,
    seen_worker_id: Option<String>,
    next_packet_id: u16,
    link_open: bool,
    last_error: Option<String>,
    frames_rx: u64,
    frames_tx: u64,
    /// 最近一次收到完整链路帧的时刻（含保活报文）。
    last_rx: Instant,
    /// 读空闲判定阈值（1.5 × keep_alive；keep_alive=0 时关闭）。
    read_idle_timeout: Option<Duration>,
    /// 最近一次向链路写出保活 PINGREQ 的时刻（诊断/判定用，P10）。
    last_ping_sent: Option<Instant>,
}

impl<L: Link> DirectWorkerEndpoint<L> {
    /// 构造端点并完成虚拟握手（CONNECT→CONNACK、SUBSCRIBE→SUBACK）。
    ///
    /// `worker_id` 为端点预设 id：收到首帧 Hello 前入站非 Hello 帧按该 id
    /// 包装主题（正常流中 Worker 总是先发 Hello，此值仅兜底）。
    pub fn new(
        config: ControlPlaneConfig,
        link: L,
        worker_id: impl Into<String>,
        endpoint: Endpoint,
        now_unix_ms: u64,
    ) -> Result<Self, DirectError> {
        let topic_root = config.topic_root.clone();
        // 读空闲判定阈值与 MQTT keep_alive 对齐：1.5 倍（P10）。
        let read_idle_timeout = if config.keep_alive == 0 {
            None
        } else {
            Some(Duration::from_millis(u64::from(config.keep_alive) * 1_500))
        };
        let mut plane = ControlPlane::new(config)?;
        plane
            .link_up(endpoint, now_unix_ms)
            .map_err(DirectError::Plane)?;
        let mut endpoint = Self {
            plane,
            link,
            topic_root,
            fallback_worker_id: worker_id.into(),
            seen_worker_id: None,
            next_packet_id: 1,
            link_open: true,
            last_error: None,
            frames_rx: 0,
            frames_tx: 0,
            last_rx: Instant::now(),
            read_idle_timeout,
            last_ping_sent: None,
        };
        endpoint.pump_outgoing(now_unix_ms)?;
        Ok(endpoint)
    }

    /// 一次完整泵：链路帧入站 → ControlPlane 出站写链路 → 推进超时。
    ///
    /// 链路读写故障**不上抛**：端点转 `link_lost`（ControlPlane 按现有
    /// 语义失败在途命令并标记 Worker 离线），用
    /// [`DirectWorkerEndpoint::is_open`] / [`DirectWorkerEndpoint::last_error`]
    /// 观察。只有 ControlPlane 协议错误上抛 [`DirectError::Plane`]。
    ///
    /// ControlPlane 因 MQTT 保活超时（PINGRESP 未按时返回）或读空闲超过
    /// 1.5 × keep_alive 时，端点同样转 `link_lost`（P10）。
    pub fn pump(&mut self, now_unix_ms: u64, now_mono_ms: u64) -> Result<(), DirectError> {
        if self.link_open {
            self.pump_inbound(now_unix_ms)?;
        }
        self.pump_outgoing(now_unix_ms)?;
        let poll_result = self.plane.poll(now_unix_ms, now_mono_ms);
        // poll 可能吐出 PINGREQ 等出站报文，再消费一轮。
        self.pump_outgoing(now_unix_ms)?;
        if let Err(error) = poll_result {
            self.link_down(now_unix_ms, "控制平面协议错误");
            return Err(DirectError::Plane(error));
        }
        // ControlPlane 自己发现保活超时（PINGRESP 未回）时不会上抛错误，
        // 而是断开客户端状态：此时链路已不可信，按断链处理。
        if self.link_open && !self.plane.is_connected() {
            self.link_down(now_unix_ms, "MQTT 保活超时");
        }
        self.check_read_idle(now_unix_ms);
        Ok(())
    }

    /// 提交命令（透传 ControlPlane）并把 Stdin 帧立刻写到链路。
    pub fn submit(
        &mut self,
        request: CommandRequest,
        now_unix_ms: u64,
        now_mono_ms: u64,
    ) -> Result<u64, ControlError> {
        let cmd_id = self.plane.submit(request, now_unix_ms, now_mono_ms)?;
        let _ = self.pump_outgoing(now_unix_ms);
        Ok(cmd_id)
    }

    /// 提交管理命令（绕过每 Worker 在途上限），并把 Stdin 帧立刻写链路。
    ///
    /// `request.kind` 在管理命令语义下固定为 Tool（与
    /// [`ControlPlane::submit_control`] 一致），`ddl_ms`/`timeout_ms` 取默认值。
    pub fn submit_control(
        &mut self,
        request: CommandRequest,
        now_unix_ms: u64,
        now_mono_ms: u64,
    ) -> Result<u64, ControlError> {
        let cmd_id = self.plane.submit_control(
            &request.worker_id,
            &request.name,
            request.args,
            now_unix_ms,
            now_mono_ms,
        )?;
        let _ = self.pump_outgoing(now_unix_ms);
        Ok(cmd_id)
    }

    /// 取消一条在途命令（下发 Worker 侧 `sekai.cancel`）。
    ///
    /// 返回 `sekai.cancel` 自身的 cmd_id；原命令最终事件仍由 Worker 回传。
    pub fn cancel(
        &mut self,
        cmd_id: u64,
        now_unix_ms: u64,
        now_mono_ms: u64,
    ) -> Result<u64, ControlError> {
        let cancel_id = self.plane.cancel(cmd_id, now_unix_ms, now_mono_ms)?;
        let _ = self.pump_outgoing(now_unix_ms);
        Ok(cancel_id)
    }

    /// 订阅是否就绪（虚拟 SUBACK 已确认）。
    pub fn is_ready(&self) -> bool {
        self.plane.is_ready()
    }

    /// 直连链路是否仍打开（对端关闭/读写故障后为 false）。
    pub fn is_open(&self) -> bool {
        self.link_open
    }

    /// 最近一次链路/协议错误描述（诊断用）。
    pub fn last_error(&self) -> Option<&str> {
        self.last_error.as_deref().or(self.plane.last_error())
    }

    /// Worker 快照列表（ControlPlane 注册表）。
    pub fn workers(&self) -> Vec<crate::worker::WorkerInfo> {
        self.plane.workers()
    }

    /// 单个 Worker 快照。
    pub fn worker(&self, worker_id: &str) -> Option<crate::worker::WorkerInfo> {
        self.plane.worker(worker_id)
    }

    /// 取走累积事件（LinkState/WorkerUpdated/CommandFinished）。
    pub fn take_events(&mut self) -> Vec<ControlEvent> {
        self.plane.take_events()
    }

    /// 收到的链路帧数（诊断）。
    pub fn frames_rx(&self) -> u64 {
        self.frames_rx
    }

    /// 写出的链路帧数（诊断）。
    pub fn frames_tx(&self) -> u64 {
        self.frames_tx
    }

    /// 主动关闭：链路关闭 + ControlPlane link_closed（在途命令按 link_lost 失败）。
    pub fn close(&mut self, now_unix_ms: u64) {
        self.link_down(now_unix_ms, "端点主动关闭");
        let _ = self.plane.poll(now_unix_ms, now_unix_ms);
    }

    /// ControlPlane 只读访问（诊断/扩展）。
    pub fn plane(&self) -> &ControlPlane {
        &self.plane
    }

    // ---- 内部 ----

    /// 读链路直到空闲（Timeout）或关闭；帧包装为 PUBLISH 喂 ControlPlane。
    ///
    /// MQTT 保活报文（PINGREQ/PINGRESP）真正跨链路：入站 PINGREQ 由本端
    /// （虚拟 broker）回 PINGRESP，入站 PINGRESP 喂回 ControlPlane（P10）。
    fn pump_inbound(&mut self, now_unix_ms: u64) -> Result<(), DirectError> {
        loop {
            match self.link.read_frame() {
                Ok(Some(frame)) => {
                    self.frames_rx += 1;
                    self.last_rx = Instant::now();
                    if let Some(keepalive) = decode_keepalive(&frame) {
                        match keepalive {
                            KeepAlive::Pingreq => {
                                if !self.link_open {
                                    continue;
                                }
                                let pong = encode_keepalive(KeepAlive::Pingresp);
                                if let Err(error) = self.link.write_frame(&pong) {
                                    self.link_down(now_unix_ms, &format!("写链路失败：{error}"));
                                    return Ok(());
                                }
                                self.frames_tx += 1;
                            }
                            KeepAlive::Pingresp => {
                                if let Err(error) = self.plane.feed(Packet::Pingresp, now_unix_ms) {
                                    self.note(format!("ControlPlane 拒绝 PINGRESP：{error}"));
                                }
                            }
                        }
                        continue;
                    }
                    if let Err(error) = self.feed_frame(&frame, now_unix_ms) {
                        // ControlPlane 协议错误：其内部已按断链处理；记录并停止读。
                        self.note(format!("ControlPlane 拒绝入站帧：{error}"));
                        self.link_down(now_unix_ms, "协议错误");
                        return Ok(());
                    }
                }
                Ok(None) => {
                    self.link_down(now_unix_ms, "对端有序关闭");
                    return Ok(());
                }
                Err(LinkError::Timeout) => return Ok(()),
                Err(error) => {
                    self.link_down(now_unix_ms, &format!("链路故障：{error}"));
                    return Ok(());
                }
            }
        }
    }

    /// 把一帧 V1 Stdout 包装成 out/state 主题 PUBLISH 喂 ControlPlane。
    fn feed_frame(&mut self, frame: &[u8], now_unix_ms: u64) -> Result<(), ControlError> {
        let stdout = match Stdout::decode(frame) {
            Ok(stdout) => stdout,
            Err(error) => {
                // 非 Stdout 帧在直连承载下是协议违规：记录但不破坏链路。
                self.note(format!("忽略无法解码的链路帧：{error}"));
                return Ok(());
            }
        };
        let worker_id = match stdout.phase {
            Phase::Hello => match Hello::decode(&stdout.payload) {
                Ok(hello) => {
                    self.seen_worker_id = Some(hello.worker_id.clone());
                    hello.worker_id
                }
                Err(error) => {
                    self.note(format!("Hello 解码失败：{error}"));
                    return Ok(());
                }
            },
            _ => self
                .seen_worker_id
                .clone()
                .unwrap_or_else(|| self.fallback_worker_id.clone()),
        };
        let topic_text = match stdout.phase {
            Phase::Hello | Phase::Offline => self.topic_root.worker_state(&worker_id),
            _ => self.topic_root.worker_out(&worker_id),
        };
        let Ok(topic) = TopicName::new(topic_text) else {
            self.note(format!("入站主题非法（worker_id={worker_id}）"));
            return Ok(());
        };
        let mut properties = Properties::new();
        properties.content_type = Some(String::from(CONTENT_TYPE_STDOUT));
        let publish = Publish {
            dup: false,
            qos: QoS::AtLeastOnce,
            retain: false,
            topic: Some(topic),
            packet_id: Some(self.alloc_packet_id()),
            properties,
            payload: frame.to_vec(),
        };
        self.plane.feed(Packet::Publish(publish), now_unix_ms)
    }

    /// 消费 ControlPlane 全部出站报文：握手就地应答，Stdin 写链路。
    fn pump_outgoing(&mut self, now_unix_ms: u64) -> Result<(), DirectError> {
        while let Some(packet) = self.plane.next_outgoing(now_unix_ms) {
            match packet {
                // 虚拟 broker：总是新会话、授予 QoS1。
                Packet::Connect(_) => {
                    let connack = Connack {
                        session_present: false,
                        reason_code: ReasonCode::Success,
                        properties: Properties::new(),
                    };
                    if let Err(error) = self.plane.feed(Packet::Connack(connack), now_unix_ms) {
                        return Err(DirectError::Plane(error));
                    }
                }
                Packet::Subscribe(subscribe) => {
                    let reason_codes = subscribe
                        .subscriptions
                        .iter()
                        .map(|_| ReasonCode::GrantedQos1)
                        .collect();
                    let suback = Suback {
                        packet_id: subscribe.packet_id,
                        properties: Properties::new(),
                        reason_codes,
                    };
                    if let Err(error) = self.plane.feed(Packet::Suback(suback), now_unix_ms) {
                        return Err(DirectError::Plane(error));
                    }
                }
                // Stdin 帧写链路，并就地 PUBACK 清发件箱。
                Packet::Publish(publish) => {
                    if !self.link_open {
                        self.note(String::from("链路已关闭，丢弃出站 Stdin 帧"));
                        continue;
                    }
                    if let Err(error) = self.link.write_frame(&publish.payload) {
                        self.link_down(now_unix_ms, &format!("写链路失败：{error}"));
                        continue;
                    }
                    self.frames_tx += 1;
                    if let Some(packet_id) = publish.packet_id {
                        let puback = Ack {
                            packet_id,
                            reason_code: ReasonCode::Success,
                            properties: Properties::new(),
                        };
                        if let Err(error) = self.plane.feed(Packet::Puback(puback), now_unix_ms) {
                            return Err(DirectError::Plane(error));
                        }
                    }
                }
                // 保活：PINGREQ 真正跨链路，由 Worker 侧虚拟 broker 回 PINGRESP
                // （P10）；不再就地合成 PINGRESP。
                Packet::Pingreq => {
                    if !self.link_open {
                        self.note(String::from("链路已关闭，丢弃出站 PINGREQ"));
                        continue;
                    }
                    let ping = encode_keepalive(KeepAlive::Pingreq);
                    if let Err(error) = self.link.write_frame(&ping) {
                        self.link_down(now_unix_ms, &format!("写链路失败：{error}"));
                        continue;
                    }
                    self.frames_tx += 1;
                    self.last_ping_sent = Some(Instant::now());
                }
                // ControlPlane 是 MQTT 客户端，正常只发 PINGREQ；收到 PINGRESP
                // 已在入站方向处理，这里防御性忽略。
                Packet::Pingresp => {}
                // ControlPlane 主动断开（Jump/迁移，当前预留）：关闭链路。
                Packet::Disconnect(_) => {
                    self.link.close();
                    self.link_open = false;
                }
                // 我们注入的入站 PUBLISH 的 PUBACK 确认，无需外发；其余报文
                //（UNSUBSCRIBE/AUTH 等）在直连承载下不应出现，忽略。
                _ => {}
            }
        }
        Ok(())
    }

    /// 链路断开：关闭链路 + ControlPlane link_lost（失败在途命令、Worker 离线）。
    fn link_down(&mut self, now_unix_ms: u64, reason: &str) {
        if !self.link_open {
            return;
        }
        self.link.close();
        self.link_open = false;
        self.note(format!("直连链路断开：{reason}"));
        self.plane.link_lost(now_unix_ms);
    }

    fn note(&mut self, message: String) {
        self.last_error = Some(message);
    }

    /// 读空闲判定（P10）：超过 1.5 × keep_alive 没有入站帧即判死。
    ///
    /// 健康连接每 keep_alive 至少有一次保活报文跨链路，因此阈值留 0.5 倍余量；
    /// 只有「确实长时间没有任何入站报文」才触发，避免误杀正常空闲连接。
    fn check_read_idle(&mut self, now_unix_ms: u64) {
        let Some(timeout) = self.read_idle_timeout else {
            return;
        };
        if !self.link_open || self.last_rx.elapsed() < timeout {
            return;
        }
        let idle_ms = self.last_rx.elapsed().as_millis();
        let ping_note = match self.last_ping_sent {
            Some(at) => format!("，最近一次 PINGREQ 在 {}ms 前", at.elapsed().as_millis()),
            None => String::from("，期间未发出 PINGREQ"),
        };
        self.link_down(
            now_unix_ms,
            &format!("读空闲超时（{idle_ms}ms 未收到对端帧，保活超时{ping_note}）"),
        );
    }

    /// 分配合成 PUBLISH 的包 id（1..=65535 循环，跳过 0）。
    fn alloc_packet_id(&mut self) -> u16 {
        let id = self.next_packet_id;
        self.next_packet_id = if id == u16::MAX { 1 } else { id + 1 };
        id
    }
}

// ---- stdio 子进程链路 ----

/// 子进程模式链路：Router 拉起 Worker 进程，其 stdin/stdout 即直连链路。
///
/// 读侧由内部线程驱动（阻塞读 → channel），对外提供与
/// [`sekai_worker::link::MemoryLink`] 一致的语义：`Err(Timeout)` 空闲 tick、
/// `Ok(None)` 子进程退出（EOF）。`close` 终止子进程并回收线程。
pub struct StdioWorkerLink {
    child: Child,
    writer: Option<ChildStdin>,
    rx: Option<std::sync::mpsc::Receiver<Result<Vec<u8>, LinkError>>>,
    closed: bool,
    /// 读线程空闲轮询超时。
    poll_timeout: std::time::Duration,
}

impl StdioWorkerLink {
    /// 拉起 `program args...`（stdin/stdout 管道、stderr 继承）。
    pub fn spawn(program: &str, args: &[&str]) -> Result<Self, LinkError> {
        let mut command = Command::new(program);
        command.args(args);
        Self::spawn_command(command)
    }

    /// 用已配置的 [`Command`] 拉起（可注入工作目录/环境变量）。
    pub fn spawn_command(mut command: Command) -> Result<Self, LinkError> {
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        let mut child = command
            .spawn()
            .map_err(|error| LinkError::Connect(format!("拉起 Worker 失败：{error}")))?;
        let mut reader = child
            .stdout
            .take()
            .ok_or_else(|| LinkError::Connect(String::from("Worker 无 stdout 管道")))?;
        let writer = child
            .stdin
            .take()
            .ok_or_else(|| LinkError::Connect(String::from("Worker 无 stdin 管道")))?;
        // 读线程：阻塞读子进程 stdout，帧送 channel；EOF/错误作为终止消息。
        let (tx, rx) = std::sync::mpsc::channel::<Result<Vec<u8>, LinkError>>();
        thread::Builder::new()
            .name(String::from("sekai-stdio-reader"))
            .spawn(move || {
                loop {
                    match read_frame_stream(&mut reader) {
                        Ok(Some(frame)) => {
                            if tx.send(Ok(frame)).is_err() {
                                return;
                            }
                        }
                        Ok(None) => {
                            let _ = tx.send(Err(LinkError::NotConnected));
                            return;
                        }
                        Err(error) => {
                            let _ = tx.send(Err(error));
                            return;
                        }
                    }
                }
            })
            .map_err(|error| LinkError::Io(format!("读线程启动失败：{error}")))?;
        Ok(Self {
            child,
            writer: Some(writer),
            rx: Some(rx),
            closed: false,
            poll_timeout: std::time::Duration::from_millis(20),
        })
    }

    /// 子进程是否已退出（诊断；不 wait，不阻塞）。
    pub fn try_wait(&mut self) -> std::io::Result<Option<std::process::ExitStatus>> {
        self.child.try_wait()
    }
}

impl Link for StdioWorkerLink {
    fn read_frame(&mut self) -> Result<Option<Vec<u8>>, LinkError> {
        if self.closed {
            return Ok(None);
        }
        let Some(rx) = self.rx.as_ref() else {
            return Ok(None);
        };
        match rx.recv_timeout(self.poll_timeout) {
            Ok(Ok(frame)) => Ok(Some(frame)),
            Ok(Err(LinkError::NotConnected)) => {
                // 读线程报告子进程 EOF：此后 channel 断开，持续有序关闭。
                self.closed = true;
                self.rx = None;
                Ok(None)
            }
            Ok(Err(error)) => {
                self.closed = true;
                self.rx = None;
                Err(error)
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => Err(LinkError::Timeout),
            // channel 空且读线程已退出（close 杀进程后）：有序关闭。
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                self.closed = true;
                self.rx = None;
                Ok(None)
            }
        }
    }

    fn write_frame(&mut self, frame: &[u8]) -> Result<(), LinkError> {
        let Some(writer) = self.writer.as_mut() else {
            return Err(LinkError::NotConnected);
        };
        write_frame_stream(writer, frame)
    }

    fn close(&mut self) {
        self.closed = true;
        self.rx = None;
        self.writer = None;
        // 先杀子进程：读线程的阻塞 read 随管道关闭返回并退出。
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for StdioWorkerLink {
    fn drop(&mut self) {
        self.close();
    }
}

// ---- TCP listen 链路（服务端方向） ----

/// 已接受的 TCP 直连链路（Router 监听侧）。
///
/// 与 `sekai_worker::TcpLink`（客户端方向）同一分帧约定；读超时视为
/// 非致命 [`LinkError::Timeout`]（20ms 轮询节奏）。
pub struct AcceptedTcpLink {
    stream: Option<TcpStream>,
    decoder: sekai_worker::link::FrameDecoder,
}

impl AcceptedTcpLink {
    /// 包装一条已接受的连接。
    pub fn new(stream: TcpStream) -> Self {
        // 监听器可能处于 nonblocking 模式；部分平台 accept 出的流会继承
        // 该状态，这里显式回到阻塞 + 读超时驱动的分帧语义。
        let _ = stream.set_nonblocking(false);
        let _ = stream.set_nodelay(true);
        let _ = stream.set_read_timeout(Some(std::time::Duration::from_millis(20)));
        let _ = stream.set_write_timeout(Some(std::time::Duration::from_secs(5)));
        Self {
            stream: Some(stream),
            decoder: sekai_worker::link::FrameDecoder::new(MAX_FRAME_LEN),
        }
    }

    /// 是否仍连接（测试用）。
    pub fn is_connected(&self) -> bool {
        self.stream.is_some()
    }
}

impl Link for AcceptedTcpLink {
    fn read_frame(&mut self) -> Result<Option<Vec<u8>>, LinkError> {
        let Some(stream) = self.stream.as_mut() else {
            return Err(LinkError::NotConnected);
        };
        loop {
            if let Some(frame) = self.decoder.next_frame()? {
                return Ok(Some(frame));
            }
            let mut chunk = [0u8; 2048];
            match stream.read(&mut chunk) {
                Ok(0) => {
                    let partial = self.decoder.has_buffered();
                    self.close();
                    if partial {
                        return Err(LinkError::InvalidFrame(String::from("连接在帧中间关闭")));
                    }
                    return Ok(None);
                }
                Ok(n) => self.decoder.push(&chunk[..n])?,
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock
                            | std::io::ErrorKind::TimedOut
                            | std::io::ErrorKind::Interrupted
                    ) =>
                {
                    return Err(LinkError::Timeout);
                }
                Err(error) => {
                    self.close();
                    return Err(LinkError::Io(error.to_string()));
                }
            }
        }
    }

    fn write_frame(&mut self, frame: &[u8]) -> Result<(), LinkError> {
        let Some(stream) = self.stream.as_mut() else {
            return Err(LinkError::NotConnected);
        };
        let bytes = sekai_worker::link::encode_frame(frame)?;
        stream
            .write_all(&bytes)
            .and_then(|_| stream.flush())
            .map_err(|error| {
                self.close();
                LinkError::Io(error.to_string())
            })
    }

    fn close(&mut self) {
        self.stream = None;
        self.decoder = sekai_worker::link::FrameDecoder::new(MAX_FRAME_LEN);
    }
}

/// Router 侧直连监听器：接受 Worker 的 TCP 连接。
///
/// 每条连接对应一个 [`DirectWorkerEndpoint`]（各自独立的 ControlPlane
/// 注册表/命令空间）；多端点聚合属于 agent 层（P2）。
///
/// 监听器内部保持 **nonblocking** 模式：阻塞的 [`DirectListener::accept_link`]
/// 与限时的 [`DirectListener::accept_link_timeout`] 都以 20ms 步长轮询实现，
/// 两者可以安全交替使用（无模式切换竞态）。
pub struct DirectListener {
    listener: TcpListener,
}

/// accept 轮询步长。
const ACCEPT_POLL_MS: u64 = 20;

impl DirectListener {
    /// 绑定地址（`127.0.0.1:0` 自动分配）。
    pub fn bind(address: &str) -> std::io::Result<Self> {
        let listener = TcpListener::bind(address)?;
        listener.set_nonblocking(true)?;
        Ok(Self { listener })
    }

    /// 实际监听地址。
    pub fn local_addr(&self) -> std::io::Result<std::net::SocketAddr> {
        self.listener.local_addr()
    }

    /// 尝试接受一条连接；此刻无连接返回 `Ok(None)`。
    fn accept_once(&self) -> std::io::Result<Option<TcpStream>> {
        match self.listener.accept() {
            Ok((stream, _)) => Ok(Some(stream)),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// 阻塞接受一条连接，返回链路。
    pub fn accept_link(&self) -> std::io::Result<AcceptedTcpLink> {
        loop {
            if let Some(stream) = self.accept_once()? {
                return Ok(AcceptedTcpLink::new(stream));
            }
            thread::sleep(std::time::Duration::from_millis(ACCEPT_POLL_MS));
        }
    }

    /// 限时接受一条连接；超时无连接返回 `Ok(None)`。
    ///
    /// 供 DirectGateway 的 IO 线程在等待 Worker 连入的同时保持对
    /// Shutdown/Call 请求的响应。
    pub fn accept_link_timeout(
        &self,
        timeout: std::time::Duration,
    ) -> std::io::Result<Option<AcceptedTcpLink>> {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            if let Some(stream) = self.accept_once()? {
                return Ok(Some(AcceptedTcpLink::new(stream)));
            }
            if std::time::Instant::now() >= deadline {
                return Ok(None);
            }
            thread::sleep(std::time::Duration::from_millis(ACCEPT_POLL_MS));
        }
    }

    /// 阻塞接受一条连接并包装成直连端点。
    pub fn accept_endpoint(
        &self,
        config: ControlPlaneConfig,
        worker_id: impl Into<String>,
        endpoint: Endpoint,
        now_unix_ms: u64,
    ) -> Result<DirectWorkerEndpoint<AcceptedTcpLink>, DirectError> {
        let link = self
            .accept_link()
            .map_err(|error| LinkError::Connect(error.to_string()))?;
        DirectWorkerEndpoint::new(config, link, worker_id, endpoint, now_unix_ms)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    fn unix_ms() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_millis() as u64)
            .unwrap_or(0)
    }

    fn endpoint_config(worker_id: &str) -> ControlPlaneConfig {
        ControlPlaneConfig::new(
            format!("router-{worker_id}"),
            TopicRoot::new("sekai/v1").unwrap(),
        )
    }

    /// 直连端点在构造时完成虚拟握手：ready 且无链路帧产生。
    #[test]
    fn endpoint_completes_virtual_handshake() {
        let (router_link, worker_link) = sekai_worker::link::memory_pair();
        drop(worker_link);
        let endpoint = Endpoint::new("direct", 1).unwrap();
        let mut ep = DirectWorkerEndpoint::new(
            endpoint_config("hs"),
            router_link,
            "pc-1",
            endpoint,
            unix_ms(),
        )
        .unwrap();
        assert!(ep.is_ready());
        assert!(ep.is_open());
        assert_eq!(ep.frames_tx(), 0);
        ep.pump(unix_ms(), unix_ms()).unwrap();
        // 对端已 drop → 排空后有序关闭。
        assert!(!ep.is_open());
    }

    /// 入站 Hello 经注册表记录为在线 Worker（走 state 主题语义）。
    #[test]
    fn inbound_hello_registers_worker() {
        let (router_link, mut worker_link) = sekai_worker::link::memory_pair();
        let endpoint = Endpoint::new("direct", 1).unwrap();
        let mut ep = DirectWorkerEndpoint::new(
            endpoint_config("hello"),
            router_link,
            "fallback",
            endpoint,
            unix_ms(),
        )
        .unwrap();

        let hello = Hello {
            worker_id: String::from("pc-9"),
            firmware: String::from("t"),
            uptime_ms: 0,
            unix_ms: 0,
            tools: vec![String::from("gpio.info")],
            tasks: vec![String::from("echo")],
            workers: sekai::sekai_device_list::DeviceList::new(),
            active_tasks: Vec::new(),
        };
        let stdout = Stdout::new(7, 0, 1, Phase::Hello, 0, hello.encode().unwrap()).unwrap();
        worker_link.write_frame(&stdout.encode().unwrap()).unwrap();

        for _ in 0..50 {
            ep.pump(unix_ms(), unix_ms()).unwrap();
            if ep.worker("pc-9").is_some() {
                break;
            }
            thread::sleep(std::time::Duration::from_millis(5));
        }
        let info = ep.worker("pc-9").expect("Hello 应注册 Worker");
        assert!(info.online);
        assert_eq!(info.boot_id, 7);
        assert!(info.has_tool("gpio.info"));
        assert!(info.has_task("echo"));
        worker_link.close();
    }

    /// 非法帧（解不出 Stdout）被记录为错误且不破坏链路。
    #[test]
    fn malformed_frame_is_noted_without_breaking_link() {
        let (router_link, mut worker_link) = sekai_worker::link::memory_pair();
        let endpoint = Endpoint::new("direct", 1).unwrap();
        let mut ep = DirectWorkerEndpoint::new(
            endpoint_config("bad"),
            router_link,
            "pc-1",
            endpoint,
            unix_ms(),
        )
        .unwrap();
        worker_link.write_frame(&[0xff, 0xfe, 0xfd]).unwrap();
        for _ in 0..20 {
            ep.pump(unix_ms(), unix_ms()).unwrap();
        }
        assert!(ep.is_open());
        assert!(ep.last_error().unwrap().contains("无法解码"));
        worker_link.close();
    }
}
