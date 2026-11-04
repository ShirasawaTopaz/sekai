//! LLM prompt 通道：Worker 进程的**带外**传输（TCP JSONL 或 stdin/stdout JSONL）。
//!
//! PC Worker 的 `pc.ask` 任务需要把 prompt 送到一个能调 LLM 的对端并取回
//! 回答。V1 二进制帧协议**冻结不动**，prompt 走本模块的带外通道，两种实现：
//!
//! - [`RouterPromptChannel`]（`--llm router`）：每次 ask 新建一条 TCP 连接，
//!   发一行请求、读一行响应、关连接。对端是 Router 的 ingress
//!   （sekai-router 交互/serve 模式默认监听 `127.0.0.1:7422`，可用
//!   `--prompt-addr` 改），Router 负责调用 LLM；
//! - [`DshPromptChannel`]（`--llm dsh`）：经**本进程 stdin/stdout** 与安装
//!   本 Worker 的 DSH 插件交换同构 JSONL。仅在 connect 模式可用（Worker 的
//!   Router 链路是 TCP，stdin/stdout 不被占用；stdio 模式 stdin 是链路，
//!   故拒绝）。主进程在启动时 spawn 一个 stdin 行读线程 [`DshPromptChannel::pump`]
//!   喂通道；ask 写一行到 stdout 并等待对应 id 的响应。
//!
//! ## JSONL 契约（与 Router ingress 冻结一致）
//!
//! - 请求：`{"id":1,"method":"prompt","params":{"text":"<prompt>"}}`
//! - 成功：`{"id":1,"ok":true,"result":{"answer":"...","status":"completed",
//!   "steps":n,"usage":{...}}}` → 返回 `answer`
//! - 失败：`{"id":1,"ok":false,"error":{"code":"..","message":".."}}` →
//!   [`PromptError`] 原样透传
//!
//! `id` 每通道内单调递增；响应按 `id` 关联请求。行内非 JSON 或无匹配 id
//! 的响应一律忽略。序列化/解析用本 crate 的 [`crate::mini_json`] 子集。
//!
//! ## 健壮性（P8）
//!
//! - **按 id 过滤**：两条通道都只接受 `id` 匹配当前请求的响应行；迟到/错位
//!   响应被忽略（不会把同 socket 上别人的 `ok` 行当成本次回答）；
//! - **有界缓冲**：DSH 通道的迟到响应缓冲有上限（`MAX_PENDING_RESPONSES`），
//!   无主响应不会无限增长；
//! - **连接超时**：Router 通道用 `TcpStream::connect_timeout`（≤
//!   `CONNECT_TIMEOUT_MS`，且不超过本次 ask 预算），不会无限等待；
//! - **可取消**：[`PromptTransport::ask_cancellable`] 让 pc.ask 任务在 DDL
//!   到期/丢弃时通知工作线程退出——只丢弃接收端不足以结束阻塞中的 socket
//!   读或 condvar 等待。

use std::fmt;
use std::io::{BufRead, BufReader, ErrorKind, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use crate::mini_json::Json;

/// Router prompt 通道建立 TCP 连接的上限（毫秒）；同时不超过本次 ask 预算。
const CONNECT_TIMEOUT_MS: u64 = 5_000;

/// 可取消 ask 的单次阻塞读/等待粒度（毫秒）：每轮检查一次取消标志。
const CANCEL_CHECK_MS: u64 = 100;

/// 单次 ask 最多忽略的「迟到/不匹配 id」响应行数（防止对端持续灌入）。
const MAX_IGNORED_LINES: usize = 64;

/// DSH 通道待取响应缓冲上限：迟到/无主响应超出即按入队顺序丢弃最旧的。
const MAX_PENDING_RESPONSES: usize = 64;

/// 永不取消的取消标志（供 [`PromptTransport::ask`] 的默认实现复用）。
static NEVER_CANCELLED: AtomicBool = AtomicBool::new(false);

/// 从环境变量读取一个钳制到 `[min, max]` 的毫秒值；缺失/非法取 `default`。
pub(crate) fn env_clamped_ms(name: &str, default: u64, min: u64, max: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|text| text.trim().parse::<u64>().ok())
        .map_or(default, |value| value.clamp(min, max))
}

/// prompt 通道的单次等待上限（env `SEKAI_PC_ASK_MS`，1000..=300000，
/// 默认 300000）。pc.ask 任务与两个通道实现共用。
pub fn ask_timeout_ms() -> u64 {
    env_clamped_ms("SEKAI_PC_ASK_MS", 300_000, 1_000, 300_000)
}

/// prompt 通道错误：`code` 为机器可读的分类（如 `unreachable` / `timeout` /
/// `llm_unavailable` / `invalid_arguments` / `unknown_method` / `agent_error`），
/// `message` 为可直接展示的中文描述。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptError {
    /// 机器可读错误码。
    pub code: String,
    /// 可读错误信息。
    pub message: String,
}

impl PromptError {
    /// 构造。
    pub fn new(code: &str, message: impl Into<String>) -> Self {
        Self {
            code: String::from(code),
            message: message.into(),
        }
    }
}

impl fmt::Display for PromptError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "prompt 通道错误（{}）：{}", self.code, self.message)
    }
}

impl std::error::Error for PromptError {}

/// LLM prompt 传输抽象：发送一段 prompt，阻塞取回回答。
///
/// 实现允许阻塞到自身超时；pc.ask 任务会把它放到独立线程推进，
/// 不占住任务 poll。
pub trait PromptTransport: Send + Sync {
    /// 发送 prompt 并等待回答。
    fn ask(&self, prompt: &str) -> Result<String, PromptError>;

    /// 可取消版本：`cancelled` 置位后应尽快返回（`code = "cancelled"`）。
    ///
    /// pc.ask 任务在 DDL 到期/被丢弃时置位该标志；**只丢弃接收端不足以**
    /// 结束阻塞中的 socket 读 / condvar 等待，因此内置通道都实现本方法。
    /// 默认实现退化为不可取消的 [`ask`]（供简单测试替身使用）。
    ///
    /// [`ask`]: PromptTransport::ask
    fn ask_cancellable(&self, prompt: &str, cancelled: &AtomicBool) -> Result<String, PromptError> {
        let _ = cancelled;
        self.ask(prompt)
    }
}

/// 构造一条 prompt 请求行（JSONL 契约见模块头）。
fn prompt_request(id: u64, prompt: &str) -> String {
    Json::object(vec![
        ("id", Json::from(id)),
        ("method", Json::from("prompt")),
        ("params", Json::object(vec![("text", Json::from(prompt))])),
    ])
    .to_compact_string()
}

/// 解析一行响应。
///
/// 返回 `Some((id, 结果))`：`ok=true` 取 `result.answer`；`ok=false` 透传
/// `error.code/message`。行不是合法 JSON、缺 `ok` 或缺 `id` 时返回 `None`
/// （调用方按契约忽略该行）。
fn parse_response_line(line: &str) -> Option<(Option<u64>, Result<String, PromptError>)> {
    let value = Json::parse(line.trim()).ok()?;
    let id = value.get("id").and_then(Json::as_u64);
    match value.get("ok").and_then(Json::as_bool) {
        Some(true) => match value.get_path(&["result", "answer"]).and_then(Json::as_str) {
            Some(answer) => Some((id, Ok(String::from(answer)))),
            None => Some((
                id,
                Err(PromptError::new(
                    "agent_error",
                    "prompt 响应缺少 result.answer 字段",
                )),
            )),
        },
        Some(false) => {
            let code = value
                .get_path(&["error", "code"])
                .and_then(Json::as_str)
                .unwrap_or("agent_error");
            let message = value
                .get_path(&["error", "message"])
                .and_then(Json::as_str)
                .unwrap_or("prompt 响应未携带错误信息");
            Some((id, Err(PromptError::new(code, message))))
        }
        None => None,
    }
}

// ---------------------------------------------------------------------------
// Router ingress 通道（TCP JSONL）
// ---------------------------------------------------------------------------

/// Router prompt 通道：每次 ask 新建 TCP 连接，一行请求 / 一行响应。
///
/// 无状态、可共享（`Arc` 下多线程 ask 安全；每次连接独立，`id` 仅用于
/// 请求/响应关联）。
pub struct RouterPromptChannel {
    addr: String,
    timeout_ms: u64,
    next_id: AtomicU64,
}

impl RouterPromptChannel {
    /// 以默认等待上限（`ask_timeout_ms()`）构造。
    pub fn new(addr: impl Into<String>) -> Self {
        Self {
            addr: addr.into(),
            timeout_ms: ask_timeout_ms(),
            next_id: AtomicU64::new(0),
        }
    }

    /// 显式指定单次等待上限（测试用；毫秒，<1 按 1 处理）。
    pub fn with_timeout_ms(addr: impl Into<String>, timeout_ms: u64) -> Self {
        Self {
            addr: addr.into(),
            timeout_ms: timeout_ms.max(1),
            next_id: AtomicU64::new(0),
        }
    }

    /// ingress 地址（诊断输出用）。
    pub fn addr(&self) -> &str {
        &self.addr
    }

    /// 带超时的 TCP 连接：用本次 ask 预算内的 [`CONNECT_TIMEOUT_MS`] 上限，
    /// 逐地址尝试 `connect_timeout`（P8：不再无超时地等 connect）。
    fn connect(&self, deadline: Instant) -> Result<TcpStream, PromptError> {
        let remaining = deadline
            .saturating_duration_since(Instant::now())
            .max(Duration::from_millis(1));
        let budget = remaining.min(Duration::from_millis(CONNECT_TIMEOUT_MS));
        let addrs: Vec<std::net::SocketAddr> = self
            .addr
            .as_str()
            .to_socket_addrs()
            .map_err(|error| unreachable_hint(&self.addr, error))?
            .collect();
        if addrs.is_empty() {
            return Err(unreachable_hint(&self.addr, "地址解析结果为空"));
        }
        let mut last_error = None;
        for addr in addrs {
            match TcpStream::connect_timeout(&addr, budget) {
                Ok(stream) => return Ok(stream),
                Err(error) => last_error = Some(error),
            }
        }
        Err(unreachable_hint(
            &self.addr,
            last_error.unwrap_or_else(|| std::io::Error::other("连接失败")),
        ))
    }

    /// ask 主体：按 `id` 过滤响应行，可被 `cancelled` 打断（P8）。
    fn ask_with(&self, prompt: &str, cancelled: &AtomicBool) -> Result<String, PromptError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed) + 1;
        let deadline = Instant::now() + Duration::from_millis(self.timeout_ms.max(1));
        let mut stream = self.connect(deadline)?;
        let write_timeout = deadline
            .saturating_duration_since(Instant::now())
            .max(Duration::from_millis(1));
        let _ = stream.set_write_timeout(Some(write_timeout));

        // 一行请求（无中间缓冲，直接写穿并 flush）。
        let mut request = prompt_request(id, prompt).into_bytes();
        request.push(b'\n');
        stream.write_all(&request).map_err(|error| {
            PromptError::new("agent_error", format!("发送 prompt 请求失败：{error}"))
        })?;
        stream.flush().map_err(|error| {
            PromptError::new("agent_error", format!("冲刷 prompt 请求失败：{error}"))
        })?;

        // 一行响应：只接受 id 匹配的行；非 JSONL / 迟到响应忽略（有上限）。
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        let mut ignored = 0usize;
        loop {
            if cancelled.load(Ordering::Relaxed) {
                return Err(PromptError::new(
                    "cancelled",
                    "pc.ask 已取消：不再等待 Router prompt 应答",
                ));
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(PromptError::new(
                    "timeout",
                    format!("等待 Router prompt 应答超时（{}ms）", self.timeout_ms),
                ));
            }
            // 分片读：每片不超过 CANCEL_CHECK_MS，以便及时观察取消标志。
            let slice = remaining.min(Duration::from_millis(CANCEL_CHECK_MS));
            let _ = reader.get_ref().set_read_timeout(Some(slice));
            match reader.read_line(&mut line) {
                Ok(0) => {
                    return Err(PromptError::new(
                        "agent_error",
                        "Router 提前关闭了 prompt 连接（未返回响应）",
                    ));
                }
                Ok(_) => {
                    let response = parse_response_line(&line);
                    line.clear();
                    match response {
                        Some((Some(rid), result)) if rid == id => return result,
                        Some(_) => {
                            // 迟到/无主响应：忽略，但不能无限忽略。
                            ignored += 1;
                            if ignored > MAX_IGNORED_LINES {
                                return Err(PromptError::new(
                                    "agent_error",
                                    format!(
                                        "Router prompt 通道连续返回 {ignored} 行不匹配 id 的响应（期望 id={id}）"
                                    ),
                                ));
                            }
                        }
                        None => {} // 非 JSONL 行：按契约忽略。
                    }
                }
                Err(error) if error.kind() == ErrorKind::Interrupted => {}
                Err(error)
                    if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) =>
                {
                    // 分片读超时：回到循环检查取消标志/总预算。
                }
                Err(error) => {
                    return Err(PromptError::new(
                        "agent_error",
                        format!("读取 Router prompt 应答失败：{error}"),
                    ));
                }
            }
        }
    }
}

/// Router 通道不可达错误：保留可操作提示（Router 默认 ingress 地址等）。
fn unreachable_hint(addr: &str, error: impl fmt::Display) -> PromptError {
    PromptError::new(
        "unreachable",
        format!(
            "无法连接 Router prompt 通道 {addr}：{error}。Router 可能未开启 prompt 通道：\
             sekai-router 交互/serve 模式默认监听 127.0.0.1:7422，\
             可用 --prompt-addr 指定实际地址"
        ),
    )
}

impl PromptTransport for RouterPromptChannel {
    fn ask(&self, prompt: &str) -> Result<String, PromptError> {
        self.ask_with(prompt, &NEVER_CANCELLED)
    }

    fn ask_cancellable(&self, prompt: &str, cancelled: &AtomicBool) -> Result<String, PromptError> {
        self.ask_with(prompt, cancelled)
    }
}

// ---------------------------------------------------------------------------
// DSH 插件通道（stdin/stdout JSONL）
// ---------------------------------------------------------------------------

/// 行式 I/O 抽象：DshPromptChannel 的可测试边界。
///
/// 生产实现 [`StdioLineIo`] 绑定进程 stdin/stdout；测试用 mpsc 内存对。
/// 读半侧与写半侧由调用方分别持有（见 [`DshPromptChannel::new`]），
/// 阻塞读不会占用写锁。
pub trait LineIo {
    /// 阻塞读一行（含换行）；EOF/错误返回 `None`。
    fn read_line(&mut self) -> Option<String>;
    /// 写一行（追加换行并 flush）；失败返回 `false`。
    fn write_line(&mut self, line: &str) -> bool;
}

/// 进程 stdin/stdout 的 JSONL 实现（`--llm dsh`：经 DSH 插件中转到 LLM）。
///
/// 无内部状态，可按需多实例（读半侧/写半侧各一）。写侧独占 stdout 锁，
/// 确保请求行不会与其它输出交错。
#[derive(Debug, Default, Clone, Copy)]
pub struct StdioLineIo;

impl LineIo for StdioLineIo {
    fn read_line(&mut self) -> Option<String> {
        let stdin = std::io::stdin();
        let mut line = String::new();
        loop {
            match stdin.read_line(&mut line) {
                Ok(0) => return None,
                Ok(_) => return Some(line),
                Err(error) if error.kind() == ErrorKind::Interrupted => continue,
                Err(_) => return None,
            }
        }
    }

    fn write_line(&mut self, line: &str) -> bool {
        let stdout = std::io::stdout();
        let mut lock = stdout.lock();
        lock.write_all(line.as_bytes()).is_ok()
            && lock.write_all(b"\n").is_ok()
            && lock.flush().is_ok()
    }
}

/// 通道共享状态（id 分配 + 已到响应 + EOF 标记）。
#[derive(Default)]
struct ChannelState {
    responses: Vec<(u64, Result<String, PromptError>)>,
    eof: bool,
}

/// DSH prompt 通道共享体：状态锁 + 唤醒条件变量。
struct ChannelShared {
    state: Mutex<ChannelState>,
    signal: Condvar,
}

impl ChannelShared {
    fn new() -> Self {
        Self {
            state: Mutex::new(ChannelState::default()),
            signal: Condvar::new(),
        }
    }
}

/// DSH prompt 通道：经本进程 stdin/stdout 与 DSH 插件交换 JSONL。
///
/// - `id` 在通道内单调递增，ask 写一行请求到 stdout（独占写锁）并等待
///   对应 `id` 的响应；
/// - 响应由读线程投递（[`feed_line`] / [`pump`]），行内非 JSON 或无匹配
///   id 一律忽略；
/// - 单次等待上限默认 `ask_timeout_ms()`。
///
/// [`feed_line`]: DshPromptChannel::feed_line
pub struct DshPromptChannel {
    reader: Arc<Mutex<Box<dyn LineIo + Send>>>,
    writer: Arc<Mutex<Box<dyn LineIo + Send>>>,
    shared: Arc<ChannelShared>,
    next_id: AtomicU64,
    timeout_ms: u64,
}

impl DshPromptChannel {
    /// 以默认等待上限构造；`reader`/`writer` 分别是读半侧与写半侧
    /// （分离两把锁：读线程阻塞读时 ask 仍可写）。
    pub fn new(
        reader: Arc<Mutex<Box<dyn LineIo + Send>>>,
        writer: Arc<Mutex<Box<dyn LineIo + Send>>>,
    ) -> Self {
        Self::with_timeout_ms(reader, writer, ask_timeout_ms())
    }

    /// 显式指定单次等待上限（测试用；毫秒，<1 按 1 处理）。
    pub fn with_timeout_ms(
        reader: Arc<Mutex<Box<dyn LineIo + Send>>>,
        writer: Arc<Mutex<Box<dyn LineIo + Send>>>,
        timeout_ms: u64,
    ) -> Self {
        Self {
            reader,
            writer,
            shared: Arc::new(ChannelShared::new()),
            next_id: AtomicU64::new(0),
            timeout_ms: timeout_ms.max(1),
        }
    }

    /// 投递一行响应（读线程调用）。行内非 JSON、缺 `id` 或缺 `ok` 时忽略并
    /// 返回 `false`；合法则登记结果、唤醒等待者并返回 `true`。
    ///
    /// 迟到/无主响应（没有 ask 在等它）仍会入队——但缓冲有上限
    ///（[`MAX_PENDING_RESPONSES`]），超出时丢弃最旧的，避免无限增长（P8）。
    pub fn feed_line(&self, line: &str) -> bool {
        let Some((Some(id), result)) = parse_response_line(line) else {
            return false;
        };
        let mut state = self.shared.state.lock().unwrap();
        state.responses.push((id, result));
        if state.responses.len() > MAX_PENDING_RESPONSES {
            let excess = state.responses.len() - MAX_PENDING_RESPONSES;
            state.responses.drain(..excess);
        }
        drop(state);
        self.shared.signal.notify_all();
        true
    }

    /// 标记通道 EOF（stdin 关闭）：唤醒全部等待者，后续 ask 立即失败。
    pub fn close(&self) {
        let mut state = self.shared.state.lock().unwrap();
        state.eof = true;
        drop(state);
        self.shared.signal.notify_all();
    }

    /// 读循环：阻塞读行并 [`feed_line`]，直到 EOF 后 [`close`]。
    ///
    /// 由主进程在 connect 模式启动的 stdin 读线程调用（`--llm dsh`）；
    /// 读半侧与写半侧锁分离，阻塞读不会饿死 ask 的写入。
    pub fn pump(&self) {
        loop {
            let line = {
                let mut reader = self.reader.lock().unwrap();
                reader.read_line()
            };
            match line {
                Some(line) => {
                    self.feed_line(&line);
                }
                None => {
                    self.close();
                    return;
                }
            }
        }
    }
}

impl PromptTransport for DshPromptChannel {
    fn ask(&self, prompt: &str) -> Result<String, PromptError> {
        self.ask_with(prompt, &NEVER_CANCELLED)
    }

    fn ask_cancellable(&self, prompt: &str, cancelled: &AtomicBool) -> Result<String, PromptError> {
        self.ask_with(prompt, cancelled)
    }
}

impl DshPromptChannel {
    /// ask 主体：写一行请求，等对应 `id` 的响应；`cancelled` 置位后尽快返回。
    fn ask_with(&self, prompt: &str, cancelled: &AtomicBool) -> Result<String, PromptError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed) + 1;
        let request = prompt_request(id, prompt);
        {
            // 独占写锁写一行请求；失败即管道关闭。
            let mut writer = self.writer.lock().unwrap();
            if !writer.write_line(&request) {
                return Err(PromptError::new(
                    "llm_unavailable",
                    "prompt 请求写入 stdout 失败：DSH 插件管道可能已关闭",
                ));
            }
        }

        let deadline = Instant::now() + Duration::from_millis(self.timeout_ms.max(1));
        let mut state = self.shared.state.lock().unwrap();
        loop {
            if cancelled.load(Ordering::Relaxed) {
                return Err(PromptError::new(
                    "cancelled",
                    "pc.ask 已取消：不再等待 DSH 插件应答",
                ));
            }
            if let Some(index) = state.responses.iter().position(|(rid, _)| *rid == id) {
                let (_, result) = state.responses.remove(index);
                return result;
            }
            if state.eof {
                return Err(PromptError::new(
                    "llm_unavailable",
                    "DSH prompt 通道已关闭（stdin EOF）：确认本进程由 DSH 插件以 --llm dsh 拉起",
                ));
            }
            let now = Instant::now();
            if now >= deadline {
                return Err(PromptError::new(
                    "timeout",
                    format!("等待 DSH prompt 应答超时（{}ms）", self.timeout_ms),
                ));
            }
            // 分片等待：每片不超过 CANCEL_CHECK_MS，以便及时观察取消标志。
            let slice = (deadline - now).min(Duration::from_millis(CANCEL_CHECK_MS));
            let (next, _) = self.shared.signal.wait_timeout(state, slice).unwrap();
            state = next;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::sync::mpsc;
    use std::thread;

    // ---- mpsc 内存对 LineIo（模拟 DSH 插件管道） ----

    /// 内存行对：两端互为对端，克隆共享同一对队列。
    #[derive(Clone)]
    struct MemLineIo {
        incoming: Arc<Mutex<mpsc::Receiver<String>>>,
        outgoing: mpsc::Sender<String>,
    }

    fn mem_pair() -> (MemLineIo, MemLineIo) {
        // 两条独立队列：worker→peer 与 peer→worker，互为对端。
        let (tx_worker_to_peer, rx_at_peer) = mpsc::channel();
        let (tx_peer_to_worker, rx_at_worker) = mpsc::channel();
        (
            MemLineIo {
                incoming: Arc::new(Mutex::new(rx_at_worker)),
                outgoing: tx_worker_to_peer,
            },
            MemLineIo {
                incoming: Arc::new(Mutex::new(rx_at_peer)),
                outgoing: tx_peer_to_worker,
            },
        )
    }

    impl LineIo for MemLineIo {
        fn read_line(&mut self) -> Option<String> {
            self.incoming.lock().unwrap().recv().ok()
        }

        fn write_line(&mut self, line: &str) -> bool {
            self.outgoing.send(String::from(line)).is_ok()
        }
    }

    fn boxed(io: MemLineIo) -> Arc<Mutex<Box<dyn LineIo + Send>>> {
        Arc::new(Mutex::new(Box::new(io) as Box<dyn LineIo + Send>))
    }

    // ---- 请求/响应解析 ----

    #[test]
    fn prompt_request_and_response_parsing() {
        let request = prompt_request(7, "你好\n世界");
        assert_eq!(
            request,
            "{\"id\":7,\"method\":\"prompt\",\"params\":{\"text\":\"你好\\n世界\"}}"
        );

        let ok = "{\"id\":7,\"ok\":true,\"result\":{\"answer\":\"答\",\"status\":\"completed\",\"steps\":2}}";
        let (id, result) = parse_response_line(ok).unwrap();
        assert_eq!(id, Some(7));
        assert_eq!(result.unwrap(), "答");

        let bad = "{\"id\":8,\"ok\":false,\"error\":{\"code\":\"llm_unavailable\",\"message\":\"额度用尽\"}}";
        let (id, result) = parse_response_line(bad).unwrap();
        assert_eq!(id, Some(8));
        let error = result.unwrap_err();
        assert_eq!(error.code, "llm_unavailable");
        assert_eq!(error.message, "额度用尽");

        // 非 JSON / 缺 ok → 忽略；缺 id 但结构合法 → 解析成功、id 为 None
        // （通道层 feed_line 会忽略无 id 行）。
        assert!(parse_response_line("not json").is_none());
        assert!(parse_response_line("{\"id\":1}").is_none());
        let (id, result) =
            parse_response_line("{\"ok\":true,\"result\":{\"answer\":\"x\"}}").unwrap();
        assert_eq!(id, None);
        assert_eq!(result.unwrap(), "x");
    }

    // ---- RouterPromptChannel ----

    /// 起一个回放服务：逐连接读取一行请求，按闭包生成响应行。
    fn spawn_router_server(respond: impl Fn(u64, String) -> String + Send + 'static) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").expect("绑定失败");
        let addr = listener.local_addr().unwrap().to_string();
        thread::spawn(move || {
            for stream in listener.incoming() {
                let mut stream = stream.expect("accept 失败");
                let mut reader = BufReader::new(stream.try_clone().expect("clone 失败"));
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap() == 0 {
                    continue;
                }
                let value = Json::parse(line.trim()).expect("请求应是 JSON");
                let id = value.get("id").and_then(Json::as_u64).unwrap_or(0);
                let text = value
                    .get_path(&["params", "text"])
                    .and_then(Json::as_str)
                    .unwrap_or_default()
                    .to_string();
                let response = respond(id, text);
                stream
                    .write_all(format!("{response}\n").as_bytes())
                    .and_then(|_| stream.flush())
                    .expect("写响应失败");
            }
        });
        addr
    }

    #[test]
    fn router_channel_success_and_error_passthrough() {
        // 第一次 ask 成功、第二次透传失败（每次 ask 一条新连接）。
        let addr = spawn_router_server(|id, text| {
            if text == "fail" {
                format!(
                    "{{\"id\":{id},\"ok\":false,\"error\":{{\"code\":\"llm_unavailable\",\"message\":\"额度用尽\"}}}}"
                )
            } else {
                format!(
                    "{{\"id\":{id},\"ok\":true,\"result\":{{\"answer\":\"echo:{text}\",\"status\":\"completed\",\"steps\":1}}}}"
                )
            }
        });
        let channel = RouterPromptChannel::with_timeout_ms(addr, 5_000);

        assert_eq!(channel.ask("hi").unwrap(), "echo:hi");
        let error = channel.ask("fail").unwrap_err();
        assert_eq!(error.code, "llm_unavailable");
        assert_eq!(error.message, "额度用尽");
    }

    #[test]
    fn router_channel_unreachable_mentions_hint() {
        // 绑定后立刻释放端口，制造连接拒绝。
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        drop(listener);

        let channel = RouterPromptChannel::with_timeout_ms(addr, 1_000);
        let error = channel.ask("hi").unwrap_err();
        assert_eq!(error.code, "unreachable");
        assert!(error.message.contains("sekai-router"), "{error}");
        assert!(error.message.contains("7422"), "{error}");
        assert!(error.message.contains("--prompt-addr"), "{error}");
    }

    // ---- DshPromptChannel ----

    #[test]
    fn dsh_channel_full_flow_over_memory_lines() {
        let (worker_io, peer_io) = mem_pair();
        let channel = Arc::new(DshPromptChannel::with_timeout_ms(
            boxed(worker_io.clone()),
            boxed(worker_io),
            5_000,
        ));

        // stdin 读线程（pump）。
        let pump_channel = channel.clone();
        thread::spawn(move || pump_channel.pump());

        // 扮演 DSH 插件：读一行请求，回一行成功响应。
        let peer = thread::spawn(move || {
            let mut peer = peer_io;
            let line = peer.read_line().expect("应收到请求行");
            let value = Json::parse(line.trim()).expect("请求应是 JSON");
            let id = value.get("id").and_then(Json::as_u64).unwrap_or(0);
            assert_eq!(
                value.get_path(&["params", "text"]).and_then(Json::as_str),
                Some("你好，sekai")
            );
            peer.write_line(&format!(
                "{{\"id\":{id},\"ok\":true,\"result\":{{\"answer\":\"你好，回答\",\"status\":\"completed\",\"steps\":3}}}}"
            ))
            .then_some(())
            .expect("写响应失败");

            // 第二轮：透传一个失败响应。
            let line = peer.read_line().expect("应收到第二个请求");
            let value = Json::parse(line.trim()).expect("请求应是 JSON");
            let id = value.get("id").and_then(Json::as_u64).unwrap_or(0);
            peer.write_line(&format!(
                "{{\"id\":{id},\"ok\":false,\"error\":{{\"code\":\"timeout\",\"message\":\"上游超时\"}}}}"
            ))
            .then_some(())
            .expect("写失败响应失败");
        });

        assert_eq!(channel.ask("你好，sekai").unwrap(), "你好，回答");
        let error = channel.ask("再问").unwrap_err();
        assert_eq!(error.code, "timeout");
        assert_eq!(error.message, "上游超时");
        peer.join().unwrap();

        // id 单调递增：两次请求的 id 应为 1、2。
        assert_eq!(channel.next_id.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn dsh_channel_ignores_garbage_lines() {
        let (worker_io, peer_io) = mem_pair();
        let channel = Arc::new(DshPromptChannel::with_timeout_ms(
            boxed(worker_io.clone()),
            boxed(worker_io),
            5_000,
        ));
        let pump_channel = channel.clone();
        thread::spawn(move || pump_channel.pump());

        // 先投垃圾行，再由对端回真实响应。
        assert!(!channel.feed_line("not json"));
        assert!(!channel.feed_line("{\"no\":\"id\"}"));
        let peer = thread::spawn(move || {
            let mut peer = peer_io;
            let line = peer.read_line().expect("应收到请求");
            let value = Json::parse(line.trim()).unwrap();
            let id = value.get("id").and_then(Json::as_u64).unwrap_or(0);
            peer.write_line("garbage").then_some(()).expect("写失败");
            peer.write_line("{\"id\":999,\"ok\":true,\"result\":{\"answer\":\"错位\"}}")
                .then_some(())
                .expect("写失败");
            peer.write_line(&format!(
                "{{\"id\":{id},\"ok\":true,\"result\":{{\"answer\":\"命中\"}}}}"
            ))
            .then_some(())
            .expect("写响应失败");
        });

        assert_eq!(channel.ask("问").unwrap(), "命中");
        peer.join().unwrap();
    }

    #[test]
    fn dsh_channel_times_out_without_response() {
        let (worker_io, _peer_alive) = mem_pair();
        let channel =
            DshPromptChannel::with_timeout_ms(boxed(worker_io.clone()), boxed(worker_io), 80);
        // 不读响应：ask 应在超时后失败。
        let started = Instant::now();
        let error = channel.ask("问").unwrap_err();
        assert_eq!(error.code, "timeout");
        assert!(started.elapsed() >= Duration::from_millis(80));
    }

    // ---- P8：响应 id 过滤 / 迟到响应清理 / 可取消 ----

    #[test]
    fn router_channel_ignores_mismatched_ids() {
        // 同 socket 上先回一个不匹配 id 的 ok 行：不能当作本次回答。
        let addr = spawn_router_server(|id, _text| {
            format!(
                "{{\"id\":{},\"ok\":true,\"result\":{{\"answer\":\"错位\"}}}}\n\
                 {{\"id\":{id},\"ok\":true,\"result\":{{\"answer\":\"命中\"}}}}",
                id + 1
            )
        });
        let channel = RouterPromptChannel::with_timeout_ms(addr, 5_000);
        assert_eq!(channel.ask("问").unwrap(), "命中");
    }

    #[test]
    fn router_channel_ask_cancellable_exits_quickly() {
        // 服务端保持连接但完全不响应：ask_cancellable 必须在取消后尽快退出，
        // 而不是等满整个 ask 预算（P8）。
        let listener = TcpListener::bind("127.0.0.1:0").expect("绑定失败");
        let addr = listener.local_addr().unwrap().to_string();
        thread::spawn(move || {
            if let Ok(stream) = listener.accept() {
                let _held = stream; // 保持连接打开、不发响应
                thread::sleep(Duration::from_secs(30));
            }
        });
        let channel = Arc::new(RouterPromptChannel::with_timeout_ms(addr, 60_000));
        let cancelled = Arc::new(AtomicBool::new(false));
        let cancel_flag = cancelled.clone();
        let ask_channel = channel.clone();
        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || {
            let result = ask_channel.ask_cancellable("问", &cancel_flag);
            let _ = sender.send(result);
        });
        thread::sleep(Duration::from_millis(50));
        cancelled.store(true, Ordering::Relaxed);
        let result = receiver
            .recv_timeout(Duration::from_secs(5))
            .expect("取消后 ask_cancellable 未在 5s 内返回");
        let error = result.expect_err("取消后不应返回成功");
        assert_eq!(error.code, "cancelled", "{}", error.message);
    }

    #[test]
    fn dsh_channel_bounds_late_responses() {
        // 迟到/无主响应不能无限增长：feed_line 只保留最近 MAX_PENDING_RESPONSES 条。
        let (worker_io, _peer_alive) = mem_pair();
        let channel =
            DshPromptChannel::with_timeout_ms(boxed(worker_io.clone()), boxed(worker_io), 100);
        for id in 0..(MAX_PENDING_RESPONSES as u64 * 4) {
            let line = format!(
                "{{\"id\":{},\"ok\":true,\"result\":{{\"answer\":\"迟到\"}}}}",
                id + 100
            );
            assert!(channel.feed_line(&line));
        }
        let state = channel.shared.state.lock().unwrap();
        assert!(
            state.responses.len() <= MAX_PENDING_RESPONSES,
            "迟到响应缓冲无限增长：{}",
            state.responses.len()
        );
        // 保留的是最近的响应（最旧的被丢弃）。
        let first_kept = 100 + MAX_PENDING_RESPONSES as u64 * 3;
        assert!(
            state.responses.iter().all(|(id, _)| *id >= first_kept),
            "{:?}",
            state.responses.first()
        );
    }

    #[test]
    fn dsh_channel_ask_cancellable_exits_quickly() {
        let (worker_io, _peer_alive) = mem_pair();
        let channel = Arc::new(DshPromptChannel::with_timeout_ms(
            boxed(worker_io.clone()),
            boxed(worker_io),
            60_000,
        ));
        let cancelled = Arc::new(AtomicBool::new(false));
        let cancel_flag = cancelled.clone();
        let ask_channel = channel.clone();
        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || {
            let result = ask_channel.ask_cancellable("问", &cancel_flag);
            let _ = sender.send(result);
        });
        thread::sleep(Duration::from_millis(50));
        cancelled.store(true, Ordering::Relaxed);
        let result = receiver
            .recv_timeout(Duration::from_secs(5))
            .expect("取消后 ask_cancellable 未在 5s 内返回");
        let error = result.expect_err("取消后不应返回成功");
        assert_eq!(error.code, "cancelled", "{}", error.message);
    }

    #[test]
    fn dsh_channel_fails_fast_on_eof() {
        let (worker_io, peer_io) = mem_pair();
        let channel = Arc::new(DshPromptChannel::with_timeout_ms(
            boxed(worker_io.clone()),
            boxed(worker_io),
            60_000,
        ));
        let pump_channel = channel.clone();
        thread::spawn(move || pump_channel.pump());
        // 对端直接消失 → 读半侧 EOF → 通道关闭；ask 应快速失败
        //（写失败或 EOF，均为 llm_unavailable）。
        drop(peer_io);
        let started = Instant::now();
        let error = channel.ask("问").expect_err("EOF 后 ask 不应成功");
        assert_eq!(error.code, "llm_unavailable");
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn stdio_line_io_is_default_constructible() {
        // 仅编译期形状检查：生产实现无状态、可复制。
        let _io = StdioLineIo;
        let _copy = _io;
    }
}
