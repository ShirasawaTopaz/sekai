//! Worker网关：把 ControlPlane 包装成线程化、可重连的同步 API。
//!
//! 主线程（agent/CLI）通过 WorkerGateway trait 调用；IO 线程负责 TCP I/O、
//! MQTT 状态机推进、自动重连，并在命令结束时通过通道回复。
//!
//! 时间基准：
//! - IO 线程用 SystemTime 生成 Unix 毫秒，喂给 MQTT 状态机；
//! - 用 Instant（IO 线程启动为 0）生成单调毫秒，喂给命令超时；
//! - 两者都不依赖被测/被控系统的时钟校准。
//!
//! 线程模型：主线程 call 通过 mpsc 发送请求并阻塞等待回复；只有 IO 线程
//! 接触 TCP 与 ControlPlane，因此 MqttGateway 是唯一需要 Send 的边界。

use std::collections::HashMap;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use sekai::sekai_device_list::Endpoint;

use crate::control::{
    CommandKind, CommandRequest, CommandResult, ControlError, ControlEvent, ControlPlane,
    ControlPlaneConfig, TASK_TIMEOUT_EXTRA_MS, TOOL_TIMEOUT_EXTRA_MS,
};
use crate::host_link::{Backoff, Link, LinkError, StreamDecoder, TcpLink};
use crate::topic::TopicRoot;
use crate::worker::WorkerInfo;

/// MQTT Keep Alive（秒）。
const DEFAULT_KEEP_ALIVE: u16 = 30;
/// CONNECT 超时（毫秒）。
const DEFAULT_CONNECT_TIMEOUT_MS: u64 = 5_000;
/// Receive Maximum。
const DEFAULT_RECEIVE_MAXIMUM: u16 = 8;
/// 入站最大报文长度。
const DEFAULT_MAXIMUM_PACKET_SIZE: u32 = 16 * 1024;
/// 主线程等待就绪时的轮询间隔（毫秒）。
const WAIT_READY_POLL_MS: u64 = 5;
/// call 在计算出的命令超时之上额外等待的余量（毫秒）。
const CALL_REPLY_GRACE_MS: u64 = 5_000;
/// 断链退避期间等待请求的最长睡眠（毫秒）。
const BACKOFF_OP_POLL_MS: u64 = 50;

/// 网关错误（带稳定错误码，供 tool 结果 JSON 使用）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GatewayError {
    /// 稳定错误码：worker_offline / worker_not_found / worker_busy /
    /// not_advertised / timeout / link_lost / worker_rebooted / protocol_error。
    pub code: String,
    /// 可读消息。
    pub message: String,
}

impl GatewayError {
    /// 构造错误。
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }

    /// 从控制平面错误映射（保留稳定错误码）。
    pub fn from_control(error: &ControlError) -> Self {
        Self::new(error.code(), error.to_string())
    }
}

impl std::fmt::Display for GatewayError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for GatewayError {}

/// Worker网关抽象；测试可注入内存实现。
///
/// `Send` 约束保证 `Arc<Mutex<Box<dyn WorkerGateway>>>`（会话句柄）跨线程安全。
pub trait WorkerGateway: Send {
    /// 当前Worker快照。
    fn list_workers(&mut self) -> Vec<WorkerInfo>;
    /// 执行一条命令并等待最终事件。
    fn call(&mut self, request: CommandRequest) -> Result<CommandResult, GatewayError>;
    /// 等待链路就绪（连接 + 订阅成功）。
    fn wait_ready(&mut self, timeout_ms: u64) -> Result<(), GatewayError>;
    /// 链路是否就绪。
    fn is_ready(&self) -> bool;
}

/// MQTT 网关配置。
#[derive(Debug, Clone)]
pub struct MqttGatewayConfig {
    /// broker 端点。
    pub endpoint: Endpoint,
    /// MQTT 用户名。
    pub username: Option<String>,
    /// MQTT 密码。
    pub password: Option<String>,
    /// 主题根。
    pub topic_root: TopicRoot,
    /// 可选 client id 覆盖；默认 sekai-router-{pid}-{unix_ms}。
    pub client_id: Option<String>,
    /// 每Worker在途命令上限。
    pub max_inflight_per_worker: usize,
    /// Tool 超时覆盖（绝对毫秒）。
    pub tool_timeout_ms: Option<u64>,
    /// Task 超时覆盖（绝对毫秒）。
    pub task_timeout_ms: Option<u64>,
    /// 默认 DDL（request.ddl_ms == 0 时使用）。
    pub default_ddl_ms: u32,
}

impl MqttGatewayConfig {
    /// 用端点与主题根构造默认配置。
    pub fn new(endpoint: Endpoint, topic_root: TopicRoot) -> Self {
        Self {
            endpoint,
            username: None,
            password: None,
            topic_root,
            client_id: None,
            max_inflight_per_worker: 1,
            tool_timeout_ms: None,
            task_timeout_ms: None,
            default_ddl_ms: 1_000,
        }
    }
}

/// 共享Worker快照（MqttGateway 与 DirectGateway 的 IO 线程共用）。
#[derive(Debug, Clone, Default)]
pub(crate) struct GatewaySnapshot {
    pub(crate) workers: Vec<WorkerInfo>,
    pub(crate) ready: bool,
    pub(crate) connected: bool,
    pub(crate) last_error: Option<String>,
}

/// IO 线程请求（两网关同构；DirectGateway 复用）。
pub(crate) enum GatewayOp {
    Call {
        request: CommandRequest,
        reply: Sender<Result<CommandResult, GatewayError>>,
    },
    Shutdown,
}

/// 会话持有的网关句柄：`Arc<Mutex<Box<dyn WorkerGateway>>>`。
///
/// REPL 会话在链路切换时替换盒内网关；多轮 Agent 各持一份克隆，
/// 经同一把锁串行访问（Agent 本身是同步调用，锁只在调用边界短暂持有）。
pub type GatewayHandle = Arc<Mutex<Box<dyn WorkerGateway>>>;

/// 装箱网关委托（CLI/会话层用 `Box<dyn WorkerGateway>` 统一三种链路形态）。
impl WorkerGateway for Box<dyn WorkerGateway> {
    fn list_workers(&mut self) -> Vec<WorkerInfo> {
        (**self).list_workers()
    }

    fn call(&mut self, request: CommandRequest) -> Result<CommandResult, GatewayError> {
        (**self).call(request)
    }

    fn wait_ready(&mut self, timeout_ms: u64) -> Result<(), GatewayError> {
        (**self).wait_ready(timeout_ms)
    }

    fn is_ready(&self) -> bool {
        (**self).is_ready()
    }
}

/// 共享句柄委托：锁内转发到盒中网关。
impl WorkerGateway for GatewayHandle {
    fn list_workers(&mut self) -> Vec<WorkerInfo> {
        let mut gateway = self.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        gateway.list_workers()
    }

    fn call(&mut self, request: CommandRequest) -> Result<CommandResult, GatewayError> {
        let mut gateway = self.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        gateway.call(request)
    }

    fn wait_ready(&mut self, timeout_ms: u64) -> Result<(), GatewayError> {
        let mut gateway = self.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        gateway.wait_ready(timeout_ms)
    }

    fn is_ready(&self) -> bool {
        let gateway = self.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        gateway.is_ready()
    }
}

/// 计算 call 的等待预算（命令超时 + 回复余量）；两网关共用。
///
/// 优先级：显式 `request.timeout_ms` > 按命令类型的网关超时覆盖 >
/// 由 DDL 推导（DDL + 类型余量）。
pub(crate) fn call_budget_ms(
    default_ddl_ms: u32,
    tool_timeout_ms: Option<u64>,
    task_timeout_ms: Option<u64>,
    request: &CommandRequest,
) -> u64 {
    let kind_timeout = match request.kind {
        CommandKind::Tool => tool_timeout_ms,
        CommandKind::Task => task_timeout_ms,
    };
    let timeout_ms = match request.timeout_ms.or(kind_timeout) {
        Some(value) => value,
        None => {
            let ddl = if request.ddl_ms == 0 {
                u64::from(default_ddl_ms.max(1))
            } else {
                u64::from(request.ddl_ms)
            };
            let extra = match request.kind {
                CommandKind::Tool => TOOL_TIMEOUT_EXTRA_MS,
                CommandKind::Task => TASK_TIMEOUT_EXTRA_MS,
            };
            ddl.saturating_add(extra)
        }
    };
    timeout_ms.saturating_add(CALL_REPLY_GRACE_MS).max(1_000)
}

/// 线程化 MQTT 网关。
pub struct MqttGateway {
    config: MqttGatewayConfig,
    requests: Sender<GatewayOp>,
    snapshot: Arc<Mutex<GatewaySnapshot>>,
    io_worker: Option<JoinHandle<()>>,
}

impl MqttGateway {
    /// 启动 IO 线程并连接 broker。
    ///
    /// 只做配置校验与线程创建；TCP 连接由 IO 线程异步完成，用
    /// MqttGateway::wait_ready 等待就绪。
    pub fn connect(config: MqttGatewayConfig) -> Result<Self, GatewayError> {
        let mut control = ControlPlaneConfig::new(
            config.client_id.clone().unwrap_or_else(default_client_id),
            config.topic_root.clone(),
        );
        control.username = config.username.clone();
        control.password = config.password.clone();
        control.keep_alive = DEFAULT_KEEP_ALIVE;
        control.connect_timeout_ms = DEFAULT_CONNECT_TIMEOUT_MS;
        control.receive_maximum = DEFAULT_RECEIVE_MAXIMUM;
        control.maximum_packet_size = DEFAULT_MAXIMUM_PACKET_SIZE;
        control.max_inflight_per_worker = config.max_inflight_per_worker.max(1);
        control.default_ddl_ms = config.default_ddl_ms.max(1);
        let plane = ControlPlane::new(control).map_err(|error| {
            GatewayError::new("protocol_error", format!("控制平面配置非法：{error}"))
        })?;

        let snapshot = Arc::new(Mutex::new(GatewaySnapshot::default()));
        let (requests_tx, requests_rx) = mpsc::channel();
        let io_config = config.clone();
        let io_snapshot = Arc::clone(&snapshot);
        let io_worker = std::thread::Builder::new()
            .name(String::from("sekai-router-mqtt"))
            .spawn(move || {
                IoWorker {
                    config: io_config,
                    plane,
                    snapshot: io_snapshot,
                    requests: requests_rx,
                    waiters: HashMap::new(),
                    start: Instant::now(),
                    next_attempt: Instant::now(),
                    backoff: Backoff::new(),
                    transport_up: false,
                    last_mono_ms: 0,
                }
                .run();
            })
            .map_err(|error| {
                GatewayError::new("link_lost", format!("无法启动 MQTT IO 线程：{error}"))
            })?;

        Ok(Self {
            config,
            requests: requests_tx,
            snapshot,
            io_worker: Some(io_worker),
        })
    }

    /// 关闭 IO 线程（幂等）。
    pub fn shutdown(&mut self) {
        if let Some(io_worker) = self.io_worker.take() {
            let _ = self.requests.send(GatewayOp::Shutdown);
            let _ = io_worker.join();
            if let Ok(mut snapshot) = self.snapshot.lock() {
                snapshot.ready = false;
                snapshot.connected = false;
            }
        }
    }

    /// 共享快照读取（持锁期间不做其它操作）。
    fn snapshot(&self) -> GatewaySnapshot {
        self.snapshot
            .lock()
            .map(|snapshot| snapshot.clone())
            .unwrap_or_else(|poisoned| poisoned.into_inner().clone())
    }

    fn worker_alive(&self) -> bool {
        self.io_worker
            .as_ref()
            .map(|io_worker| !io_worker.is_finished())
            .unwrap_or(false)
    }

    /// 计算 call 的等待预算（命令超时 + 回复余量）。
    fn wait_budget_ms(&self, request: &CommandRequest) -> u64 {
        call_budget_ms(
            self.config.default_ddl_ms,
            self.config.tool_timeout_ms,
            self.config.task_timeout_ms,
            request,
        )
    }
}

impl Drop for MqttGateway {
    fn drop(&mut self) {
        self.shutdown();
    }
}

impl WorkerGateway for MqttGateway {
    fn list_workers(&mut self) -> Vec<WorkerInfo> {
        self.snapshot().workers
    }

    fn call(&mut self, mut request: CommandRequest) -> Result<CommandResult, GatewayError> {
        if request.ddl_ms == 0 {
            request.ddl_ms = self.config.default_ddl_ms.max(1);
        }
        if request.timeout_ms.is_none() {
            request.timeout_ms = match request.kind {
                CommandKind::Tool => self.config.tool_timeout_ms,
                CommandKind::Task => self.config.task_timeout_ms,
            };
        }
        let budget = self.wait_budget_ms(&request);
        let (reply_tx, reply_rx) = mpsc::channel();
        self.requests
            .send(GatewayOp::Call {
                request,
                reply: reply_tx,
            })
            .map_err(|_| GatewayError::new("link_lost", "MQTT IO 线程已退出"))?;
        match reply_rx.recv_timeout(Duration::from_millis(budget)) {
            Ok(result) => result,
            Err(RecvTimeoutError::Timeout) => Err(GatewayError::new(
                "timeout",
                "等待Worker最终事件超时（IO 线程未在预算内回复）",
            )),
            Err(RecvTimeoutError::Disconnected) => {
                Err(GatewayError::new("link_lost", "MQTT IO 线程已退出"))
            }
        }
    }

    fn wait_ready(&mut self, timeout_ms: u64) -> Result<(), GatewayError> {
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        loop {
            {
                let snapshot = self.snapshot();
                if snapshot.ready && snapshot.connected {
                    return Ok(());
                }
            }
            if !self.worker_alive() {
                return Err(GatewayError::new("link_lost", "MQTT IO 线程已退出"));
            }
            if Instant::now() >= deadline {
                let snapshot = self.snapshot();
                let message = snapshot
                    .last_error
                    .clone()
                    .unwrap_or_else(|| String::from("等待 MQTT 链路就绪超时"));
                return Err(GatewayError::new(
                    if snapshot.connected {
                        "timeout"
                    } else {
                        "link_lost"
                    },
                    message,
                ));
            }
            std::thread::sleep(Duration::from_millis(WAIT_READY_POLL_MS));
        }
    }

    fn is_ready(&self) -> bool {
        self.snapshot().ready && self.worker_alive()
    }
}

fn default_client_id() -> String {
    let unix_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or(0);
    format!("sekai-router-{}-{}", std::process::id(), unix_ms)
}

/// IO 线程状态。
struct IoWorker {
    config: MqttGatewayConfig,
    plane: ControlPlane,
    snapshot: Arc<Mutex<GatewaySnapshot>>,
    requests: Receiver<GatewayOp>,
    waiters: HashMap<u64, Sender<Result<CommandResult, GatewayError>>>,
    start: Instant,
    next_attempt: Instant,
    backoff: Backoff,
    transport_up: bool,
    last_mono_ms: u64,
}

enum IoFailure {
    /// 链路断开，需要重连。
    Lost(String),
}

impl IoWorker {
    fn run(mut self) {
        let mut link = TcpLink::new();
        let mut decoder: Option<StreamDecoder> = None;
        loop {
            if self.pump_ops() {
                break;
            }
            if !self.transport_up {
                let now = Instant::now();
                if now < self.next_attempt {
                    let wait =
                        (self.next_attempt - now).min(Duration::from_millis(BACKOFF_OP_POLL_MS));
                    match self.requests.recv_timeout(wait) {
                        Ok(GatewayOp::Call { request, reply }) => {
                            self.reject_call(request, reply);
                        }
                        Ok(GatewayOp::Shutdown) => break,
                        Err(RecvTimeoutError::Timeout) => {}
                        Err(RecvTimeoutError::Disconnected) => break,
                    }
                    continue;
                }
                if self.start_transport(&mut link).is_err() {
                    continue;
                }
                decoder = Some(StreamDecoder::new(DEFAULT_MAXIMUM_PACKET_SIZE));
                continue;
            }

            let round = match decoder.as_mut() {
                Some(decoder_ref) => self.io_round(&mut link, decoder_ref),
                None => {
                    self.transport_up = false;
                    continue;
                }
            };
            if let Err(IoFailure::Lost(reason)) = round {
                self.on_transport_lost(&mut link, &reason);
                decoder = None;
            }
        }
        self.shutdown(&mut link, decoder.is_some());
    }

    fn shutdown(&mut self, link: &mut TcpLink, transport_up: bool) {
        if transport_up {
            let now_unix = self.now_unix_ms();
            self.plane.link_closed(now_unix);
            self.drain_events();
            let now_unix = self.now_unix_ms();
            while let Some(packet) = self.plane.next_outgoing(now_unix) {
                if let Ok(bytes) = packet.encode()
                    && link.write_all(&bytes).is_err()
                {
                    break;
                }
            }
            link.close();
        }
        self.transport_up = false;
        self.fail_waiters("link_lost", "网关已关闭");
        self.publish_snapshot();
    }

    // ---- 时间 ----

    fn now_unix_ms(&mut self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_millis() as u64)
            .unwrap_or(0)
    }

    fn now_mono_ms(&mut self) -> u64 {
        let elapsed = self.start.elapsed().as_millis() as u64;
        self.last_mono_ms = elapsed;
        elapsed
    }

    // ---- 请求 ----

    fn pump_ops(&mut self) -> bool {
        loop {
            match self.requests.try_recv() {
                Ok(GatewayOp::Call { request, reply }) => self.dispatch_call(request, reply),
                Ok(GatewayOp::Shutdown) => return true,
                Err(TryRecvError::Empty) => return false,
                Err(TryRecvError::Disconnected) => return true,
            }
        }
    }

    fn dispatch_call(
        &mut self,
        request: CommandRequest,
        reply: Sender<Result<CommandResult, GatewayError>>,
    ) {
        if !self.transport_up {
            let _ = reply.send(Err(GatewayError::new(
                "link_lost",
                "MQTT 链路未就绪（正在连接或退避重连）",
            )));
            return;
        }
        let now_unix = self.now_unix_ms();
        let now_mono = self.now_mono_ms();
        match self.plane.submit(request, now_unix, now_mono) {
            Ok(cmd_id) => {
                self.waiters.insert(cmd_id, reply);
            }
            Err(error) => {
                let _ = reply.send(Err(GatewayError::from_control(&error)));
            }
        }
        self.publish_snapshot();
    }

    fn reject_call(
        &mut self,
        _request: CommandRequest,
        reply: Sender<Result<CommandResult, GatewayError>>,
    ) {
        let _ = reply.send(Err(GatewayError::new(
            "link_lost",
            "MQTT 链路未就绪（正在连接或退避重连）",
        )));
    }

    fn fail_waiters(&mut self, code: &str, message: &str) {
        let waiters = std::mem::take(&mut self.waiters);
        for (_, reply) in waiters {
            let _ = reply.send(Err(GatewayError::new(code, message)));
        }
    }

    fn drain_events(&mut self) {
        let mut finished = Vec::new();
        for event in self.plane.take_events() {
            if let ControlEvent::CommandFinished(result) = event {
                finished.push(result);
            }
        }
        for result in finished {
            if let Some(reply) = self.waiters.remove(&result.cmd_id) {
                let _ = reply.send(Ok(result));
            }
        }
        self.publish_snapshot();
    }

    fn publish_snapshot(&mut self) {
        let workers = self.plane.workers();
        let ready = self.plane.is_ready();
        let connected = self.plane.is_connected();
        let plane_error = self.plane.last_error().map(String::from);
        if let Ok(mut snapshot) = self.snapshot.lock() {
            snapshot.workers = workers;
            snapshot.ready = ready;
            snapshot.connected = connected;
            // 保留 worker 侧的传输错误（连接失败/分帧失败），否则用控制平面错误。
            if plane_error.is_some() {
                snapshot.last_error = plane_error;
            }
        }
    }

    fn note_error(&mut self, message: String) {
        if let Ok(mut snapshot) = self.snapshot.lock() {
            snapshot.last_error = Some(message);
        }
    }

    // ---- 连接 / I/O ----

    fn start_transport(&mut self, link: &mut TcpLink) -> Result<(), ()> {
        if let Err(error) = link.connect(&self.config.endpoint) {
            self.note_error(format!("broker 连接失败：{error}"));
            let delay = self.backoff.next_delay_ms();
            self.next_attempt = Instant::now() + Duration::from_millis(delay);
            self.publish_snapshot();
            return Err(());
        }
        let now_unix = self.now_unix_ms();
        match self.plane.link_up(self.config.endpoint.clone(), now_unix) {
            Ok(()) => {
                self.transport_up = true;
                self.backoff.reset();
                self.publish_snapshot();
                Ok(())
            }
            Err(error) => {
                link.close();
                self.note_error(format!("MQTT link_up 失败：{error}"));
                let delay = self.backoff.next_delay_ms();
                self.next_attempt = Instant::now() + Duration::from_millis(delay);
                self.publish_snapshot();
                Err(())
            }
        }
    }

    fn io_round(
        &mut self,
        link: &mut TcpLink,
        decoder: &mut StreamDecoder,
    ) -> Result<(), IoFailure> {
        let now_unix = self.now_unix_ms();
        while let Some(packet) = self.plane.next_outgoing(now_unix) {
            match packet.encode() {
                Ok(bytes) => {
                    if let Err(error) = link.write_all(&bytes) {
                        return Err(IoFailure::Lost(format!("写链路失败：{error}")));
                    }
                }
                Err(error) => {
                    self.note_error(format!("MQTT 报文编码失败：{error}"));
                }
            }
        }

        let mut buffer = [0u8; 4096];
        loop {
            match link.read(&mut buffer) {
                Ok(0) => {
                    return Err(IoFailure::Lost(String::from("broker 关闭连接（EOF）")));
                }
                Ok(read) => {
                    if let Err(error) = decoder.push(&buffer[..read]) {
                        return Err(IoFailure::Lost(format!("MQTT 分帧失败：{error}")));
                    }
                    loop {
                        match decoder.next_packet() {
                            Ok(Some(packet)) => {
                                let now_unix = self.now_unix_ms();
                                if let Err(error) = self.plane.feed(packet, now_unix) {
                                    self.note_error(format!("MQTT 协议错误：{error}"));
                                }
                            }
                            Ok(None) => break,
                            Err(error) => {
                                return Err(IoFailure::Lost(format!("MQTT 解码失败：{error}")));
                            }
                        }
                    }
                }
                Err(LinkError::Timeout) => break,
                Err(LinkError::NotConnected) => {
                    return Err(IoFailure::Lost(String::from("链路未连接")));
                }
                Err(error) => return Err(IoFailure::Lost(error.to_string())),
            }
        }

        let now_unix = self.now_unix_ms();
        let now_mono = self.now_mono_ms();
        if let Err(error) = self.plane.poll(now_unix, now_mono) {
            self.note_error(format!("控制平面 poll 失败：{error}"));
        }
        self.drain_events();

        if !self.plane.is_connected() && self.plane.last_error().is_some() {
            let message = self
                .plane
                .last_error()
                .map(String::from)
                .unwrap_or_else(|| String::from("MQTT 连接已断开"));
            return Err(IoFailure::Lost(message));
        }
        Ok(())
    }

    fn on_transport_lost(&mut self, link: &mut TcpLink, reason: &str) {
        self.transport_up = false;
        link.close();
        let now_unix = self.now_unix_ms();
        self.plane.link_lost(now_unix);
        self.drain_events();
        self.fail_waiters("link_lost", &format!("MQTT 链路断开：{reason}"));
        self.note_error(format!("MQTT 链路断开：{reason}"));
        let delay = self.backoff.next_delay_ms();
        self.next_attempt = Instant::now() + Duration::from_millis(delay);
        self.publish_snapshot();
    }
}
