//! 直连网关：把直连链路（stdio 子进程 / TCP 监听 / 注入）包装成线程化
//! [`WorkerGateway`]，与 [`MqttGateway`](crate::gateway::MqttGateway) 同构。
//!
//! ## 线程模型（与 MqttGateway 一致）
//!
//! 主线程（agent/CLI/REPL）经 [`WorkerGateway`] trait 调用；IO 线程独占
//! 直连链路与 ControlPlane 端点，通过 mpsc 收 Call/Cancel/Shutdown 请求，
//! 命令结束事件经 `waiters` 通道回复，快照经 `Arc<Mutex<GatewaySnapshot>>`
//! 共享、诊断经 `Arc<Mutex<GatewayDiagnostics>>` 共享。
//!
//! ## IO 线程状态机
//!
//! ```text
//! listen_tcp:   Listening ──每连接一个端点──▶（常驻 accept，断链只移除该端点）
//! spawn_stdio:  Single（子进程）──子进程退出──▶ Closed（拒绝后续 call）
//! over_link:    Single ──链路关闭──▶ Closed（测试注入，单端点语义）
//! ```
//!
//! - **tcp 多 Worker（B1）**：监听器常驻，每次 accept 建一条独立
//!   [`DirectWorkerEndpoint`]（各自独立 ControlPlane 注册表/命令空间）；
//!   派工按 Hello 上报的 `worker_id` **精确路由**到属主端点；
//!   `list_workers` 聚合全部端点，`wait_ready`/`is_ready` 任一端点就绪即可；
//!   同一 `worker_id` 的后来连接顶替旧连接（关闭旧连接并记录 `last_error`）；
//! - **stdio 单端点**：行为与旧版一致，子进程死亡是终态，`ready=false`，
//!   后续 call 以 `link_lost` 拒绝（由会话层重新 `/link stdio`）；
//! - **跳-/断链**：tcp 端点断开只移除该端点，监听器与其它 Worker 不受影响。

use std::collections::HashMap;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use sekai::sekai_device_list::Endpoint;
use sekai_worker::link::Link;

use crate::control::{
    CommandKind, CommandRequest, CommandResult, ControlPlaneConfig, GatewayDiagnostics,
};
use crate::direct::{AcceptedTcpLink, DirectListener, DirectWorkerEndpoint, StdioWorkerLink};
use crate::gateway::{
    CANCEL_ACK_TIMEOUT_MS, CommandReceipt, GatewayError, GatewayOp, GatewaySnapshot,
    SUBMIT_ACCEPTED_TIMEOUT_MS, WorkerGateway, call_budget_ms,
};

/// 主线程等待就绪时的轮询间隔（毫秒）。
const WAIT_READY_POLL_MS: u64 = 5;
/// 无连接时 accept / Closed 状态下每轮最长睡眠（毫秒）。
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

/// 一条直连连接：端点 + 该连接自己的等待者表。
///
/// cmd_id 只在一条连接的 ControlPlane 命名空间内唯一，因此等待者必须按连接
/// 隔离（不同连接的 ControlPlane 可能生成相同 cmd_id）。
struct DirectConnection {
    endpoint: DirectWorkerEndpoint<ServerLink>,
    waiters: HashMap<u64, Sender<Result<CommandResult, GatewayError>>>,
}

impl DirectConnection {
    fn new(endpoint: DirectWorkerEndpoint<ServerLink>) -> Self {
        Self {
            endpoint,
            waiters: HashMap::new(),
        }
    }

    /// 提交命令（`management` 为 true 时绕过每 Worker 在途上限）。
    fn submit(
        &mut self,
        request: CommandRequest,
        management: bool,
        now_unix_ms: u64,
        now_mono_ms: u64,
    ) -> Result<u64, GatewayError> {
        let result = if management {
            self.endpoint
                .submit_control(request, now_unix_ms, now_mono_ms)
        } else {
            self.endpoint.submit(request, now_unix_ms, now_mono_ms)
        };
        result.map_err(|error| GatewayError::from_control(&error))
    }

    /// 取消在途命令（透传 `ControlPlane::cancel`）。
    fn cancel(
        &mut self,
        cmd_id: u64,
        now_unix_ms: u64,
        now_mono_ms: u64,
    ) -> Result<(), GatewayError> {
        self.endpoint
            .cancel(cmd_id, now_unix_ms, now_mono_ms)
            .map(|_| ())
            .map_err(|error| GatewayError::from_control(&error))
    }

    /// 回收本连接的命令结束事件并回复等待者。
    fn drain_events(&mut self) {
        let mut finished = Vec::new();
        for event in self.endpoint.take_events() {
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

    /// 失败本连接全部等待者（链路断开 / 被新连接顶替）。
    fn fail_waiters(&mut self, code: &str, message: &str) {
        let waiters = std::mem::take(&mut self.waiters);
        for (_, reply) in waiters {
            let _ = reply.send(Err(GatewayError::new(code, message)));
        }
    }
}

/// IO 线程状态。
// Listening/Single 变体都持有较大的端点状态：该枚举由 IO 线程独占、
// 切换频率低，装箱会牵动全部构造与 match 位点，得不偿失，显式豁免。
#[allow(clippy::large_enum_variant)]
enum IoState {
    /// tcp：常驻监听 + 多端点表（每条连接一个独立 ControlPlane）。
    Listening {
        listener: DirectListener,
        connections: Vec<DirectConnection>,
    },
    /// stdio / 注入：单端点语义（链路断开即终态）。
    Single { connection: DirectConnection },
    /// 终态：只等请求，拒绝 call。
    Closed,
}

/// 直连网关（线程化 `WorkerGateway`）。
pub struct DirectGateway {
    config: DirectGatewayConfig,
    mode: DirectMode,
    requests: Sender<GatewayOp>,
    snapshot: Arc<Mutex<GatewaySnapshot>>,
    /// IO 线程维护的控制面诊断快照（`diagnostics` 直接 clone）。
    diagnostics: Arc<Mutex<GatewayDiagnostics>>,
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
            IoState::Single {
                connection: DirectConnection::new(endpoint),
            },
        )
    }

    /// tcp listen 模式：常驻监听 `addr`，每个连入的 Worker 一条独立端点。
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
            IoState::Listening {
                listener,
                connections: Vec::new(),
            },
        )
    }

    /// 注入链路（测试）：直接包装一条已建立的链路（单端点语义）。
    pub fn over_link(
        link: Box<dyn Link + Send>,
        config: DirectGatewayConfig,
    ) -> Result<Self, GatewayError> {
        let endpoint = build_endpoint(&config, ServerLink::Injected(link))?;
        Self::start(
            config,
            DirectMode::Injected,
            IoState::Single {
                connection: DirectConnection::new(endpoint),
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
        let diagnostics = Arc::new(Mutex::new(GatewayDiagnostics::default()));
        let (requests_tx, requests_rx) = mpsc::channel();
        let io_snapshot = Arc::clone(&snapshot);
        let io_diagnostics = Arc::clone(&diagnostics);
        let io_config = config.clone();
        let io_worker = std::thread::Builder::new()
            .name(String::from(IO_THREAD_NAME))
            .spawn(move || {
                DirectIoWorker {
                    config: io_config,
                    state,
                    snapshot: io_snapshot,
                    diagnostics: io_diagnostics,
                    requests: requests_rx,
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
            diagnostics,
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
            if let Ok(mut diagnostics) = self.diagnostics.lock() {
                *diagnostics = GatewayDiagnostics::default();
            }
        }
    }

    fn snapshot(&self) -> GatewaySnapshot {
        self.snapshot
            .lock()
            .map(|snapshot| snapshot.clone())
            .unwrap_or_else(|poisoned| poisoned.into_inner().clone())
    }

    /// 控制面诊断快照读取（IO 线程每轮刷新，直接 clone）。
    fn diagnostics_snapshot(&self) -> GatewayDiagnostics {
        self.diagnostics
            .lock()
            .map(|diagnostics| diagnostics.clone())
            .unwrap_or_else(|poisoned| poisoned.into_inner().clone())
    }

    fn worker_alive(&self) -> bool {
        self.io_worker
            .as_ref()
            .map(|io_worker| !io_worker.is_finished())
            .unwrap_or(false)
    }

    /// 提交命令的公共实现：规整 DDL/超时、发送 IO op、有界等待 cmd_id。
    fn dispatch_request(
        &mut self,
        mut request: CommandRequest,
        management: bool,
    ) -> Result<CommandReceipt, GatewayError> {
        if request.ddl_ms == 0 {
            request.ddl_ms = self.config.default_ddl_ms.max(1);
        }
        if request.timeout_ms.is_none() {
            request.timeout_ms = match request.kind {
                CommandKind::Tool => self.config.tool_timeout_ms,
                CommandKind::Task | CommandKind::Jump => self.config.task_timeout_ms,
            };
        }
        let budget = call_budget_ms(
            self.config.default_ddl_ms,
            self.config.tool_timeout_ms,
            self.config.task_timeout_ms,
            &request,
        );
        let (reply_tx, reply_rx) = mpsc::channel();
        let (accepted_tx, accepted_rx) = mpsc::channel();
        self.requests
            .send(GatewayOp::Call {
                request,
                reply: reply_tx,
                accepted: accepted_tx,
                management,
            })
            .map_err(|_| GatewayError::new("link_lost", "直连 IO 线程已退出"))?;
        // 有界等待 cmd_id（取消契约）：超时/失败都返回回执，最终事件语义不变。
        let cmd_id = accepted_rx
            .recv_timeout(Duration::from_millis(SUBMIT_ACCEPTED_TIMEOUT_MS))
            .unwrap_or(0);
        Ok(CommandReceipt::with_cmd_id(
            reply_rx, budget, "直连", cmd_id,
        ))
    }

    /// 发送 Cancel 操作并等待 IO 线程确认（有界）。
    fn dispatch_cancel(&mut self, cmd_id: u64) -> Result<(), GatewayError> {
        let (reply_tx, reply_rx) = mpsc::channel();
        self.requests
            .send(GatewayOp::Cancel {
                cmd_id,
                reply: reply_tx,
            })
            .map_err(|_| GatewayError::new("link_lost", "直连 IO 线程已退出"))?;
        match reply_rx.recv_timeout(Duration::from_millis(CANCEL_ACK_TIMEOUT_MS)) {
            Ok(result) => result,
            Err(RecvTimeoutError::Timeout) => {
                Err(GatewayError::new("timeout", "等待直连 IO 线程确认取消超时"))
            }
            Err(RecvTimeoutError::Disconnected) => {
                Err(GatewayError::new("link_lost", "直连 IO 线程已退出"))
            }
        }
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

    fn submit(&mut self, request: CommandRequest) -> Result<CommandReceipt, GatewayError> {
        self.dispatch_request(request, false)
    }

    fn submit_control(&mut self, request: CommandRequest) -> Result<CommandReceipt, GatewayError> {
        self.dispatch_request(request, true)
    }

    fn cancel(&mut self, cmd_id: u64) -> Result<(), GatewayError> {
        self.dispatch_cancel(cmd_id)
    }

    fn diagnostics(&mut self) -> GatewayDiagnostics {
        self.diagnostics_snapshot()
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
    diagnostics: Arc<Mutex<GatewayDiagnostics>>,
    requests: Receiver<GatewayOp>,
    start: Instant,
}

impl DirectIoWorker {
    fn run(mut self) {
        loop {
            if self.pump_ops() {
                break;
            }
            let now_unix = now_ms();
            let now_mono = self.now_mono_ms();
            // 本轮要记录的 last_error（在 state 借用结束后统一写入）。
            let mut notes: Vec<String> = Vec::new();
            // Single 端点链路关闭：借用结束后把状态切到 Closed。
            let mut single_closed = false;
            match &mut self.state {
                IoState::Listening {
                    listener,
                    connections,
                } => {
                    // 1) 排空待接受的连接；没有连接时用限时 accept 给线程留出
                    //    处理 Shutdown/Call 的节奏。监听器常驻，永不移交。
                    loop {
                        let accept_wait = if connections.is_empty() {
                            Duration::from_millis(IDLE_POLL_MS)
                        } else {
                            Duration::ZERO
                        };
                        match listener.accept_link_timeout(accept_wait) {
                            Ok(Some(link)) => {
                                match build_endpoint(&self.config, ServerLink::Tcp(link)) {
                                    Ok(endpoint) => {
                                        connections.push(DirectConnection::new(endpoint));
                                    }
                                    Err(error) => {
                                        notes.push(format!("建立直连端点失败：{error}"));
                                    }
                                }
                            }
                            Ok(None) => break,
                            Err(error) => {
                                notes.push(format!("accept 失败：{error}"));
                                break;
                            }
                        }
                    }
                    // 2) 泵每条连接（入站帧 → ControlPlane → 出站帧）。
                    for connection in connections.iter_mut() {
                        if let Err(error) = connection.endpoint.pump(now_unix, now_mono) {
                            notes.push(format!("控制平面错误：{error}"));
                        }
                    }
                    // 3) 回收事件（先回复已完成的命令，再处理断链）。
                    for connection in connections.iter_mut() {
                        connection.drain_events();
                    }
                    // 4) 断开的连接 + 同一 worker_id 被新连接顶替的旧连接。
                    let mut removals: Vec<(usize, &'static str, String)> = Vec::new();
                    for (index, connection) in connections.iter().enumerate() {
                        if connection.endpoint.is_open() {
                            continue;
                        }
                        if let Some(last_error) = connection.endpoint.last_error() {
                            notes.push(format!("直连链路断开：{last_error}"));
                        }
                        removals.push((
                            index,
                            "link_lost",
                            String::from("直连链路断开，等待 Worker 重连"),
                        ));
                    }
                    // 按连接建立顺序扫描：先入表者让位给后来者（后来者胜）。
                    let mut owner: HashMap<String, usize> = HashMap::new();
                    for (index, connection) in connections.iter().enumerate() {
                        for worker in connection.endpoint.workers() {
                            if let Some(previous) = owner.insert(worker.worker_id.clone(), index)
                                && previous != index
                            {
                                notes.push(format!(
                                    "Worker {} 的新连接顶替了旧连接",
                                    worker.worker_id
                                ));
                                removals.push((
                                    previous,
                                    "link_lost",
                                    format!(
                                        "Worker {} 已被新连接顶替，旧连接关闭",
                                        worker.worker_id
                                    ),
                                ));
                            }
                        }
                    }
                    removals.sort_by_key(|(index, _, _)| *index);
                    removals.dedup_by_key(|(index, _, _)| *index);
                    // 5) 关闭并移除（倒序删除避免下标漂移）。
                    for (index, code, message) in removals.into_iter().rev() {
                        let mut connection = connections.remove(index);
                        connection.endpoint.close(now_unix);
                        connection.fail_waiters(code, &message);
                    }
                    // 无连接时不要空转：限时 accept 依赖 listener 状态，此处已
                    // 在下一轮 1) 中以 IDLE_POLL_MS 限时等待。
                }
                IoState::Single { connection } => {
                    if let Err(error) = connection.endpoint.pump(now_unix, now_mono) {
                        notes.push(format!("控制平面错误：{error}"));
                    }
                    connection.drain_events();
                    if !connection.endpoint.is_open() {
                        if let Some(last_error) = connection.endpoint.last_error() {
                            notes.push(format!("直连链路断开：{last_error}"));
                        }
                        connection.fail_waiters("link_lost", "直连链路已断开");
                        single_closed = true;
                    }
                }
                IoState::Closed => match self
                    .requests
                    .recv_timeout(Duration::from_millis(IDLE_POLL_MS))
                {
                    Ok(op) => {
                        if self.handle_op(op) {
                            break;
                        }
                    }
                    Err(RecvTimeoutError::Timeout) | Err(RecvTimeoutError::Disconnected) => {}
                },
            }
            if single_closed {
                self.state = IoState::Closed;
            }
            for message in notes {
                self.note(message);
            }
            self.publish_snapshot();
            self.publish_diagnostics();
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
            GatewayOp::Call {
                request,
                reply,
                accepted,
                management,
            } => self.dispatch_call(request, reply, accepted, management),
            GatewayOp::Cancel { cmd_id, reply } => self.dispatch_cancel(cmd_id, reply),
            GatewayOp::Shutdown => return true,
        }
        false
    }

    /// 派工：按 `worker_id` 路由到属主连接。
    fn dispatch_call(
        &mut self,
        request: CommandRequest,
        reply: Sender<Result<CommandResult, GatewayError>>,
        accepted: Sender<u64>,
        management: bool,
    ) {
        let now_unix = now_ms();
        let now_mono = self.now_mono_ms();
        let worker_id = request.worker_id.clone();
        match &mut self.state {
            IoState::Listening { connections, .. } => {
                let target = connections.iter().position(|connection| {
                    connection
                        .endpoint
                        .worker(&worker_id)
                        .map(|worker| worker.online)
                        .unwrap_or(false)
                });
                match target {
                    Some(index) => {
                        match connections[index].submit(request, management, now_unix, now_mono) {
                            Ok(cmd_id) => {
                                // 先回传 cmd_id（调用方据此取消），再等最终事件。
                                let _ = accepted.send(cmd_id);
                                connections[index].waiters.insert(cmd_id, reply);
                            }
                            Err(error) => {
                                let _ = reply.send(Err(error));
                            }
                        }
                    }
                    None => {
                        let any_open = connections
                            .iter()
                            .any(|connection| connection.endpoint.is_open());
                        let error = if any_open {
                            GatewayError::new(
                                "worker_not_found",
                                format!("Worker {worker_id} 不存在"),
                            )
                        } else {
                            GatewayError::new("link_lost", "直连链路未就绪（等待 Worker 连入）")
                        };
                        let _ = reply.send(Err(error));
                    }
                }
            }
            IoState::Single { connection } => {
                if !connection.endpoint.is_open() {
                    let _ = reply.send(Err(GatewayError::new("link_lost", "直连链路已断开")));
                } else {
                    match connection.submit(request, management, now_unix, now_mono) {
                        Ok(cmd_id) => {
                            let _ = accepted.send(cmd_id);
                            connection.waiters.insert(cmd_id, reply);
                        }
                        Err(error) => {
                            let _ = reply.send(Err(error));
                        }
                    }
                }
            }
            IoState::Closed => {
                let _ = reply.send(Err(GatewayError::new("link_lost", "直连链路已断开")));
            }
        }
        self.publish_snapshot();
    }

    /// 取消：找到 cmd_id 的属主连接后透传 `ControlPlane::cancel`。
    fn dispatch_cancel(&mut self, cmd_id: u64, reply: Sender<Result<(), GatewayError>>) {
        let now_unix = now_ms();
        let now_mono = self.now_mono_ms();
        let result = match &mut self.state {
            IoState::Listening { connections, .. } => {
                let any_open = connections
                    .iter()
                    .any(|connection| connection.endpoint.is_open());
                let target = connections
                    .iter()
                    .position(|connection| connection.waiters.contains_key(&cmd_id));
                match (target, any_open) {
                    (Some(index), _) => connections[index].cancel(cmd_id, now_unix, now_mono),
                    (None, true) => Err(GatewayError::new(
                        "unknown_command",
                        format!("命令 {cmd_id} 不在在途账本中"),
                    )),
                    (None, false) => Err(GatewayError::new(
                        "link_lost",
                        "直连链路未就绪（等待 Worker 连入）",
                    )),
                }
            }
            IoState::Single { connection } => {
                if connection.endpoint.is_open() {
                    connection.cancel(cmd_id, now_unix, now_mono)
                } else {
                    Err(GatewayError::new("link_lost", "直连链路已断开"))
                }
            }
            IoState::Closed => Err(GatewayError::new("link_lost", "直连链路已断开")),
        };
        let _ = reply.send(result);
        self.publish_snapshot();
    }

    fn note(&mut self, message: String) {
        if let Ok(mut snapshot) = self.snapshot.lock() {
            snapshot.last_error = Some(message);
        }
    }

    fn publish_snapshot(&mut self) {
        let (workers, ready, connected) = match &self.state {
            IoState::Listening { connections, .. } => {
                let mut workers = Vec::new();
                for connection in connections {
                    workers.extend(connection.endpoint.workers());
                }
                // 任一端点就绪即 ready（多 Worker 只需连上一个即可派工）；
                // connected 表示至少有一条链路打开。
                let ready = connections.iter().any(|connection| {
                    connection.endpoint.is_ready() && connection.endpoint.is_open()
                });
                let connected = connections
                    .iter()
                    .any(|connection| connection.endpoint.is_open());
                (workers, ready, connected)
            }
            IoState::Single { connection } => (
                connection.endpoint.workers(),
                connection.endpoint.is_ready() && connection.endpoint.is_open(),
                connection.endpoint.is_open(),
            ),
            IoState::Closed => (Vec::new(), false, false),
        };
        if let Ok(mut snapshot) = self.snapshot.lock() {
            snapshot.workers = workers;
            snapshot.ready = ready;
            snapshot.connected = connected;
        }
    }

    /// 聚合各端点的 ControlPlane 诊断（多端点时合并 inflight/late_results/计数）。
    fn publish_diagnostics(&mut self) {
        let now_mono = self.now_mono_ms();
        let merged = match &self.state {
            IoState::Listening { connections, .. } => {
                let mut merged = GatewayDiagnostics::default();
                let mut clocks_synced = !connections.is_empty();
                for connection in connections {
                    let diagnostics = connection.endpoint.plane().diagnostics(now_mono);
                    if merged.last_error.is_none() {
                        merged.last_error = diagnostics.last_error;
                    }
                    merged.inflight.extend(diagnostics.inflight);
                    merged.late_results.extend(diagnostics.late_results);
                    merged.counters.late_results = merged
                        .counters
                        .late_results
                        .saturating_add(diagnostics.counters.late_results);
                    merged.counters.dropped_events = merged
                        .counters
                        .dropped_events
                        .saturating_add(diagnostics.counters.dropped_events);
                    clocks_synced &= diagnostics.clock_synced;
                }
                merged.clock_synced = clocks_synced;
                merged
            }
            IoState::Single { connection } => connection.endpoint.plane().diagnostics(now_mono),
            IoState::Closed => GatewayDiagnostics::default(),
        };
        if let Ok(mut slot) = self.diagnostics.lock() {
            *slot = merged;
        }
    }

    fn now_mono_ms(&self) -> u64 {
        self.start.elapsed().as_millis() as u64
    }

    fn shutdown_state(&mut self) {
        let now_unix = now_ms();
        match &mut self.state {
            IoState::Listening { connections, .. } => {
                for connection in connections.iter_mut() {
                    connection.endpoint.close(now_unix);
                    connection.fail_waiters("link_lost", "网关已关闭");
                }
                connections.clear();
            }
            IoState::Single { connection } => {
                connection.endpoint.close(now_unix);
                connection.fail_waiters("link_lost", "网关已关闭");
            }
            IoState::Closed => {}
        }
        self.state = IoState::Closed;
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
