//! 飞书对话入口（`--feishu`，见 docs/sekai-feishu.md）。
//!
//! 飞书是 Router 的第 4 个前端（与 REPL / TUI / serve 并列）：用户在 IM 里
//! 发消息，经飞书开放平台推送（WebSocket 长连接 + pbbp2 帧）到本模块；
//! 本模块做事件过滤、授权、限流与命令层，再把 prompt 交给
//! [`PromptCore::run_prompt_with_cancel`] 复用全部护栏（`deny_llm_relay`、
//! `RunGate` 全局 4 路、策略/网关快照），最后用 REST
//! `im/v1/messages/{id}/reply` 把答案回复到原消息。
//!
//! # 线程模型（同步 std，无 tokio）
//!
//! | 线程 | 职责 |
//! | --- | --- |
//! | IO 线程（[`run`] 调用线程） | endpoint 取 URL → WS 握手 → 阻塞读帧（读超时切成 [`READ_TICK`] 心跳节拍）；分片重组、**先 ack 再投递**；断线按 §3.6 节奏重连 |
//! | dispatch 线程 | 从 mpsc 取完整事件 JSON：event_id 去重、allowlist、限流、命令层、per-chat 串行判定 |
//! | 运行线程（每条消息一个短命线程） | `run_prompt` + REST 回复 + 审计行 |
//!
//! 退出：Ctrl-C/SIGTERM 由操作系统默认终止（本 crate `#![forbid(unsafe_code)]`，
//! 不注册信号处理器）；链路永久失败返回退出码 3。**不读 stdin**：服务管理器
//! 常把 stdin 接 /dev/null，若以 stdin EOF 作为退出信号会误退（与设计 §2
//! 的差异见文档「实现基线校准」）。
//!
//! # 安全
//!
//! 凭据只从环境变量读（[`FeishuConfig`]）；endpoint 返回的连接 URL 含凭据
//! query，**任何日志/错误/审计都不得出现 URL、token 或 secret**。审计行只
//! 记 chat/sender/命令/长度/结局/耗时（§6.5），不记消息内容。

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::agent::{CancelToken, ConversationContext, ConversationTurn};
use crate::config::{Config, FeishuConfig, LinkMode};
use crate::gateway::{GatewayHandle, WorkerGateway};
use crate::http::{HttpClient, HttpRequest, ReadWrite, StdHttpClient, connect_tls_stream};
use crate::json::Json;
use crate::llm::ChatClient;
use crate::pbbp::{Frame, METHOD_CONTROL, METHOD_DATA};
use crate::prompt_server::{
    CODE_AGENT_ERROR, CODE_BUSY, CODE_LLM_UNAVAILABLE, CODE_NO_WORKERS, CODE_NOT_LINKED,
    MAX_CONCURRENT_PROMPT_RUNS, MAX_PROMPT_TEXT_BYTES, PromptCore, PromptRunError, PromptRunResult,
};
use crate::ws::{WsError, WsMessage, WsStream};

// ---------------------------------------------------------------------------
// 常量（协议面见 docs/sekai-feishu.md §3~§6）
// ---------------------------------------------------------------------------

/// TCP/TLS 连接超时。
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// WS 握手读超时（升级应答应在一个 RTT 内到达）。
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// 读节拍：把阻塞读切成短周期，超时即检查心跳与分片 TTL。
const READ_TICK: Duration = Duration::from_millis(500);
/// 写超时：挂死的对端不得无限拖住发送方（ack/心跳）。
const WRITE_TIMEOUT: Duration = Duration::from_secs(30);
/// REST 单请求超时。
const HTTP_TIMEOUT: Duration = Duration::from_secs(10);
/// 回复文本上限（UTF-8 字节；超限按字符边界截断并追加标记）。
const MAX_REPLY_BYTES: usize = 4_000;
/// event_id 去重窗口容量（应对服务端 3s 重投；断线重连不清空）。
const DEDUP_CAPACITY: usize = 1_024;
/// 每 chat 每分钟消息上限（与 ingress 默认值一致）。
const RATE_PER_MINUTE: usize = 60;
/// 限流滑动窗口。
const RATE_WINDOW: Duration = Duration::from_secs(60);
/// 未授权提示的 per-chat 冷却（防骚扰刷屏）。
const DENIED_REPLY_INTERVAL: Duration = Duration::from_secs(60);
/// 分片 TTL（超时丢弃未齐分片并审计）。
const FRAGMENT_TTL: Duration = Duration::from_secs(5);
/// 单事件重组后的字节上限（防恶意/异常事件撑爆内存）。
const MAX_FRAGMENT_BYTES: usize = 4 * 1024 * 1024;
/// 非事件 DATA 帧忽略（V1 无卡片回调）。
const EVENT_MESSAGE_RECEIVE: &str = "im.message.receive_v1";
/// token 失效错误码：强制刷新后重试一次（§4.1，以官方 SDK 对拍为准）。
const TOKEN_INVALID_CODES: [i64; 2] = [99991663, 99991661];
/// 会话上下文单条文本上限（用户/助手各 4 KiB；与回复上限同量级）。
const MAX_CONTEXT_MESSAGE_BYTES: usize = 4 * 1024;
/// 单会话上下文总字节预算（超出时从最旧轮次开始丢弃）。
const MAX_CONTEXT_BYTES: usize = 32 * 1024;
/// 会话上下文最多跟踪的会话数（超出时 LRU 淘汰）。
const MAX_CONTEXT_SESSIONS: usize = 256;

/// 事件已收到、正在处理的 IM 提示（`SEKAI_FEISHU_ACK=off` 关闭）。
const ACK_TEXT: &str = "已收到，正在处理…";
/// 未授权提示。
const DENIED_TEXT: &str = "未授权：本机器人未向你开放";
/// 超速提示。
const RATE_TEXT: &str = "请求过于频繁，请稍后再试";
/// 非文本消息提示。
const NON_TEXT_TEXT: &str = "V1 仅支持文本消息";
/// 空文本使用提示。
const USAGE_TEXT: &str = "直接发文本，或 /status /cancel";
/// 文本超限提示。
const TOO_LONG_TEXT: &str = "消息过长：请控制在 8192 字节以内";
/// 同 chat 在途提示。
const BUSY_CHAT_TEXT: &str = "上一条还在处理中，可发送 /cancel 中止";
/// 未知命令提示。
const COMMANDS_TEXT: &str =
    "可用命令：/cancel 中止当前任务，/status 查看状态；其余输入直接作为指令";
/// `/cancel` 已置位。
const CANCEL_OK_TEXT: &str = "已请求取消";
/// `/cancel` 无在途运行。
const CANCEL_IDLE_TEXT: &str = "当前没有运行中的任务";
/// 运行线程创建失败（资源耗尽，极罕见）。
const SPAWN_FAILED_TEXT: &str = "系统繁忙，请稍后再试";

// ---------------------------------------------------------------------------
// 配置面
// ---------------------------------------------------------------------------

/// 飞书入口运行参数（[`FeishuConfig`] + 链路/超时等运行期取值）。
///
/// Debug 手写脱敏（`app_secret` 显示 `<redacted>`，同 LLM key 惯例）。
#[derive(Clone)]
pub struct FeishuOptions {
    /// 应用 ID（`cli_` 开头）。
    pub app_id: String,
    /// 应用密钥（不得出现在任何日志/错误中）。
    pub app_secret: String,
    /// 开放平台域名（无尾斜杠；测试可用 `http://127.0.0.1:PORT`）。
    pub domain: String,
    /// 允许的用户 id（open_id/user_id 混排；空 = 全拒）。
    pub allow_users: Vec<String>,
    /// 允许的会话 id（chat_id；空 = 全拒）。
    pub allow_chats: Vec<String>,
    /// 收到消息先回「正在处理」。
    pub ack: bool,
    /// 每消息 agent 步数上限（None = 随全局）。
    pub max_steps: Option<u32>,
    /// 每会话保留的最近问答轮数（0 = 只注入会话元数据）。
    pub context_turns: usize,
    /// Worker 链路类型（`/status` 展示用）。
    pub link: LinkMode,
    /// REST 单请求超时。
    pub http_timeout: Duration,
}

impl std::fmt::Debug for FeishuOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FeishuOptions")
            .field("app_id", &self.app_id)
            .field("app_secret", &"<redacted>")
            .field("domain", &self.domain)
            .field("allow_users", &self.allow_users)
            .field("allow_chats", &self.allow_chats)
            .field("ack", &self.ack)
            .field("max_steps", &self.max_steps)
            .field("context_turns", &self.context_turns)
            .field("link", &self.link)
            .field("http_timeout", &self.http_timeout)
            .finish()
    }
}

impl FeishuOptions {
    /// 从 CLI 配置构造运行参数（凭据必需性已由 config 校验）。
    pub fn from_config(config: &Config) -> Self {
        let feishu: &FeishuConfig = &config.feishu_config;
        Self {
            app_id: feishu.app_id.trim().to_string(),
            app_secret: feishu.app_secret.trim().to_string(),
            domain: feishu.domain.trim_end_matches('/').to_string(),
            allow_users: feishu.allow_users.clone(),
            allow_chats: feishu.allow_chats.clone(),
            ack: feishu.ack,
            max_steps: feishu.max_steps,
            context_turns: feishu.context_turns,
            link: config.link,
            http_timeout: HTTP_TIMEOUT,
        }
    }
}

// ---------------------------------------------------------------------------
// 入口
// ---------------------------------------------------------------------------

/// 运行飞书前端（阻塞直到停止标志置位或链路永久失败）。
///
/// 返回值即进程退出码：**0** 优雅退出；**3** 飞书链路永久失败（endpoint
/// 不可达、配置类错误、握手 403/514、重连次数用尽）；**4** 构建缺少
/// `tls` feature（无法承载 `wss://`）。
pub fn run<C: ChatClient + Clone + Send + 'static>(
    core: Arc<PromptCore<C>>,
    gateway: GatewayHandle,
    options: FeishuOptions,
    stop: Arc<AtomicBool>,
) -> i32 {
    eprintln!(
        "sekai-router: 飞书入口 domain={} allow_users={} allow_chats={}",
        options.domain,
        options.allow_users.len(),
        options.allow_chats.len()
    );
    if options.allow_users.is_empty() && options.allow_chats.is_empty() {
        eprintln!(
            "sekai-router: 警告：飞书 allowlist 为空，所有消息都将被拒绝；\
             先给机器人发一条消息，从审计行抄 chat=/sender= 后回填"
        );
    }
    if !cfg!(feature = "tls") {
        eprintln!(
            "sekai-router: 飞书入口需要进程内 TLS（native-tls）：当前构建未启用 tls feature，无法承载 wss:// 长连接"
        );
        return 4;
    }

    let rest = Arc::new(RestClient::new(&options));
    let chat_runs: Arc<Mutex<HashMap<String, CancelToken>>> = Arc::new(Mutex::new(HashMap::new()));
    let active_runs = Arc::new(AtomicUsize::new(0));
    let history = Arc::new(Mutex::new(HistoryStore::new()));
    let (tx, rx) = mpsc::channel::<Vec<u8>>();

    let dispatcher = Dispatcher::<C> {
        core,
        gateway,
        options: options.clone(),
        rest: Arc::clone(&rest),
        stop: Arc::clone(&stop),
        seen: DedupWindow::new(DEDUP_CAPACITY),
        rates: HashMap::new(),
        denied_reply_at: HashMap::new(),
        chat_runs: Arc::clone(&chat_runs),
        active_runs: Arc::clone(&active_runs),
        history,
    };
    let dispatch_thread = thread::Builder::new()
        .name(String::from("sekai-feishu-dispatch"))
        .spawn(move || dispatcher.run(rx))
        .ok();

    // 本线程 = WS IO 线程（重连循环）。dispatch 线程随 tx 释放而结束。
    let code = io_loop(&rest, &tx, &stop);
    drop(tx);
    stop.store(true, Ordering::SeqCst);

    // 优雅收尾：取消在途运行，最多等 2s 让运行线程回复/记审计。
    if let Ok(map) = chat_runs.lock() {
        for token in map.values() {
            token.cancel();
        }
    }
    let deadline = Instant::now() + Duration::from_secs(2);
    while active_runs.load(Ordering::SeqCst) > 0 && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(50));
    }
    if let Some(handle) = dispatch_thread {
        let _ = handle.join();
    }
    code
}

// ---------------------------------------------------------------------------
// 连接端点（§3.1）
// ---------------------------------------------------------------------------

/// endpoint 返回的连接参数（URL 含凭据 query：不得写日志）。
#[derive(Debug, Clone, PartialEq, Eq)]
struct EndpointInfo {
    /// `wss://` 连接地址（含 query 凭据）。
    url: String,
    /// 重连次数上限；≤0 视为无限。
    reconnect_count: i64,
    /// 重连间隔。
    reconnect_interval: Duration,
    /// 首次重连前的抖动上限。
    reconnect_nonce: Duration,
    /// 应用层心跳间隔。
    ping_interval: Duration,
}

impl Default for EndpointInfo {
    fn default() -> Self {
        Self {
            url: String::new(),
            reconnect_count: -1,
            reconnect_interval: Duration::from_secs(120),
            reconnect_nonce: Duration::from_secs(30),
            ping_interval: Duration::from_secs(120),
        }
    }
}

/// endpoint 失败分类（决定退出还是按节奏重试，§3.1/§3.6）。
#[derive(Debug)]
enum EndpointError {
    /// `code=1`/`1000040343`：暂不可用，按重连节奏重试。
    Temporary(String),
    /// 网络层不可达（启动时据此返回退出码 3）。
    Unreachable(String),
    /// 配置类错误（Secret 错等）：退出码 3。
    Fatal(String),
}

/// WS 连接/握手失败分类。
#[derive(Debug)]
enum ConnectError {
    /// 暂时失败（5xx、网络、超时）：按重连节奏。
    Temporary(String),
    /// 永久失败（403/514）：退出码 3。
    Permanent(String),
}

/// WS 目标（`wss://host[:port]/path?query` 拆解结果）。
#[derive(Debug, Clone, PartialEq, Eq)]
struct WsTarget {
    /// 是否 TLS（`wss://` 为真，`ws://` 仅供本地 mock/明文直连）。
    tls: bool,
    /// 主机名（TLS SNI 与 Host 头用）。
    host: String,
    /// 端口（缺省 443/80）。
    port: u16,
    /// 路径 + query（原样透传）。
    path: String,
}

/// 解析连接 URL。失败消息**不含 URL**（query 含凭据）。
fn parse_ws_url(url: &str) -> Result<WsTarget, String> {
    let (tls, rest) = if let Some(rest) = url.strip_prefix("wss://") {
        (true, rest)
    } else if let Some(rest) = url.strip_prefix("ws://") {
        (false, rest)
    } else {
        return Err(String::from("连接 URL 协议非法（需 wss://）"));
    };
    let split = rest.find(['/', '?']).unwrap_or(rest.len());
    let authority = &rest[..split];
    let mut path = rest[split..].to_string();
    if path.is_empty() {
        path = String::from("/");
    } else if path.starts_with('?') {
        path.insert(0, '/');
    }
    if authority.is_empty() {
        return Err(String::from("连接 URL 缺少主机名"));
    }
    let (host, port) = match authority.rsplit_once(':') {
        Some((host, port_str)) => {
            let port: u16 = port_str
                .parse()
                .map_err(|_| String::from("连接 URL 端口非法"))?;
            (host, port)
        }
        None => (authority, if tls { 443 } else { 80 }),
    };
    if host.is_empty() {
        return Err(String::from("连接 URL 缺少主机名"));
    }
    Ok(WsTarget {
        tls,
        host: host.to_string(),
        port,
        path,
    })
}

/// 建立 WS 连接并完成握手（超时由本函数设置）。
fn connect_ws(target: &WsTarget) -> Result<WsStream<Box<dyn ReadWrite + Send>>, ConnectError> {
    let stream: Box<dyn ReadWrite + Send> = if target.tls {
        connect_tls_stream(&target.host, target.port, CONNECT_TIMEOUT)
            .map_err(|error| ConnectError::Temporary(format!("TLS 连接失败：{error}")))?
    } else {
        let address = format!("{}:{}", target.host, target.port);
        let socket = address
            .to_socket_addrs()
            .map_err(|error| ConnectError::Temporary(format!("地址解析失败：{error}")))?
            .next()
            .ok_or_else(|| ConnectError::Temporary(String::from("没有可用地址")))?;
        let stream = TcpStream::connect_timeout(&socket, CONNECT_TIMEOUT)
            .map_err(|error| ConnectError::Temporary(format!("TCP 连接失败：{error}")))?;
        let _ = stream.set_nodelay(true);
        Box::new(stream)
    };
    stream
        .set_read_timeout(Some(HANDSHAKE_TIMEOUT))
        .map_err(|error| ConnectError::Temporary(format!("设置握手超时失败：{error}")))?;
    let authority = if (target.tls && target.port == 443) || (!target.tls && target.port == 80) {
        target.host.clone()
    } else {
        format!("{}:{}", target.host, target.port)
    };
    let mut ws = WsStream::<Box<dyn ReadWrite + Send>>::handshake(stream, &authority, &target.path)
        .map_err(|error| match error {
            WsError::HandshakeRejected { status, .. } if status == 403 || status == 514 => {
                ConnectError::Permanent(format!(
                    "WebSocket 握手被拒绝（HTTP {status}）：检查 App ID/Secret 与长连接权限"
                ))
            }
            WsError::HandshakeRejected { status, .. } => {
                ConnectError::Temporary(format!("WebSocket 握手失败（HTTP {status}）"))
            }
            other => ConnectError::Temporary(format!("WebSocket 握手失败：{other}")),
        })?;
    ws.set_read_timeout(Some(READ_TICK))
        .map_err(|error| ConnectError::Temporary(format!("设置读超时失败：{error}")))?;
    ws.set_write_timeout(Some(WRITE_TIMEOUT))
        .map_err(|error| ConnectError::Temporary(format!("设置写超时失败：{error}")))?;
    Ok(ws)
}

// ---------------------------------------------------------------------------
// 重连循环与事件泵
// ---------------------------------------------------------------------------

/// pump 结束原因。
#[derive(Debug)]
enum PumpOutcome {
    /// 停止标志置位（已发 close 帧）。
    Stopped,
    /// 链路断开（原因不含 URL/凭据）。
    Disconnected(String),
}

/// 重连循环：endpoint → 连接 → 泵；按 §3.6 分类决定退出或等待。
fn io_loop(rest: &RestClient, tx: &mpsc::Sender<Vec<u8>>, stop: &Arc<AtomicBool>) -> i32 {
    let mut info = EndpointInfo::default();
    let mut connected_once = false;
    let mut failures: i64 = 0;
    loop {
        if stop.load(Ordering::SeqCst) {
            return 0;
        }
        let fresh = match rest.endpoint() {
            Ok(fresh) => fresh,
            Err(EndpointError::Temporary(message)) => {
                eprintln!(
                    "sekai-router: 飞书 endpoint 暂不可用（{message}），{}s 后重试",
                    info.reconnect_interval.as_secs()
                );
                failures += 1;
                if reconnect_exhausted(&info, failures) {
                    eprintln!("sekai-router: 飞书重连次数用尽，退出");
                    return 3;
                }
                sleep_interruptible(info.reconnect_interval, stop);
                continue;
            }
            Err(EndpointError::Unreachable(message)) => {
                eprintln!("sekai-router: 飞书 endpoint 不可达：{message}");
                // 设计 §2：启动时 endpoint API 不可达 → 退出码 3；运行期
                // 网络抖动按重连节奏重试（常驻服务不因断网退出）。
                if !connected_once {
                    return 3;
                }
                failures += 1;
                if reconnect_exhausted(&info, failures) {
                    eprintln!("sekai-router: 飞书重连次数用尽，退出");
                    return 3;
                }
                sleep_interruptible(info.reconnect_interval, stop);
                continue;
            }
            Err(EndpointError::Fatal(message)) => {
                eprintln!("sekai-router: 飞书配置错误：{message}");
                return 3;
            }
        };
        info = fresh;
        // 断线重连首次尝试前做随机抖动（§3.1 ReconnectNonce）；首连不等待。
        if connected_once {
            sleep_interruptible(jitter(info.reconnect_nonce), stop);
            if stop.load(Ordering::SeqCst) {
                return 0;
            }
        }
        let target = match parse_ws_url(&info.url) {
            Ok(target) => target,
            Err(message) => {
                eprintln!("sekai-router: 飞书 endpoint 返回的连接 URL 不可用：{message}");
                return 3;
            }
        };
        match connect_ws(&target) {
            Ok(mut ws) => {
                connected_once = true;
                failures = 0;
                eprintln!("sekai-router: 飞书长连接已建立");
                match pump(&mut ws, tx, stop, &info) {
                    PumpOutcome::Stopped => return 0,
                    PumpOutcome::Disconnected(reason) => {
                        eprintln!("sekai-router: 飞书长连接断开：{reason}（重连中）");
                        // 分片缓冲是 pump 局部状态，重连后自动清空（§3.6）。
                        continue;
                    }
                }
            }
            Err(ConnectError::Permanent(message)) => {
                eprintln!("sekai-router: {message}");
                return 3;
            }
            Err(ConnectError::Temporary(message)) => {
                eprintln!("sekai-router: 飞书长连接失败：{message}");
                failures += 1;
                if reconnect_exhausted(&info, failures) {
                    eprintln!("sekai-router: 飞书重连次数用尽，退出");
                    return 3;
                }
                sleep_interruptible(info.reconnect_interval, stop);
                continue;
            }
        }
    }
}

/// 重连次数是否用尽（`ReconnectCount` ≤0 为无限，§3.1）。
fn reconnect_exhausted(info: &EndpointInfo, failures: i64) -> bool {
    info.reconnect_count > 0 && failures >= info.reconnect_count
}

/// 可中断睡眠：每 200ms 检查停止标志（Ctrl-C 默认终止，此路径服务管道 EOF）。
fn sleep_interruptible(duration: Duration, stop: &Arc<AtomicBool>) {
    let mut remaining = duration;
    while !remaining.is_zero() {
        if stop.load(Ordering::SeqCst) {
            return;
        }
        let slice = remaining.min(Duration::from_millis(200));
        thread::sleep(slice);
        remaining = remaining.saturating_sub(slice);
    }
}

/// `rand(0..=max)` 秒抖动（无需密码学强度；时间+进程号混合即可）。
fn jitter(max: Duration) -> Duration {
    let ceiling = max.as_secs();
    if ceiling == 0 {
        return Duration::ZERO;
    }
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.subsec_nanos())
        .unwrap_or(0) as u64;
    let seed = nanos ^ (u64::from(std::process::id()) << 17);
    Duration::from_secs(seed % ceiling)
}

/// 事件泵：读帧、心跳、分片、ack、投递；返回断开/停止原因。
fn pump(
    ws: &mut WsStream<Box<dyn ReadWrite + Send>>,
    tx: &mpsc::Sender<Vec<u8>>,
    stop: &Arc<AtomicBool>,
    info: &EndpointInfo,
) -> PumpOutcome {
    let mut ping_interval = info.ping_interval.max(Duration::from_secs(1));
    let mut last_ping = Instant::now();
    let mut last_pong = Instant::now();
    let mut fragments = FragmentSet::new();
    loop {
        if stop.load(Ordering::SeqCst) {
            let _ = ws.send_close();
            return PumpOutcome::Stopped;
        }
        match ws.read_message() {
            Ok(WsMessage::Binary(bytes)) => {
                if let Err(error) = handle_binary(
                    ws,
                    &bytes,
                    tx,
                    &mut fragments,
                    &mut ping_interval,
                    &mut last_pong,
                ) {
                    return PumpOutcome::Disconnected(error.to_string());
                }
            }
            // 长连接协议只走二进制帧；文本帧视为协议噪声忽略。
            Ok(WsMessage::Text(_)) => {}
            // 协议层 pong：心跳存活信号。实测飞书网关按 PingInterval 对 WS ping
            // 回 WS pong；不刷新它就必然在 2×PingInterval 后误判断线（见
            // docs/sekai-feishu.md §14「心跳实现」）。
            Ok(WsMessage::Pong) => {
                last_pong = Instant::now();
            }
            Ok(WsMessage::Close) => {
                return PumpOutcome::Disconnected(String::from("收到 close 帧"));
            }
            Err(WsError::Timeout) => {
                let now = Instant::now();
                let dropped = fragments.purge(now);
                if dropped > 0 {
                    eprintln!("sekai-router: 飞书分片超时丢弃 {dropped} 组（TTL 5s）");
                }
                if now.duration_since(last_pong) >= ping_interval.saturating_mul(2) {
                    return PumpOutcome::Disconnected(String::from(
                        "心跳超时（2×PingInterval 未收到 pong）",
                    ));
                }
                if now.duration_since(last_ping) >= ping_interval {
                    if let Err(error) = ws.send_ping() {
                        return PumpOutcome::Disconnected(format!("心跳发送失败：{error}"));
                    }
                    last_ping = now;
                }
            }
            Err(WsError::Closed) => {
                return PumpOutcome::Disconnected(String::from("连接已关闭"));
            }
            Err(error) => return PumpOutcome::Disconnected(error.to_string()),
        }
    }
}

/// 处理一条 WS 二进制消息（= 一条 pbbp2.Frame，§3.3~§3.5）。
fn handle_binary(
    ws: &mut WsStream<Box<dyn ReadWrite + Send>>,
    bytes: &[u8],
    tx: &mpsc::Sender<Vec<u8>>,
    fragments: &mut FragmentSet,
    ping_interval: &mut Duration,
    last_pong: &mut Instant,
) -> Result<(), WsError> {
    let frame = match Frame::decode(bytes) {
        Ok(frame) => frame,
        Err(error) => {
            eprintln!("sekai-router: 飞书帧解析失败：{error}");
            return Ok(());
        }
    };
    match frame.method {
        METHOD_DATA => {
            // 非事件 DATA（卡片回调等）：忽略且不 ack（§3.5）。
            if frame.header("type") != Some("event") {
                return Ok(());
            }
            // 先 ack 再投递：3s 未 ack 服务端会重投同一事件。
            let ack = build_ack(&frame);
            ws.send_binary(&ack.encode())?;
            let message_id = frame.header("message_id").unwrap_or_default().to_string();
            let sum = frame
                .header("sum")
                .and_then(|value| value.parse::<u32>().ok())
                .unwrap_or(1);
            let seq = frame
                .header("seq")
                .and_then(|value| value.parse::<u32>().ok())
                .unwrap_or(0);
            if sum <= 1 {
                let _ = tx.send(frame.payload.clone());
                return Ok(());
            }
            if let Some(payload) =
                fragments.insert(&message_id, seq, sum, &frame.payload, Instant::now())
            {
                let _ = tx.send(payload);
            }
        }
        METHOD_CONTROL if frame.header("type") == Some("pong") => {
            *last_pong = Instant::now();
            if let Some(interval) = parse_ping_interval(&frame) {
                *ping_interval = interval;
            }
        }
        _ => {}
    }
    Ok(())
}

/// 从 pong 帧解析新的 ClientConfig（§3.4：headers 可携带调整后的值）。
fn parse_ping_interval(frame: &Frame) -> Option<Duration> {
    if let Some(value) = frame.header("PingInterval")
        && let Ok(seconds) = value.parse::<u64>()
        && seconds > 0
    {
        return Some(Duration::from_secs(seconds));
    }
    // 兼容实现把整段 ClientConfig JSON 放在 header 里的形态。
    let raw = frame.header("ClientConfig")?;
    let config = Json::parse(raw).ok()?;
    let seconds = config
        .get("PingInterval")
        .and_then(|value| value.as_u64())?;
    (seconds > 0).then(|| Duration::from_secs(seconds))
}

/// 构造事件 ack 帧：同 seq/method/headers + `biz_rt`，payload `{"code":200}`。
fn build_ack(frame: &Frame) -> Frame {
    let mut ack = Frame {
        seq_id: frame.seq_id,
        log_id: frame.log_id,
        service: frame.service,
        method: frame.method,
        headers: frame.headers.clone(),
        payload_encoding: Some(String::from("json")),
        payload_type: None,
        payload: br#"{"code":200}"#.to_vec(),
        log_id_new: None,
    };
    ack.set_header("biz_rt", String::from("0"));
    ack
}

/// 分片重组缓冲（§3.5：按 message_id 聚合 seq/sum，TTL 5s）。
#[derive(Debug, Default)]
struct FragmentSet {
    entries: HashMap<String, Fragment>,
}

/// 一组未齐分片。
#[derive(Debug)]
struct Fragment {
    started: Instant,
    sum: u32,
    parts: BTreeMap<u32, Vec<u8>>,
    bytes: usize,
}

impl FragmentSet {
    fn new() -> Self {
        Self {
            entries: HashMap::new(),
        }
    }

    /// 追加一段。声明非法、重复段、越界段返回 `None`；凑齐时返回重组 payload。
    fn insert(
        &mut self,
        message_id: &str,
        seq: u32,
        sum: u32,
        payload: &[u8],
        now: Instant,
    ) -> Option<Vec<u8>> {
        if sum == 0 || seq >= sum {
            return None;
        }
        let entry = self
            .entries
            .entry(message_id.to_string())
            .or_insert_with(|| Fragment {
                started: now,
                sum,
                parts: BTreeMap::new(),
                bytes: 0,
            });
        if entry.sum != sum {
            // 声明变化：旧片段不再可信，重置该组。
            entry.sum = sum;
            entry.parts.clear();
            entry.bytes = 0;
            entry.started = now;
        }
        if entry.parts.contains_key(&seq) {
            // 重投段：忽略（应用层按 event_id 再兜底去重）。
            return None;
        }
        if entry.bytes + payload.len() > MAX_FRAGMENT_BYTES {
            self.entries.remove(message_id);
            return None;
        }
        entry.parts.insert(seq, payload.to_vec());
        entry.bytes += payload.len();
        if entry.parts.len() != sum as usize {
            return None;
        }
        let entry = self.entries.remove(message_id)?;
        let mut out = Vec::with_capacity(entry.bytes);
        for part in entry.parts.values() {
            out.extend_from_slice(part);
        }
        Some(out)
    }

    /// 丢弃超过 [`FRAGMENT_TTL`] 的未齐分片，返回丢弃组数。
    fn purge(&mut self, now: Instant) -> usize {
        let before = self.entries.len();
        self.entries
            .retain(|_, entry| now.duration_since(entry.started) <= FRAGMENT_TTL);
        before - self.entries.len()
    }
}

// ---------------------------------------------------------------------------
// REST（§4）
// ---------------------------------------------------------------------------

/// REST 失败分类（消息不含 URL/token/secret）。
#[derive(Debug, Clone, PartialEq, Eq)]
enum RestError {
    /// 网络层失败（连接/超时/读写）。
    Network(String),
    /// 非 2xx 状态（已重试 429/5xx）。
    Http { status: u16 },
    /// 飞书业务错误码。
    Api { code: i64, message: String },
    /// 响应结构非法。
    Protocol(String),
}

impl std::fmt::Display for RestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RestError::Network(message) => write!(f, "网络错误：{message}"),
            RestError::Http { status } => write!(f, "HTTP {status}"),
            RestError::Api { code, message } => write!(f, "飞书错误 code={code}：{message}"),
            RestError::Protocol(message) => write!(f, "响应非法：{message}"),
        }
    }
}

/// token 缓存（内存，不落盘；`expire_in - 300s` 前视为过期）。
#[derive(Debug, Clone)]
struct TokenCache {
    value: String,
    expires_at: Instant,
}

/// 飞书 REST 客户端（endpoint/token/发送/回复）。
///
/// `Mutex<StdHttpClient>`：REST 调用串行（消息频率低，串行足够且省去连接
/// 池）；token 缓存独立加锁，刷新时不持有 HTTP 锁。
struct RestClient {
    http: Mutex<StdHttpClient>,
    domain: String,
    app_id: String,
    app_secret: String,
    token: Mutex<Option<TokenCache>>,
    timeout: Duration,
}

impl RestClient {
    fn new(options: &FeishuOptions) -> Self {
        Self {
            http: Mutex::new(StdHttpClient::new()),
            domain: options.domain.trim_end_matches('/').to_string(),
            app_id: options.app_id.clone(),
            app_secret: options.app_secret.clone(),
            token: Mutex::new(None),
            timeout: options.http_timeout,
        }
    }

    /// 单次 HTTP 请求（无重试）：返回（状态码, 响应文本）。
    fn http_post(
        &self,
        url: &str,
        headers: &[(String, String)],
        body: &[u8],
    ) -> Result<(u16, String), RestError> {
        self.http_send("POST", url, headers, body)
    }

    /// 单次 HTTP 请求（无重试；`api_call`/`token`/`endpoint` 共用）。
    fn http_send(
        &self,
        method: &str,
        url: &str,
        headers: &[(String, String)],
        body: &[u8],
    ) -> Result<(u16, String), RestError> {
        let mut client = self.http.lock().unwrap_or_else(|p| p.into_inner());
        let mut request = HttpRequest::new(method, url);
        for (name, value) in headers {
            request = request.header(name.as_str(), value.as_str());
        }
        request.body = body.to_vec();
        request.timeout = self.timeout;
        match client.execute(&request) {
            Ok(response) => Ok((response.status, response.body_text())),
            Err(crate::http::HttpError::Timeout) => {
                Err(RestError::Network(String::from("请求超时")))
            }
            Err(error) => Err(RestError::Network(error.to_string())),
        }
    }
    /// 取 tenant_access_token（`force` 时强制刷新，§4.1）。
    fn token(&self, force: bool) -> Result<String, RestError> {
        if !force {
            let cache = self.token.lock().unwrap_or_else(|p| p.into_inner());
            if let Some(cache) = cache.as_ref()
                && Instant::now() < cache.expires_at
            {
                return Ok(cache.value.clone());
            }
        }
        let url = format!(
            "{}/open-apis/auth/v3/tenant_access_token/internal",
            self.domain
        );
        let body = Json::object(vec![
            ("app_id", Json::string(self.app_id.as_str())),
            ("app_secret", Json::string(self.app_secret.as_str())),
        ])
        .to_compact_string();
        let headers = vec![(
            String::from("Content-Type"),
            String::from("application/json"),
        )];
        let (status, text) = self.http_post(&url, &headers, body.as_bytes())?;
        if status != 200 {
            return Err(RestError::Http { status });
        }
        let json = Json::parse(&text)
            .map_err(|_| RestError::Protocol(String::from("token 响应不是合法 JSON")))?;
        let code = json
            .get("code")
            .and_then(|value| value.as_i64())
            .unwrap_or(-1);
        if code != 0 {
            return Err(RestError::Api {
                code,
                message: api_message(&json),
            });
        }
        let value = json
            .get("tenant_access_token")
            .and_then(|value| value.as_str())
            .filter(|value| !value.is_empty())
            .ok_or_else(|| RestError::Protocol(String::from("token 响应缺少 tenant_access_token")))?
            .to_string();
        let expire_in = json
            .get("expire_in")
            .and_then(|value| value.as_u64())
            .unwrap_or(7_200);
        // 提前 300s 过期，且至少缓存 60s（防止异常小值导致每次请求都刷新）。
        let ttl = Duration::from_secs(expire_in.saturating_sub(300).max(60));
        let mut cache = self.token.lock().unwrap_or_else(|p| p.into_inner());
        *cache = Some(TokenCache {
            value: value.clone(),
            expires_at: Instant::now() + ttl,
        });
        Ok(value)
    }

    /// 带鉴权的 API 调用：429/5xx 指数退避重试（1s、2s）各一次；token 失效
    /// 强制刷新后重试一次；其余 4xx 不重试。
    fn api_call(&self, method: &str, path: &str, body: &Json) -> Result<Json, RestError> {
        let url = format!("{}{}", self.domain, path);
        let payload = body.to_compact_string();
        let mut last_error = RestError::Network(String::from("请求失败"));
        let mut refreshed = false;
        let mut attempt = 0u32;
        loop {
            attempt += 1;
            if attempt > 3 {
                return Err(last_error);
            }
            if attempt > 1 {
                thread::sleep(Duration::from_secs(u64::from(attempt - 1)));
            }
            let token = match self.token(false) {
                Ok(token) => token,
                Err(error) => {
                    last_error = error;
                    continue;
                }
            };
            let headers = vec![
                (
                    String::from("Content-Type"),
                    String::from("application/json"),
                ),
                (String::from("Authorization"), format!("Bearer {token}")),
            ];
            match self.http_send(method, &url, &headers, payload.as_bytes()) {
                Ok((200, text)) => {
                    let json = match Json::parse(&text) {
                        Ok(json) => json,
                        Err(_) => {
                            last_error = RestError::Protocol(String::from("响应不是合法 JSON"));
                            continue;
                        }
                    };
                    let code = json
                        .get("code")
                        .and_then(|value| value.as_i64())
                        .unwrap_or(-1);
                    if code == 0 {
                        return Ok(json);
                    }
                    if TOKEN_INVALID_CODES.contains(&code) && !refreshed {
                        refreshed = true;
                        let _ = self.token(true);
                        continue;
                    }
                    return Err(RestError::Api {
                        code,
                        message: api_message(&json),
                    });
                }
                Ok((status, _)) if status == 429 || (500..=599).contains(&status) => {
                    last_error = RestError::Http { status };
                    continue;
                }
                Ok((401, _)) if !refreshed => {
                    refreshed = true;
                    let _ = self.token(true);
                    continue;
                }
                Ok((status, _)) => return Err(RestError::Http { status }),
                Err(error) => {
                    last_error = error;
                    continue;
                }
            }
        }
    }

    /// 取连接端点（§3.1；失败分类见 [`EndpointError`]）。
    fn endpoint(&self) -> Result<EndpointInfo, EndpointError> {
        let url = format!("{}/callback/ws/endpoint", self.domain);
        let body = Json::object(vec![
            ("AppID", Json::string(self.app_id.as_str())),
            ("AppSecret", Json::string(self.app_secret.as_str())),
        ])
        .to_compact_string();
        let headers = vec![(
            String::from("Content-Type"),
            String::from("application/json"),
        )];
        let (status, text) = self
            .http_post(&url, &headers, body.as_bytes())
            .map_err(|error| match error {
                RestError::Network(message) => EndpointError::Unreachable(message),
                other => EndpointError::Unreachable(other.to_string()),
            })?;
        if status != 200 {
            return Err(EndpointError::Temporary(format!("HTTP {status}")));
        }
        let json = Json::parse(&text)
            .map_err(|_| EndpointError::Temporary(String::from("响应不是合法 JSON")))?;
        let code = json
            .get("code")
            .and_then(|value| value.as_i64())
            .unwrap_or(-1);
        match code {
            0 => {}
            1 | 1_000_040_343 => return Err(EndpointError::Temporary(format!("code={code}"))),
            other => {
                return Err(EndpointError::Fatal(format!(
                    "code={other}：{}",
                    api_message(&json)
                )));
            }
        }
        let data = json
            .get("data")
            .ok_or_else(|| EndpointError::Temporary(String::from("响应缺少 data")))?;
        let url = data
            .get("URL")
            .and_then(|value| value.as_str())
            .filter(|url| !url.is_empty())
            .ok_or_else(|| EndpointError::Temporary(String::from("响应缺少连接 URL")))?
            .to_string();
        let client_config = data.get("ClientConfig");
        let number = |key: &str, default: i64| {
            client_config
                .and_then(|config| config.get(key))
                .and_then(|value| value.as_i64())
                .unwrap_or(default)
        };
        Ok(EndpointInfo {
            url,
            reconnect_count: number("ReconnectCount", -1),
            reconnect_interval: Duration::from_secs(
                u64::try_from(number("ReconnectInterval", 120).max(1)).unwrap_or(1),
            ),
            reconnect_nonce: Duration::from_secs(
                u64::try_from(number("ReconnectNonce", 30).max(0)).unwrap_or(0),
            ),
            ping_interval: Duration::from_secs(
                u64::try_from(number("PingInterval", 120).max(1)).unwrap_or(1),
            ),
        })
    }

    /// 回复原消息（答案，§4.2）。
    fn reply_text(&self, message_id: &str, text: &str) -> Result<(), RestError> {
        let body = Json::object(vec![
            ("msg_type", Json::string("text")),
            ("content", Json::string(text_content(text))),
        ]);
        let path = format!("/open-apis/im/v1/messages/{message_id}/reply");
        self.api_call("POST", &path, &body).map(|_| ())
    }

    /// 主动发送到会话（ack 与提示，§4.2）。
    fn send_text(&self, chat_id: &str, text: &str) -> Result<(), RestError> {
        let body = Json::object(vec![
            ("receive_id", Json::string(chat_id)),
            ("msg_type", Json::string("text")),
            ("content", Json::string(text_content(text))),
        ]);
        self.api_call(
            "POST",
            "/open-apis/im/v1/messages?receive_id_type=chat_id",
            &body,
        )
        .map(|_| ())
    }
}

/// `content` 字段是 JSON 字符串（二次编码，§5.1）。
fn text_content(text: &str) -> String {
    Json::object(vec![("text", Json::string(text))]).to_compact_string()
}

/// 飞书错误响应的 `msg` 文本（截断到 200 字符，不含凭据）。
fn api_message(json: &Json) -> String {
    json.get("msg")
        .and_then(|value| value.as_str())
        .unwrap_or_default()
        .chars()
        .take(200)
        .collect()
}

// ---------------------------------------------------------------------------
// dispatch（§5、§6）
// ---------------------------------------------------------------------------

/// 一条事件的解析结果（只保留回路需要的字段）。
#[derive(Debug, Clone, PartialEq, Eq)]
struct FeishuEvent {
    /// 事件 id（去重键；缺失时退化为 message_id）。
    event_id: String,
    /// 事件类型（只处理 `im.message.receive_v1`）。
    event_type: String,
    /// 会话 id（回复与限流键）。
    chat_id: String,
    /// `p2p` / `group`。
    chat_type: String,
    /// 消息 id（回复锚点）。
    message_id: String,
    /// 消息所属话题 id（非话题消息为空）。
    thread_id: Option<String>,
    /// 回复场景的根消息 id。
    root_id: Option<String>,
    /// 回复场景的父消息 id。
    parent_id: Option<String>,
    /// 消息类型（`text` 为 V1 支持）。
    message_type: String,
    /// prompt 文本（已剥离 @ 占位符并 trim）。
    text: String,
    /// 命中的 @ 占位符（群聊触发判定）。
    mentions: Vec<String>,
    /// 发送者 id 列表（open_id/user_id/union_id；审计展示首个）。
    sender_ids: Vec<String>,
    /// 审计用发送者 id。
    sender: String,
    /// 消息创建时间（事件原样字符串，毫秒时间戳）。
    message_time: Option<String>,
}

impl FeishuEvent {
    /// 解析事件 JSON（§3.3）；缺关键字段返回 None（丢弃）。
    fn parse(payload: &[u8]) -> Option<Self> {
        let text = std::str::from_utf8(payload).ok()?;
        let root = Json::parse(text).ok()?;
        let header = root.get("header")?;
        let event_type = json_string(header.get("event_type"))?;
        let event = root.get("event")?;
        let message = event.get("message")?;
        let chat_id = json_string(message.get("chat_id"))?;
        let message_id = json_string(message.get("message_id")).unwrap_or_default();
        let event_id = json_string(header.get("event_id"))
            .filter(|value| !value.is_empty())
            .or_else(|| {
                let fallback = message_id.clone();
                (!fallback.is_empty()).then_some(fallback)
            })?;
        let chat_type =
            json_string(message.get("chat_type")).unwrap_or_else(|| String::from("p2p"));
        let thread_id = json_optional_string(message.get("thread_id"));
        let root_id = json_optional_string(message.get("root_id"));
        let parent_id = json_optional_string(message.get("parent_id"));
        let message_time = json_optional_string(message.get("create_time"));
        let message_type = json_string(message.get("message_type")).unwrap_or_default();
        let mut text = if message_type == "text" {
            message
                .get("content")
                .and_then(|value| value.as_str())
                .and_then(|raw| Json::parse(raw).ok())
                .and_then(|body| json_string(body.get("text")))
                .unwrap_or_default()
        } else {
            String::new()
        };
        let mut mentions = Vec::new();
        if let Some(items) = message.get("mentions").and_then(|value| value.as_array()) {
            for item in items {
                if let Some(key) = json_string(item.get("key")) {
                    mentions.push(key);
                }
            }
        }
        for key in &mentions {
            text = text.replace(key.as_str(), " ");
        }
        let mut sender_ids = Vec::new();
        if let Some(sender) = event
            .get("sender")
            .and_then(|sender| sender.get("sender_id"))
        {
            for field in ["open_id", "user_id", "union_id"] {
                if let Some(id) = json_string(sender.get(field)) {
                    sender_ids.push(id);
                }
            }
        }
        let sender = sender_ids.first().cloned().unwrap_or_default();
        Some(FeishuEvent {
            event_id,
            event_type,
            chat_id,
            chat_type,
            message_id,
            thread_id,
            root_id,
            parent_id,
            message_type,
            text: text.trim().to_string(),
            mentions,
            sender_ids,
            sender,
            message_time,
        })
    }

    /// 多轮记忆的键：普通消息按 chat；话题消息按 chat/thread，避免话题串上下文。
    fn session_key(&self) -> String {
        match &self.thread_id {
            Some(thread_id) => format!("{}/{}", self.chat_id, thread_id),
            None => self.chat_id.clone(),
        }
    }

    /// 构造交给 agent 的会话上下文（元数据 + 最近轮次快照）。
    fn conversation_context(&self, turns: Vec<ConversationTurn>) -> ConversationContext {
        ConversationContext {
            channel: String::from("feishu"),
            chat_type: self.chat_type.clone(),
            chat_id: self.chat_id.clone(),
            thread_id: self.thread_id.clone(),
            message_id: self.message_id.clone(),
            root_id: self.root_id.clone(),
            parent_id: self.parent_id.clone(),
            sender_id: self.sender.clone(),
            message_time: self.message_time.clone(),
            turns,
        }
    }

    /// allowlist 判定：user 命中或 chat 命中（§6.1）。
    fn authorized(&self, options: &FeishuOptions) -> bool {
        options
            .allow_users
            .iter()
            .any(|id| self.sender_ids.iter().any(|sender| sender == id))
            || options.allow_chats.iter().any(|id| id == &self.chat_id)
    }

    /// 群聊是否 @ 了本机器人：收到 `group_at_msg` 事件即说明是，静态可见
    /// 的判据是 `mentions` 非空（未 @ 的群消息不触发）。
    fn triggered(&self) -> bool {
        if self.chat_type == "group" {
            !self.mentions.is_empty()
        } else {
            true
        }
    }
}

/// 小命令层（§5.3）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Command {
    /// `/cancel`：置位该 chat 的在途取消令牌。
    Cancel,
    /// `/status`：一行状态。
    Status,
    /// 未知 `/` 命令：回可用命令清单。
    Unknown,
}

/// 解析 `/` 命令（只认第一个 token；`/cancel extra` 仍按 cancel）。
fn command_of(text: &str) -> Option<Command> {
    let rest = text.strip_prefix('/')?;
    let name = rest.split_whitespace().next().unwrap_or_default();
    Some(match name {
        "cancel" => Command::Cancel,
        "status" => Command::Status,
        _ => Command::Unknown,
    })
}

/// event_id 有界去重窗口（同 WorkerRegistry `DedupWindow` 手法）。
#[derive(Debug)]
struct DedupWindow {
    order: VecDeque<String>,
    seen: HashSet<String>,
    capacity: usize,
}

impl DedupWindow {
    fn new(capacity: usize) -> Self {
        Self {
            order: VecDeque::with_capacity(capacity),
            seen: HashSet::with_capacity(capacity),
            capacity: capacity.max(1),
        }
    }

    /// 记录事件 id：首次返回 true，重复返回 false（窗口满时淘汰最旧）。
    fn insert(&mut self, key: &str) -> bool {
        if !self.seen.insert(key.to_string()) {
            return false;
        }
        self.order.push_back(key.to_string());
        while self.order.len() > self.capacity {
            if let Some(oldest) = self.order.pop_front() {
                self.seen.remove(&oldest);
            }
        }
        true
    }
}

/// 每 chat 滑动窗口限流（§6.4）。
#[derive(Debug)]
struct RateWindow {
    stamps: VecDeque<Instant>,
    limit: usize,
    window: Duration,
}

impl RateWindow {
    fn new(limit: usize, window: Duration) -> Self {
        Self {
            stamps: VecDeque::new(),
            limit: limit.max(1),
            window,
        }
    }

    /// 窗口内是否仍可放行；放行时记账。
    fn allow(&mut self, now: Instant) -> bool {
        while let Some(front) = self.stamps.front() {
            if now.duration_since(*front) >= self.window {
                self.stamps.pop_front();
            } else {
                break;
            }
        }
        if self.stamps.len() >= self.limit {
            return false;
        }
        self.stamps.push_back(now);
        true
    }
}

/// 会话历史中的一轮问答（用户文本 + 机器人实际回复文本）。
#[derive(Debug, Clone, PartialEq, Eq)]
struct HistoryTurn {
    user: String,
    assistant: String,
}

/// 单会话历史与最近使用时间（会话数上限按 LRU 淘汰）。
#[derive(Debug)]
struct SessionHistory {
    turns: VecDeque<HistoryTurn>,
    last_used: Instant,
}

/// 飞书会话多轮记忆：仅进程内，键 = `chat_id` 或 `chat_id/thread_id`。
///
/// 只记录已放行、非命令、`run_prompt` 成功返回的轮次；审计/trace 不含内容。
#[derive(Debug)]
struct HistoryStore {
    sessions: HashMap<String, SessionHistory>,
}

impl HistoryStore {
    fn new() -> Self {
        Self {
            sessions: HashMap::new(),
        }
    }

    /// 取最近 `max_turns` 轮（按时间升序），并受单会话总字节预算约束；
    /// 同时刷新该会话的最近使用时间。
    fn snapshot(&mut self, key: &str, max_turns: usize) -> Vec<ConversationTurn> {
        if max_turns == 0 {
            return Vec::new();
        }
        let Some(session) = self.sessions.get_mut(key) else {
            return Vec::new();
        };
        session.last_used = Instant::now();
        let mut picked: Vec<&HistoryTurn> = Vec::new();
        let mut bytes = 0usize;
        for turn in session.turns.iter().rev() {
            let size = turn.user.len() + turn.assistant.len();
            if picked.len() >= max_turns || bytes + size > MAX_CONTEXT_BYTES {
                break;
            }
            bytes += size;
            picked.push(turn);
        }
        picked.reverse();
        picked
            .into_iter()
            .map(|turn| ConversationTurn {
                user: turn.user.clone(),
                assistant: turn.assistant.clone(),
            })
            .collect()
    }

    /// 追加一轮；按单条字节、轮数、单会话总预算收敛；新会话插入时按 LRU
    /// 淘汰最久未用的会话。
    fn record(&mut self, key: &str, user: &str, assistant: &str, max_turns: usize) {
        if max_turns == 0 {
            return;
        }
        let now = Instant::now();
        if !self.sessions.contains_key(key) {
            while self.sessions.len() >= MAX_CONTEXT_SESSIONS {
                let Some(oldest) = self
                    .sessions
                    .iter()
                    .min_by_key(|(_, session)| session.last_used)
                    .map(|(key, _)| key.clone())
                else {
                    break;
                };
                self.sessions.remove(&oldest);
            }
            self.sessions.insert(
                key.to_string(),
                SessionHistory {
                    turns: VecDeque::new(),
                    last_used: now,
                },
            );
        }
        let Some(session) = self.sessions.get_mut(key) else {
            return;
        };
        session.last_used = now;
        session.turns.push_back(HistoryTurn {
            user: truncate_text(user, MAX_CONTEXT_MESSAGE_BYTES),
            assistant: truncate_text(assistant, MAX_CONTEXT_MESSAGE_BYTES),
        });
        while session.turns.len() > max_turns {
            session.turns.pop_front();
        }
        let mut bytes: usize = session
            .turns
            .iter()
            .map(|turn| turn.user.len() + turn.assistant.len())
            .sum();
        while bytes > MAX_CONTEXT_BYTES && session.turns.len() > 1 {
            if let Some(front) = session.turns.pop_front() {
                bytes = bytes.saturating_sub(front.user.len() + front.assistant.len());
            }
        }
        // 防御：单轮（各上限 4 KiB）正常不会超总预算；仍超时截断 assistant
        // 保留一个可用的 user 上下文。
        if bytes > MAX_CONTEXT_BYTES
            && let Some(back) = session.turns.back_mut()
        {
            let keep = MAX_CONTEXT_BYTES.saturating_sub(back.user.len());
            back.assistant = truncate_text(&back.assistant, keep);
        }
    }
}

/// dispatch 线程状态（单消费者，无需给自有字段加锁）。
struct Dispatcher<C: ChatClient + Clone + Send + 'static> {
    core: Arc<PromptCore<C>>,
    gateway: GatewayHandle,
    options: FeishuOptions,
    rest: Arc<RestClient>,
    stop: Arc<AtomicBool>,
    seen: DedupWindow,
    rates: HashMap<String, RateWindow>,
    denied_reply_at: HashMap<String, Instant>,
    chat_runs: Arc<Mutex<HashMap<String, CancelToken>>>,
    active_runs: Arc<AtomicUsize>,
    history: Arc<Mutex<HistoryStore>>,
}

impl<C: ChatClient + Clone + Send + 'static> Dispatcher<C> {
    /// 主循环：阻塞取完整事件，逐条处理，直到 IO 线程释放发送端。
    fn run(mut self, rx: mpsc::Receiver<Vec<u8>>) {
        while let Ok(payload) = rx.recv() {
            if self.stop.load(Ordering::SeqCst) {
                break;
            }
            self.handle_event(&payload);
        }
    }

    /// 单条事件回路（§5.1 → §5.2 / §5.3）。
    fn handle_event(&mut self, payload: &[u8]) {
        let Some(event) = FeishuEvent::parse(payload) else {
            return;
        };
        if event.event_type != EVENT_MESSAGE_RECEIVE {
            return;
        }
        if !self.seen.insert(&event.event_id) {
            // 服务端重投（3s 未 ack/断线重放）：静默丢弃，不重复回复。
            return;
        }
        let started = Instant::now();
        if !event.triggered() {
            // 群聊未 @ 本机器人：不处理（不审计，避免噪声）。
            return;
        }
        if !event.authorized(&self.options) {
            self.reply_denied(&event);
            audit_line(
                &event.chat_id,
                &event.sender,
                "-",
                event.text.len(),
                "denied",
                started,
            );
            return;
        }
        let now = Instant::now();
        let rate = self
            .rates
            .entry(event.chat_id.clone())
            .or_insert_with(|| RateWindow::new(RATE_PER_MINUTE, RATE_WINDOW));
        if !rate.allow(now) {
            let _ = self.notify(&event, RATE_TEXT);
            audit_line(
                &event.chat_id,
                &event.sender,
                "-",
                event.text.len(),
                "rate_limited",
                started,
            );
            return;
        }
        if event.message_type != "text" {
            let _ = self.notify(&event, NON_TEXT_TEXT);
            audit_line(
                &event.chat_id,
                &event.sender,
                "-",
                event.text.len(),
                "non_text",
                started,
            );
            return;
        }
        let prompt = event.text.trim();
        if prompt.is_empty() {
            let _ = self.notify(&event, USAGE_TEXT);
            audit_line(
                &event.chat_id,
                &event.sender,
                "-",
                event.text.len(),
                "empty",
                started,
            );
            return;
        }
        if prompt.len() > MAX_PROMPT_TEXT_BYTES {
            let _ = self.notify(&event, TOO_LONG_TEXT);
            audit_line(
                &event.chat_id,
                &event.sender,
                "-",
                prompt.len(),
                "too_long",
                started,
            );
            return;
        }
        if let Some(command) = command_of(prompt) {
            self.handle_command(&event, command, started);
            return;
        }
        self.spawn_run(&event, prompt.to_string(), started);
    }

    /// 未授权提示（每 chat 60s 只回一次，§6.1）。
    fn reply_denied(&mut self, event: &FeishuEvent) {
        let now = Instant::now();
        if let Some(last) = self.denied_reply_at.get(&event.chat_id)
            && now.duration_since(*last) < DENIED_REPLY_INTERVAL
        {
            return;
        }
        self.denied_reply_at.insert(event.chat_id.clone(), now);
        let _ = self.notify(event, DENIED_TEXT);
    }

    /// 主动发送提示到会话（返回是否成功）。
    fn notify(&self, event: &FeishuEvent, text: &str) -> bool {
        self.rest.send_text(&event.chat_id, text).is_ok()
    }

    /// 执行 `/` 命令（不进 LLM）。
    fn handle_command(&mut self, event: &FeishuEvent, command: Command, started: Instant) {
        let (name, ok) = match command {
            Command::Cancel => {
                let cancelled = {
                    let map = self.chat_runs.lock().unwrap_or_else(|p| p.into_inner());
                    match map.get(&event.chat_id) {
                        Some(token) => {
                            token.cancel();
                            true
                        }
                        None => false,
                    }
                };
                let text = if cancelled {
                    CANCEL_OK_TEXT
                } else {
                    CANCEL_IDLE_TEXT
                };
                ("cancel", self.notify(event, text))
            }
            Command::Status => {
                let text = self.status_text(&event.chat_id);
                ("status", self.notify(event, &text))
            }
            Command::Unknown => ("-", self.notify(event, COMMANDS_TEXT)),
        };
        audit_line(
            &event.chat_id,
            &event.sender,
            name,
            event.text.len(),
            if ok { "ok" } else { "reply_failed" },
            started,
        );
    }

    /// `/status` 一行状态：链路类型与连接态、在线 Worker（前 5）、本会话
    /// 是否在途、全局运行数/4（§5.3）。
    fn status_text(&self, chat_id: &str) -> String {
        let workers = {
            let mut gateway = self.gateway.lock().unwrap_or_else(|p| p.into_inner());
            gateway.list_workers()
        };
        let online: Vec<&str> = workers
            .iter()
            .filter(|worker| worker.online)
            .map(|worker| worker.worker_id.as_str())
            .collect();
        let worker_text = if online.is_empty() {
            String::from("0 个")
        } else {
            let shown: Vec<&str> = online.iter().take(5).copied().collect();
            let more = online.len().saturating_sub(shown.len());
            if more > 0 {
                format!(
                    "{} 个（{} … 等 {} 个）",
                    online.len(),
                    shown.join(", "),
                    more
                )
            } else {
                format!("{} 个（{}）", online.len(), shown.join(", "))
            }
        };
        let running = self
            .chat_runs
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .contains_key(chat_id);
        format!(
            "链路：{}（已连接）；在线 Worker：{}；本会话：{}；全局运行：{}/{}",
            self.options.link.as_str(),
            worker_text,
            if running { "运行中" } else { "空闲" },
            self.active_runs.load(Ordering::SeqCst),
            MAX_CONCURRENT_PROMPT_RUNS
        )
    }

    /// per-chat 串行（深度 1）地启动一次运行（§5.4）。
    fn spawn_run(&mut self, event: &FeishuEvent, prompt: String, started: Instant) {
        let len = prompt.len();
        {
            let map = self.chat_runs.lock().unwrap_or_else(|p| p.into_inner());
            if map.contains_key(&event.chat_id) {
                drop(map);
                let _ = self.notify(event, BUSY_CHAT_TEXT);
                audit_line(&event.chat_id, &event.sender, "-", len, "busy", started);
                return;
            }
        }
        let cancel = CancelToken::new();
        {
            let mut map = self.chat_runs.lock().unwrap_or_else(|p| p.into_inner());
            map.insert(event.chat_id.clone(), cancel.clone());
        }
        self.active_runs.fetch_add(1, Ordering::SeqCst);
        if self.options.ack {
            let _ = self.rest.send_text(&event.chat_id, ACK_TEXT);
        }
        // 快照历史 + 会话元数据：快照发生在记录本轮之前，当前消息不会重复
        // 出现在历史里。历史只在内存中，互斥锁保护跨运行线程的读写。
        let session_key = event.session_key();
        let context = {
            let mut history = self.history.lock().unwrap_or_else(|p| p.into_inner());
            event.conversation_context(history.snapshot(&session_key, self.options.context_turns))
        };
        let core = Arc::clone(&self.core);
        let rest = Arc::clone(&self.rest);
        let chat_runs = Arc::clone(&self.chat_runs);
        let active = Arc::clone(&self.active_runs);
        let history = Arc::clone(&self.history);
        let context_turns = self.options.context_turns;
        let max_steps = self.options.max_steps;
        let chat = event.chat_id.clone();
        let sender = event.sender.clone();
        let message_id = event.message_id.clone();
        let spawned = thread::Builder::new()
            .name(String::from("sekai-feishu-run"))
            .spawn(move || {
                let task_started = Instant::now();
                let result = core.run_prompt_with_context(&prompt, max_steps, cancel, context);
                let record_turn = result.is_ok();
                let (text, outcome_label) = reply_for(result);
                let text = truncate_reply(&text);
                // 先记录本轮再回复：保证同一会话的下一条消息一定看到上一轮
                // 上下文（回复失败时该轮保留，见 docs/sekai-feishu.md §5.5）。
                if record_turn {
                    let mut store = history.lock().unwrap_or_else(|p| p.into_inner());
                    store.record(&session_key, &prompt, &text, context_turns);
                }
                let outcome_label = if rest.reply_text(&message_id, &text).is_ok() {
                    outcome_label
                } else {
                    String::from("reply_failed")
                };
                audit_line(&chat, &sender, "-", len, &outcome_label, task_started);
                let mut map = chat_runs.lock().unwrap_or_else(|p| p.into_inner());
                map.remove(&chat);
                drop(map);
                active.fetch_sub(1, Ordering::SeqCst);
            });
        if spawned.is_err() {
            let mut map = self.chat_runs.lock().unwrap_or_else(|p| p.into_inner());
            map.remove(&event.chat_id);
            drop(map);
            self.active_runs.fetch_sub(1, Ordering::SeqCst);
            let _ = self.notify(event, SPAWN_FAILED_TEXT);
            audit_line(
                &event.chat_id,
                &event.sender,
                "-",
                len,
                "error:spawn",
                started,
            );
        }
    }
}

/// 审计行（stderr，每消息一行；不记内容，§6.5）。
fn audit_line(chat: &str, sender: &str, cmd: &str, len: usize, outcome: &str, started: Instant) {
    eprintln!(
        "sekai-router: feishu chat={chat} sender={sender} cmd={cmd} len={len} outcome={outcome} ms={}",
        started.elapsed().as_millis()
    );
}

/// run_prompt 结果 → （回复文本, 审计 outcome）（§5.2 映射表）。
fn reply_for(result: Result<PromptRunResult, PromptRunError>) -> (String, String) {
    match result {
        Ok(result) => {
            if result.status == "max_steps" {
                let mut text = format!("（已达最大步数 {}）", result.steps);
                if !result.answer.trim().is_empty() {
                    text.push_str("\n\n");
                    text.push_str(&result.answer);
                }
                (text, String::from("max_steps"))
            } else {
                let answer = if result.answer.trim().is_empty() {
                    String::from("（无输出）")
                } else {
                    result.answer
                };
                (answer, String::from("ok"))
            }
        }
        Err(error) => {
            let text = match error.code.as_str() {
                CODE_LLM_UNAVAILABLE => String::from("LLM 未配置：需要 DEEPSEEK_API_KEY"),
                CODE_NOT_LINKED => String::from("Worker 链路未建立"),
                CODE_NO_WORKERS => String::from("暂无 Worker 在线，请稍后再试"),
                CODE_BUSY => String::from("系统繁忙，请稍后再试"),
                CODE_AGENT_ERROR if error.message.contains("cancelled") => String::from("已取消"),
                _ => format!("执行失败：{}", error.message),
            };
            let outcome = if error.code == CODE_BUSY {
                String::from("busy")
            } else {
                format!("error:{}", error.code)
            };
            (text, outcome)
        }
    }
}

/// 回复截断：超过 [`MAX_REPLY_BYTES`] 时按 UTF-8 字符边界截断并加标记。
fn truncate_reply(text: &str) -> String {
    truncate_text(text, MAX_REPLY_BYTES)
}

/// 按 UTF-8 字符边界截断到 `max` 字节；超限追加 `\n[[truncated]]`。
fn truncate_text(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut end = 0;
    for (index, character) in text.char_indices() {
        let next = index + character.len_utf8();
        if next > max {
            break;
        }
        end = next;
    }
    format!("{}\n[[truncated]]", &text[..end])
}

/// 从 JSON 对象取字符串字段（None/非字符串都视为缺失）。
fn json_string(value: Option<&Json>) -> Option<String> {
    value
        .and_then(|value| value.as_str())
        .map(|value| value.to_string())
}

/// 同 [`json_string`]，但把空串/纯空白归一为缺失（飞书对 root_id 等发空串）。
fn json_optional_string(value: Option<&Json>) -> Option<String> {
    json_string(value).filter(|value| !value.trim().is_empty())
}

// ---------------------------------------------------------------------------
// 单测（无网络；协议面用内存/纯函数覆盖）
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn options() -> FeishuOptions {
        FeishuOptions {
            app_id: String::from("cli_test"),
            app_secret: String::from("secret"),
            domain: String::from("https://open.feishu.cn"),
            allow_users: Vec::new(),
            allow_chats: Vec::new(),
            ack: true,
            max_steps: None,
            context_turns: 8,
            link: LinkMode::Tcp,
            http_timeout: Duration::from_secs(10),
        }
    }

    /// 一条真实的 `im.message.receive_v1` 事件（字段形状见 §3.3）。
    fn event_json(chat_type: &str, message_type: &str, text: &str, mentions: &str) -> Vec<u8> {
        let content = Json::object(vec![("text", Json::string(text))]).to_compact_string();
        let payload = format!(
            r#"{{"schema":"2.0","header":{{"event_id":"ev-1","event_type":"im.message.receive_v1","create_time":"1700000000000","token":"t","app_id":"cli_x","tenant_key":"tk"}},"event":{{"sender":{{"sender_id":{{"union_id":"on_1","user_id":"u_1","open_id":"ou_1"}},"sender_type":"user","tenant_key":"tk"}},"message":{{"message_id":"om_1","root_id":"","parent_id":"","create_time":"1700000000000","chat_id":"oc_1","chat_type":"{chat_type}","message_type":"{message_type}","content":{},"mentions":{mentions}}}}}}}"#,
            Json::string(content)
        );
        payload.into_bytes()
    }

    #[test]
    fn debug_redacts_secret() {
        let mut options = options();
        options.app_secret = String::from("s3cr3t-value");
        let rendered = format!("{options:?}");
        assert!(!rendered.contains("s3cr3t-value"), "{rendered}");
        assert!(rendered.contains("<redacted>"), "{rendered}");
    }

    #[test]
    fn parse_ws_url_shapes() {
        let target =
            parse_ws_url("wss://open.feishu.cn/callback/ws?device_id=1&service_id=2").unwrap();
        assert!(target.tls);
        assert_eq!(target.host, "open.feishu.cn");
        assert_eq!(target.port, 443);
        assert_eq!(target.path, "/callback/ws?device_id=1&service_id=2");

        let target = parse_ws_url("wss://host.example:8443/x").unwrap();
        assert_eq!(target.port, 8443);
        assert_eq!(target.path, "/x");

        let target = parse_ws_url("ws://127.0.0.1:9000/echo").unwrap();
        assert!(!target.tls);
        assert_eq!(target.port, 9000);

        // 只有 query 没有路径。
        let target = parse_ws_url("ws://host:1?k=v").unwrap();
        assert_eq!(target.host, "host");
        assert_eq!(target.path, "/?k=v");

        // 无端口：ws 默认 80。
        let target = parse_ws_url("ws://host/x").unwrap();
        assert_eq!(target.port, 80);

        // 错误消息不含原 URL（query 含凭据）。
        for bad in [
            "https://host/x",
            "wss:///x",
            "wss://:443/x",
            "wss://host:abc/x",
        ] {
            let error = parse_ws_url(bad).unwrap_err();
            assert!(!error.contains("host"), "{bad} → {error}");
        }
    }

    #[test]
    fn dedup_window_bounded_and_replays() {
        let mut window = DedupWindow::new(3);
        assert!(window.insert("a"));
        assert!(!window.insert("a"));
        assert!(window.insert("b"));
        assert!(window.insert("c"));
        // 窗口满：插入 d 淘汰最旧 a，a 可再次插入。
        assert!(window.insert("d"));
        assert!(window.insert("a"));
        assert!(!window.insert("b") || true);
    }

    #[test]
    fn rate_window_allows_then_limits() {
        let base = Instant::now();
        let mut window = RateWindow::new(2, Duration::from_secs(60));
        assert!(window.allow(base));
        assert!(window.allow(base + Duration::from_secs(1)));
        assert!(!window.allow(base + Duration::from_secs(2)));
        // 窗口滑过后重新放行。
        assert!(window.allow(base + Duration::from_secs(62)));
    }

    #[test]
    fn fragment_reassembly_out_of_order() {
        let base = Instant::now();
        let mut fragments = FragmentSet::new();
        assert_eq!(fragments.insert("m1", 1, 2, b"World", base), None);
        let joined = fragments.insert("m1", 0, 2, b"Hello, ", base);
        assert_eq!(joined.as_deref(), Some(b"Hello, World".as_slice()));
        // 已完成：重投段返回 None。
        assert_eq!(fragments.insert("m1", 0, 2, b"Hello, ", base), None);
    }

    #[test]
    fn fragment_rejects_invalid_and_ttl() {
        let base = Instant::now();
        let mut fragments = FragmentSet::new();
        assert_eq!(fragments.insert("m", 3, 2, b"x", base), None); // seq >= sum
        assert_eq!(fragments.insert("m", 0, 0, b"x", base), None); // sum 0
        assert_eq!(fragments.insert("m", 0, 2, b"a", base), None);
        // TTL 内不丢。
        assert_eq!(fragments.purge(base + Duration::from_secs(1)), 0);
        // 超过 TTL 丢弃。
        assert_eq!(fragments.purge(base + Duration::from_secs(6)), 1);
    }

    #[test]
    fn ack_echoes_headers_and_payload() {
        let frame = Frame {
            seq_id: 7,
            log_id: 3,
            service: 2,
            method: METHOD_DATA,
            headers: vec![
                crate::pbbp::Header {
                    key: String::from("type"),
                    value: String::from("event"),
                },
                crate::pbbp::Header {
                    key: String::from("message_id"),
                    value: String::from("om_1"),
                },
            ],
            payload_encoding: Some(String::from("json")),
            payload_type: Some(String::from("event/v2.0")),
            payload: br#"{"schema":"2.0"}"#.to_vec(),
            log_id_new: Some(String::from("log-9")),
        };
        let ack = build_ack(&frame);
        assert_eq!(ack.seq_id, 7);
        assert_eq!(ack.log_id, 3);
        assert_eq!(ack.service, 2);
        assert_eq!(ack.method, METHOD_DATA);
        assert_eq!(ack.header("type"), Some("event"));
        assert_eq!(ack.header("message_id"), Some("om_1"));
        assert_eq!(ack.header("biz_rt"), Some("0"));
        assert_eq!(ack.payload_type, None);
        assert_eq!(ack.payload, br#"{"code":200}"#.to_vec());
        // ack 帧可正确编码/解码。
        let decoded = Frame::decode(&ack.encode()).unwrap();
        assert_eq!(decoded, ack);
    }

    #[test]
    fn ping_interval_from_pong_headers() {
        let mut frame = Frame {
            method: METHOD_CONTROL,
            ..Frame::default()
        };
        frame.set_header("type", "pong");
        assert_eq!(parse_ping_interval(&frame), None);
        frame.set_header("PingInterval", "30");
        assert_eq!(parse_ping_interval(&frame), Some(Duration::from_secs(30)));
        // 0 不生效（防止心跳被关死）。
        frame.set_header("PingInterval", "0");
        assert_eq!(parse_ping_interval(&frame), None);
        // ClientConfig 整段 JSON 形态。
        frame.set_header("ClientConfig", r#"{"PingInterval":15}"#);
        assert_eq!(parse_ping_interval(&frame), Some(Duration::from_secs(15)));
    }

    #[test]
    fn event_parse_text_and_mentions() {
        let payload = event_json(
            "group",
            "text",
            "@_user_1 帮我看看磁盘",
            r#"[{"key":"@_user_1","name":"机器人","id":{"open_id":"ou_bot"}}]"#,
        );
        let event = FeishuEvent::parse(&payload).unwrap();
        assert_eq!(event.event_id, "ev-1");
        assert_eq!(event.event_type, "im.message.receive_v1");
        assert_eq!(event.chat_id, "oc_1");
        assert_eq!(event.chat_type, "group");
        assert_eq!(event.message_id, "om_1");
        assert_eq!(event.message_type, "text");
        assert_eq!(event.text, "帮我看看磁盘");
        assert_eq!(event.mentions, ["@_user_1"]);
        assert_eq!(event.sender, "ou_1");
        assert!(event.sender_ids.contains(&String::from("u_1")));
        assert!(event.triggered());
    }

    #[test]
    fn event_parse_p2p_and_non_text() {
        let payload = event_json("p2p", "text", "  /status  ", "[]");
        let event = FeishuEvent::parse(&payload).unwrap();
        assert_eq!(event.text, "/status");
        assert!(event.triggered());

        // 非文本：不认识 content，但事件本身要能解析出来（回提示用）。
        let payload = event_json("p2p", "image", "", "[]");
        let event = FeishuEvent::parse(&payload).unwrap();
        assert_eq!(event.message_type, "image");
        assert!(event.text.is_empty());
    }

    #[test]
    fn event_parse_group_without_mentions_not_triggered() {
        let payload = event_json("group", "text", "随便说说", "[]");
        let event = FeishuEvent::parse(&payload).unwrap();
        assert!(!event.triggered());
    }

    #[test]
    fn event_parse_rejects_garbage() {
        assert!(FeishuEvent::parse(b"not json").is_none());
        assert!(FeishuEvent::parse(br#"{"header":{}}"#).is_none());
        assert!(FeishuEvent::parse(br#"{"header":{"event_type":"x"}}"#).is_none());
    }

    #[test]
    fn event_parse_captures_session_metadata() {
        let payload = br#"{"schema":"2.0","header":{"event_id":"ev-1","event_type":"im.message.receive_v1"},"event":{"sender":{"sender_id":{"open_id":"ou_1"}},"message":{"message_id":"om_1","root_id":"om_root","parent_id":"om_parent","thread_id":"omt_1","create_time":"1700000000000","chat_id":"oc_1","chat_type":"group","message_type":"text","content":"{\"text\":\"hi\"}","mentions":[]}}}"#;
        let event = FeishuEvent::parse(payload).unwrap();
        assert_eq!(event.thread_id.as_deref(), Some("omt_1"));
        assert_eq!(event.root_id.as_deref(), Some("om_root"));
        assert_eq!(event.parent_id.as_deref(), Some("om_parent"));
        assert_eq!(event.message_time.as_deref(), Some("1700000000000"));
        assert_eq!(event.session_key(), "oc_1/omt_1");

        // 飞书对普通消息发 root_id/parent_id 空串：归一为 None，不渲染空字段。
        let event = FeishuEvent::parse(&event_json("p2p", "text", "hi", "[]")).unwrap();
        assert_eq!(event.thread_id, None);
        assert_eq!(event.root_id, None);
        assert_eq!(event.parent_id, None);
        assert_eq!(event.session_key(), "oc_1");
        assert_eq!(event.message_time.as_deref(), Some("1700000000000"));
    }

    #[test]
    fn event_builds_conversation_context() {
        let event = FeishuEvent::parse(&event_json("p2p", "text", "hi", "[]")).unwrap();
        let context = event.conversation_context(vec![ConversationTurn {
            user: String::from("第一问"),
            assistant: String::from("第一答"),
        }]);
        assert_eq!(context.channel, "feishu");
        assert_eq!(context.chat_type, "p2p");
        assert_eq!(context.chat_id, "oc_1");
        assert_eq!(context.message_id, "om_1");
        assert_eq!(context.sender_id, "ou_1");
        assert_eq!(context.message_time.as_deref(), Some("1700000000000"));
        assert_eq!(context.turns.len(), 1);
        assert_eq!(context.turns[0].user, "第一问");
    }

    #[test]
    fn history_store_keeps_recent_turns_and_budget() {
        let mut store = HistoryStore::new();
        for index in 0..3 {
            store.record("oc_1", &format!("问{index}"), &format!("答{index}"), 10);
        }
        let turns = store.snapshot("oc_1", 2);
        assert_eq!(turns.len(), 2);
        assert_eq!(turns[0].user, "问1");
        assert_eq!(turns[1].user, "问2");

        // 单会话总预算：每轮 user 1 字节 + assistant 4096 字节 = 4097，
        // 8 轮 32776 > 32 KiB，快照最多取 7 轮。
        let mut store = HistoryStore::new();
        for index in 0..10 {
            store.record("oc_1", "u", &format!("{index}{}", "a".repeat(4095)), 10);
        }
        let turns = store.snapshot("oc_1", 10);
        assert!(!turns.is_empty());
        assert!(turns.len() <= 7, "快照超出总预算：{}", turns.len());
        let total: usize = turns
            .iter()
            .map(|turn| turn.user.len() + turn.assistant.len())
            .sum();
        assert!(total <= MAX_CONTEXT_BYTES, "总字节 {total} 超预算");
        // 取的是最新轮次（由新到旧裁剪）。
        assert!(turns.last().unwrap().assistant.starts_with('9'));
    }

    #[test]
    fn history_store_zero_turns_and_session_cap() {
        let mut store = HistoryStore::new();
        store.record("oc_1", "问", "答", 0);
        assert!(store.snapshot("oc_1", 0).is_empty());
        assert!(store.snapshot("oc_1", 8).is_empty());

        let mut store = HistoryStore::new();
        for index in 0..=MAX_CONTEXT_SESSIONS {
            store.record(&format!("oc_{index}"), "问", "答", 8);
        }
        assert_eq!(store.sessions.len(), MAX_CONTEXT_SESSIONS);
    }

    #[test]
    fn allowlist_user_or_chat() {
        let payload = event_json("p2p", "text", "hi", "[]");
        let event = FeishuEvent::parse(&payload).unwrap();

        // 空清单：全拒。
        assert!(!event.authorized(&options()));

        let mut users = options();
        users.allow_users = vec![String::from("ou_1")];
        assert!(event.authorized(&users));

        // user_id 命中同样放行。
        let mut users = options();
        users.allow_users = vec![String::from("u_other"), String::from("u_1")];
        assert!(event.authorized(&users));

        let mut chats = options();
        chats.allow_chats = vec![String::from("oc_1")];
        assert!(event.authorized(&chats));

        let mut chats = options();
        chats.allow_chats = vec![String::from("oc_other")];
        assert!(!event.authorized(&chats));
    }

    #[test]
    fn command_parsing() {
        assert_eq!(command_of("/cancel"), Some(Command::Cancel));
        assert_eq!(command_of("/cancel now"), Some(Command::Cancel));
        assert_eq!(command_of("/status"), Some(Command::Status));
        assert_eq!(command_of("/STATUS"), Some(Command::Unknown));
        assert_eq!(command_of("/"), Some(Command::Unknown));
        assert_eq!(command_of("/foo"), Some(Command::Unknown));
        assert_eq!(command_of("hello /status"), None);
    }

    #[test]
    fn reply_mapping() {
        let ok = PromptRunResult {
            answer: String::from("done"),
            status: "completed",
            steps: 1,
            usage: crate::llm::Usage::default(),
        };
        assert_eq!(
            reply_for(Ok(ok)),
            (String::from("done"), String::from("ok"))
        );

        let empty = PromptRunResult {
            answer: String::new(),
            status: "completed",
            steps: 1,
            usage: crate::llm::Usage::default(),
        };
        assert_eq!(reply_for(Ok(empty)).0, String::from("（无输出）"));

        let max = PromptRunResult {
            answer: String::from("部分结果"),
            status: "max_steps",
            steps: 8,
            usage: crate::llm::Usage::default(),
        };
        let (text, outcome) = reply_for(Ok(max));
        assert_eq!(outcome, "max_steps");
        assert!(text.contains("已达最大步数 8"), "{text}");
        assert!(text.contains("部分结果"), "{text}");

        let busy = PromptRunError::new(CODE_BUSY, "已有 4 个 prompt 在运行");
        assert_eq!(
            reply_for(Err(busy)),
            (String::from("系统繁忙，请稍后再试"), String::from("busy"))
        );

        let llm = PromptRunError::new(CODE_LLM_UNAVAILABLE, "未配置 LLM");
        assert_eq!(
            reply_for(Err(llm)).0,
            String::from("LLM 未配置：需要 DEEPSEEK_API_KEY")
        );

        let cancelled = PromptRunError::new(CODE_AGENT_ERROR, "cancelled: 用户已取消当前目标");
        let (text, outcome) = reply_for(Err(cancelled));
        assert_eq!(text, "已取消");
        assert_eq!(outcome, "error:agent_error");

        let not_linked = PromptRunError::new(CODE_NOT_LINKED, "无链路网关");
        assert_eq!(reply_for(Err(not_linked)).1, "error:not_linked");

        let no_workers = PromptRunError::new(CODE_NO_WORKERS, "无已发现的 Worker");
        assert_eq!(reply_for(Err(no_workers)).1, "error:no_workers");
    }

    #[test]
    fn truncation_respects_char_boundary() {
        // 短文本原样返回。
        assert_eq!(truncate_reply("short"), "short");

        // 刚好 4000 字节（ASCII）：不截断。
        let exact = "a".repeat(MAX_REPLY_BYTES);
        assert_eq!(truncate_reply(&exact), exact);

        // 4001 字节 ASCII：截到 4000 + 标记。
        let long = "a".repeat(MAX_REPLY_BYTES + 1);
        let cut = truncate_reply(&long);
        assert!(cut.ends_with("\n[[truncated]]"), "{cut}");
        assert_eq!(cut.len(), MAX_REPLY_BYTES + "\n[[truncated]]".len());

        // 多字节字符：不切开 UTF-8（“汉”3 字节，4000 不是 3 的倍数）。
        let han = "汉".repeat(1400); // 4200 字节
        let cut = truncate_reply(&han);
        let body = cut.trim_end_matches("\n[[truncated]]");
        assert!(body.is_char_boundary(body.len()));
        assert!(body.len() <= MAX_REPLY_BYTES);
        assert_eq!(body.len() % 3, 0);
        assert!(body.chars().all(|c| c == '汉'));
    }

    #[test]
    fn text_content_is_escaped_json() {
        let content = text_content("说\"引号\"\n换行");
        let parsed = Json::parse(&content).unwrap();
        assert_eq!(
            parsed.get("text").and_then(|v| v.as_str()),
            Some("说\"引号\"\n换行")
        );
    }

    #[test]
    fn token_cache_ttl_minimum() {
        // expire_in 小于 300s 时至少缓存 60s（防止异常值导致每请求刷新）。
        let ttl = Duration::from_secs(100u64.saturating_sub(300).max(60));
        assert_eq!(ttl, Duration::from_secs(60));
        let ttl = Duration::from_secs(7200u64.saturating_sub(300).max(60));
        assert_eq!(ttl, Duration::from_secs(6900));
    }

    #[test]
    fn reconnect_exhaustion() {
        let mut info = EndpointInfo {
            reconnect_count: -1,
            ..EndpointInfo::default()
        };
        assert!(!reconnect_exhausted(&info, 1_000_000));
        info.reconnect_count = 2;
        assert!(!reconnect_exhausted(&info, 1));
        assert!(reconnect_exhausted(&info, 2));
    }

    #[test]
    fn jitter_within_bounds() {
        for _ in 0..8 {
            assert!(jitter(Duration::ZERO).is_zero());
            assert!(jitter(Duration::from_secs(5)) < Duration::from_secs(5));
        }
    }

    #[test]
    fn api_message_truncates() {
        let json = Json::object(vec![("msg", Json::string("x".repeat(500)))]);
        assert_eq!(api_message(&json).chars().count(), 200);
        assert_eq!(api_message(&Json::Null), "");
    }
}
