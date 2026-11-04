//! 直连网关：把 [`DirectWorkerEndpoint`]（stdio / TCP listen）包装成线程化
//! [`WorkerGateway`]，与 [`MqttGateway`](crate::gateway::MqttGateway) 同构。
//!
//! ## 线程模型（与 MqttGateway 一致）
//!
//! 主线程（agent/CLI/REPL）经 [`WorkerGateway`] trait 调用；IO 线程独占
//! 直连链路与 ControlPlane 端点，通过 mpsc 收 Call/Shutdown 请求，命令
//! 结束事件经 `waiters` 通道回复，快照经 `Arc<Mutex<GatewaySnapshot>>` 共享。
//!
//! ## IO 线程状态机
//!
//! ```text
//! listen_tcp:   Accepting ──Worker 连入──▶ Connected ──断链──▶ Accepting（等重连）
//! spawn_stdio:  Connected（子进程）──子进程退出──▶ Closed（拒绝后续 call）
//! over_link:    Connected ──链路关闭──▶ Closed（测试注入）
//! ```
//!
//! - **tcp 断链不退出**：监听器随状态保留，Worker 侧自带指数退避重连，
//!   重连后新 Hello 在全新 ControlPlane 注册表重新注册；
//! - **stdio 子进程死亡是终态**：网关保持存活但 `ready=false`，后续 call
//!   以 `link_lost` 拒绝；不自动重启子进程（由会话层重新 `/link stdio`）。

use std::collections::HashMap;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use sekai::sekai_device_list::Endpoint;
use sekai_worker::link::Link;

use crate::control::{CommandKind, CommandRequest, CommandResult, ControlPlaneConfig};
use crate::direct::{AcceptedTcpLink, DirectListener, DirectWorkerEndpoint, StdioWorkerLink};
use crate::gateway::{
    CommandReceipt, GatewayError, GatewayOp, GatewaySnapshot, WorkerGateway, call_budget_ms,
};

/// 主线程等待就绪时的轮询间隔（毫秒）。
const WAIT_READY_POLL_MS: u64 = 5;
/// Accepting / Closed 状态下每轮最长睡眠（毫秒）。
const IDLE_POLL_MS: u64 = 50;
/// IO 线程名。
const IO_THREAD_NAME: &str = "sekai-router-direct";

/// 直连网关配置（stdio / tcp / 注入链路共用）。
#[derive(Debug, Clone)]
pub struct DirectGatewayConfig {
    /// 控制平面配置（每条 TCP 连接克隆一份，注册表互相独立）。
    pub plane: ControlPlaneConfig,
    /// 端点预设 Worker id（首帧 Hello 前的兜底，仅影响主题包装）。
    pub worker_id_hint: String,
    /// 默认 DDL（request.ddl_ms == 0 时使用）。
    pub default_ddl_ms: u32,
    /// Tool 超时覆盖（绝对毫秒）。
    pub tool_timeout_ms: Option<u64>,
    /// Task 超时覆盖（绝对毫秒）。
    pub task_timeout_ms: Option<u64>,
}

impl DirectGatewayConfig {
    /// 用控制平面配置构造；默认 DDL 沿 ControlPlane 配置。
    pub fn new(plane: ControlPlaneConfig, worker_id_hint: impl Into<String>) -> Self {
        let default_ddl_ms = plane.default_ddl_ms;
        Self {
            plane,
            worker_id_hint: worker_id_hint.into(),
            default_ddl_ms,
            tool_timeout_ms: None,
            task_timeout_ms: None,
        }
    }
}

/// 网关链路形态（诊断 / `/status` 展示用）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DirectMode {
    /// Router 拉起的 Worker 子进程。
    StdioSpawn { program: String },
    /// Router 监听、Worker 主动连入（addr 为实际监听地址，端口 0 已展开）。
    TcpListen { addr: String },
    /// 注入链路（测试）。
    Injected,
}

impl DirectMode {
    /// 人读描述。
    pub fn describe(&self) -> String {
        match self {
            Self::StdioSpawn { program } => format!("stdio 子进程（{program}）"),
            Self::TcpListen { addr } => format!("tcp 监听（{addr}）"),
            Self::Injected => String::from("注入链路"),
        }
    }
}

/// IO 线程持有的服务端链路：三种来源统一到一个 Link 实现。
enum ServerLink {
    Stdio(StdioWorkerLink),
    Tcp(AcceptedTcpLink),
    Injected(Box<dyn Link + Send>),
}

impl Link for ServerLink {
    fn read_frame(&mut self) -> Result<Option<Vec<u8>>, sekai_worker::link::LinkError> {
        match self {
            Self::Stdio(link) => link.read_frame(),
            Self::Tcp(link) => link.read_frame(),
            Self::Injected(link) => link.read_frame(),
        }
    }

    fn write_frame(&mut self, frame: &[u8]) -> Result<(), sekai_worker::link::LinkError> {
        match self {
            Self::Stdio(link) => link.write_frame(frame),
            Self::Tcp(link) => link.write_frame(frame),
            Self::Injected(link) => link.write_frame(frame),
        }
    }

    fn close(&mut self) {
        match self {
            Self::Stdio(link) => link.close(),
            Self::Tcp(link) => link.close(),
            Self::Injected(link) => link.close(),
        }
    }
}

/// IO 线程状态。
// Connected 变体（端点 + 可选监听器）显著大于其余变体：该枚举为 IO 线程
// 独占、切换频率低，装箱省内存会牵动全部构造与 match 位点，得不偿失，
// 此处显式豁免 large_enum_variant。
#[allow(clippy::large_enum_variant)]
enum IoState {
    /// tcp：等待 Worker 连入。
    Accepting { listener: DirectListener },
    /// 链路已建立（端点内含 ControlPlane）。
    ///
    /// `listener` 在 tcp 模式保留，断链后回到 [`IoState::Accepting`]；
    /// stdio/注入为 `None`，断链即终态。
    Connected {
        endpoint: DirectWorkerEndpoint<ServerLink>,
        listener: Option<DirectListener>,
    },
    /// 终态（stdio 子进程退出 / 注入链路关闭）：拒绝后续 call。
    Closed,
}

/// 直连网关（线程化 `WorkerGateway`）。
pub struct DirectGateway {
    config: DirectGatewayConfig,
    mode: DirectMode,
    requests: Sender<GatewayOp>,
    snapshot: Arc<Mutex<GatewaySnapshot>>,
    io_worker: Option<JoinHandle<()>>,
}

impl DirectGateway {
    /// stdio 模式：拉起 Worker 子进程，其 stdin/stdout 即直连链路。
    ///
    /// 子进程拉起失败（程序不存在等）同步返回错误。
    pub fn spawn_stdio(
        program: &str,
        args: &[String],
        config: DirectGatewayConfig,
    ) -> Result<Self, GatewayError> {
        let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let link = StdioWorkerLink::spawn(program, &arg_refs).map_err(|error| {
            GatewayError::new("link_lost", format!("拉起 Worker 失败：{error}"))
        })?;
        let endpoint = build_endpoint(&config, ServerLink::Stdio(link))?;
        Self::start(
            config,
            DirectMode::StdioSpawn {
                program: program.to_string(),
            },
            IoState::Connected {
                endpoint,
                listener: None,
            },
        )
    }

    /// tcp listen 模式：监听 `addr` 等 Worker 连入；断链后回到等待状态。
    ///
    /// 绑定失败同步返回错误；实际地址（支持端口 0 自动分配）记录在
    /// [`DirectMode::TcpListen`]，可经 [`DirectGateway::mode`] 读取。
    pub fn listen_tcp(addr: &str, config: DirectGatewayConfig) -> Result<Self, GatewayError> {
        let listener = DirectListener::bind(addr).map_err(|error| {
            GatewayError::new("link_lost", format!("监听 {addr} 失败：{error}"))
        })?;
        let local = listener
            .local_addr()
            .map(|addr| addr.to_string())
            .unwrap_or_else(|_| addr.to_string());
        Self::start(
            config,
            DirectMode::TcpListen { addr: local },
            IoState::Accepting { listener },
        )
    }

    /// 注入链路（测试）：直接包装一条已建立的链路。
    pub fn over_link(
        link: Box<dyn Link + Send>,
        config: DirectGatewayConfig,
    ) -> Result<Self, GatewayError> {
        let endpoint = build_endpoint(&config, ServerLink::Injected(link))?;
        Self::start(
            config,
            DirectMode::Injected,
            IoState::Connected {
                endpoint,
                listener: None,
            },
        )
    }

    /// 链路形态描述。
    pub fn mode(&self) -> &DirectMode {
        &self.mode
    }

    // ---- 内部 ----

    fn start(
        config: DirectGatewayConfig,
        mode: DirectMode,
        state: IoState,
    ) -> Result<Self, GatewayError> {
        let snapshot = Arc::new(Mutex::new(GatewaySnapshot::default()));
        let (requests_tx, requests_rx) = mpsc::channel();
        let io_snapshot = Arc::clone(&snapshot);
        let io_config = config.clone();
        let io_worker = std::thread::Builder::new()
            .name(String::from(IO_THREAD_NAME))
            .spawn(move || {
                DirectIoWorker {
                    config: io_config,
                    state,
                    snapshot: io_snapshot,
                    requests: requests_rx,
                    waiters: HashMap::new(),
                    start: Instant::now(),
                }
                .run();
            })
            .map_err(|error| {
                GatewayError::new("link_lost", format!("无法启动直连 IO 线程：{error}"))
            })?;
        Ok(Self {
            config,
            mode,
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
                snapshot.workers.clear();
            }
        }
    }

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
}

impl Drop for DirectGateway {
    fn drop(&mut self) {
        self.shutdown();
    }
}

impl WorkerGateway for DirectGateway {
    fn list_workers(&mut self) -> Vec<crate::worker::WorkerInfo> {
        self.snapshot().workers
    }

    fn submit(&mut self, mut request: CommandRequest) -> Result<CommandReceipt, GatewayError> {
        if request.ddl_ms == 0 {
            request.ddl_ms = self.config.default_ddl_ms.max(1);
        }
        if request.timeout_ms.is_none() {
            request.timeout_ms = match request.kind {
                CommandKind::Tool => self.config.tool_timeout_ms,
                CommandKind::Task => self.config.task_timeout_ms,
            };
        }
        let budget = call_budget_ms(
            self.config.default_ddl_ms,
            self.config.tool_timeout_ms,
            self.config.task_timeout_ms,
            &request,
        );
        let (reply_tx, reply_rx) = mpsc::channel();
        self.requests
            .send(GatewayOp::Call {
                request,
                reply: reply_tx,
            })
            .map_err(|_| GatewayError::new("link_lost", "直连 IO 线程已退出"))?;
        Ok(CommandReceipt::new(reply_rx, budget, "直连"))
    }

    fn wait_ready(&mut self, timeout_ms: u64) -> Result<(), GatewayError> {
        let deadline = Instant::now() + Duration::from_millis(timeout_ms.max(1));
        loop {
            {
                let snapshot = self.snapshot();
                if snapshot.ready && snapshot.connected {
                    return Ok(());
                }
            }
            if !self.worker_alive() {
                return Err(GatewayError::new("link_lost", "直连 IO 线程已退出"));
            }
            if Instant::now() >= deadline {
                let snapshot = self.snapshot();
                let message = snapshot
                    .last_error
                    .clone()
                    .unwrap_or_else(|| String::from("等待直连链路就绪超时"));
                return Err(GatewayError::new("timeout", message));
            }
            std::thread::sleep(Duration::from_millis(WAIT_READY_POLL_MS));
        }
    }

    fn is_ready(&self) -> bool {
        self.snapshot().ready && self.worker_alive()
    }
}

/// 构造直连端点（虚拟握手在构造内完成）。
fn build_endpoint(
    config: &DirectGatewayConfig,
    link: ServerLink,
) -> Result<DirectWorkerEndpoint<ServerLink>, GatewayError> {
    let endpoint_addr = Endpoint::new("direct", 1)
        .map_err(|error| GatewayError::new("protocol_error", error.to_string()))?;
    DirectWorkerEndpoint::new(
        config.plane.clone(),
        link,
        config.worker_id_hint.clone(),
        endpoint_addr,
        now_ms(),
    )
    .map_err(|error| GatewayError::new("protocol_error", error.to_string()))
}

/// IO 线程状态机。
struct DirectIoWorker {
    config: DirectGatewayConfig,
    state: IoState,
    snapshot: Arc<Mutex<GatewaySnapshot>>,
    requests: Receiver<GatewayOp>,
    waiters: HashMap<u64, Sender<Result<CommandResult, GatewayError>>>,
    start: Instant,
}

impl DirectIoWorker {
    fn run(mut self) {
        loop {
            if self.pump_ops() {
                break;
            }
            // 在状态借用内只做无自借用冲突的工作；事件排空/快照/迁移
            // 统一放在 match 之后。
            let mut note: Option<String> = None;
            let mut drain = false;
            let mut relisten: Option<DirectListener> = None;
            let mut finalize = false;
            let now_mono = self.now_mono_ms();

            // tcp：Accepting 状态限时等连入。监听器经 mem::replace 取出、
            // 建立连接后随端点一起保留（断链时放回），否则连接一旦建立
            // 监听器就被丢弃、无法回到等待状态。
            if let IoState::Accepting { .. } = self.state {
                let listener = match std::mem::replace(&mut self.state, IoState::Closed) {
                    IoState::Accepting { listener } => listener,
                    _ => unreachable!("上方已判定为 Accepting"),
                };
                match listener.accept_link_timeout(Duration::from_millis(IDLE_POLL_MS)) {
                    Ok(Some(link)) => match build_endpoint(&self.config, ServerLink::Tcp(link)) {
                        Ok(endpoint) => {
                            self.state = IoState::Connected {
                                endpoint,
                                listener: Some(listener),
                            };
                        }
                        Err(error) => {
                            note = Some(format!("建立直连端点失败：{error}"));
                            self.state = IoState::Accepting { listener };
                        }
                    },
                    Ok(None) => self.state = IoState::Accepting { listener },
                    Err(error) => {
                        note = Some(format!("accept 失败：{error}"));
                        std::thread::sleep(Duration::from_millis(IDLE_POLL_MS));
                        self.state = IoState::Accepting { listener };
                    }
                }
            }

            match &mut self.state {
                IoState::Accepting { .. } => {
                    // accept 已在上方处理，这里无事可做。
                }
                IoState::Connected { endpoint, listener } => {
                    let now_unix = now_ms();
                    if let Err(error) = endpoint.pump(now_unix, now_mono) {
                        note = Some(format!("控制平面错误：{error}"));
                    }
                    let open = endpoint.is_open();
                    drain = true;
                    if !open && let Some(back) = listener.take() {
                        relisten = Some(back);
                    } else if !open {
                        finalize = true;
                    }
                }
                IoState::Closed => {
                    // 终态：只等请求，保持线程可关停。
                    match self
                        .requests
                        .recv_timeout(Duration::from_millis(IDLE_POLL_MS))
                    {
                        Ok(op) => {
                            if self.handle_op(op) {
                                break;
                            }
                        }
                        Err(RecvTimeoutError::Timeout) | Err(RecvTimeoutError::Disconnected) => {}
                    }
                }
            }
            if let Some(message) = note {
                self.note(message);
            }
            if drain {
                self.drain_events();
            }
            if relisten.is_some() || finalize {
                self.publish_snapshot();
                if let Some(listener) = relisten {
                    self.fail_waiters("link_lost", "直连链路断开，等待 Worker 重连");
                    self.state = IoState::Accepting { listener };
                } else {
                    self.fail_waiters("link_lost", "直连链路已断开");
                    self.state = IoState::Closed;
                }
            }
            self.publish_snapshot();
        }
        self.shutdown_state();
    }

    fn pump_ops(&mut self) -> bool {
        loop {
            match self.requests.try_recv() {
                Ok(op) => {
                    if self.handle_op(op) {
                        return true;
                    }
                }
                Err(TryRecvError::Empty) => return false,
                Err(TryRecvError::Disconnected) => return true,
            }
        }
    }

    /// 处理一个请求；返回 true 表示 Shutdown。
    fn handle_op(&mut self, op: GatewayOp) -> bool {
        match op {
            GatewayOp::Call { request, reply } => self.dispatch_call(request, reply),
            GatewayOp::Shutdown => return true,
        }
        false
    }

    fn dispatch_call(
        &mut self,
        request: CommandRequest,
        reply: Sender<Result<CommandResult, GatewayError>>,
    ) {
        let now_unix = now_ms();
        let now_mono = self.now_mono_ms();
        // 先在端点借用内完成 submit，避免与 self.waiters/snapshot 重叠借用。
        enum Outcome {
            Submitted(Result<u64, crate::control::ControlError>),
            Rejected(&'static str),
        }
        let outcome = match &mut self.state {
            IoState::Connected { endpoint, .. } => {
                Outcome::Submitted(endpoint.submit(request, now_unix, now_mono))
            }
            IoState::Accepting { .. } => Outcome::Rejected("直连链路未就绪（等待 Worker 连入）"),
            IoState::Closed => Outcome::Rejected("直连链路已断开"),
        };
        match outcome {
            Outcome::Submitted(Ok(cmd_id)) => {
                self.waiters.insert(cmd_id, reply);
            }
            Outcome::Submitted(Err(error)) => {
                let _ = reply.send(Err(GatewayError::new(error.code(), error.to_string())));
            }
            Outcome::Rejected(message) => {
                let _ = reply.send(Err(GatewayError::new("link_lost", message)));
            }
        }
        self.publish_snapshot();
    }

    fn drain_events(&mut self) {
        let IoState::Connected { endpoint, .. } = &mut self.state else {
            return;
        };
        let mut finished = Vec::new();
        for event in endpoint.take_events() {
            if let crate::control::ControlEvent::CommandFinished(result) = event {
                finished.push(result);
            }
        }
        for result in finished {
            if let Some(reply) = self.waiters.remove(&result.cmd_id) {
                let _ = reply.send(Ok(result));
            }
        }
    }

    fn fail_waiters(&mut self, code: &str, message: &str) {
        let waiters = std::mem::take(&mut self.waiters);
        for (_, reply) in waiters {
            let _ = reply.send(Err(GatewayError::new(code, message)));
        }
    }

    fn publish_snapshot(&mut self) {
        let (workers, ready, connected) = match &self.state {
            IoState::Connected { endpoint, .. } => (
                endpoint.workers(),
                endpoint.is_ready() && endpoint.is_open(),
                endpoint.is_open(),
            ),
            IoState::Accepting { .. } | IoState::Closed => (Vec::new(), false, false),
        };
        if let Ok(mut snapshot) = self.snapshot.lock() {
            snapshot.workers = workers;
            snapshot.ready = ready;
            snapshot.connected = connected;
        }
    }

    fn note(&mut self, message: String) {
        if let Ok(mut snapshot) = self.snapshot.lock() {
            snapshot.last_error = Some(message);
        }
    }

    fn now_mono_ms(&self) -> u64 {
        self.start.elapsed().as_millis() as u64
    }

    fn shutdown_state(&mut self) {
        if let IoState::Connected { endpoint, .. } = &mut self.state {
            endpoint.close(now_ms());
        }
        self.state = IoState::Closed;
        self.fail_waiters("link_lost", "网关已关闭");
        self.publish_snapshot();
    }
}

/// 当前 Unix 毫秒；时钟异常时返回 0。
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}
