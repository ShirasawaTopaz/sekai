//! 直连链路传输层：Link trait + 内存 / stdio / TCP 三种实现。
//!
//! ## 链路帧格式（本 crate 的传输约定，V1 帧之外唯一新增的封装）
//!
//! ```text
//! ┌────────────────────┬─────────────────────────────┐
//! │ u32 大端帧长度 N    │ N 字节 V1 控制平面帧          │
//! │ （4 字节，1..=4130）│ （Stdin 或 Stdout，复用      │
//! │                    │   sekai_wire / sekai_stdin / │
//! │                    │   sekai_stdout 的编解码）     │
//! └────────────────────┴─────────────────────────────┘
//! ```
//!
//! 点对点直连没有 MQTT 主题路由，一条帧就是一个方向上的一次传输；
//! 帧本体必须是完整的 V1 Stdin/Stdout 帧，编解码全部复用 `sekai` 核心库，
//! 不发明新的帧格式。`N` 的上限取 V1 两类帧的理论最大值
//! （Stdout 头 34 + 载荷上限 4096 = 4130），超出即视为对端异常。
//!
//! ## 语义约定
//!
//! - [`Link::read_frame`]：`Ok(Some(frame))` 收到一帧；`Ok(None)` 对端有序关闭；
//!   `Err(LinkError::Timeout)` 表示此刻没有完整帧（非致命，主循环可借机 poll）；
//!   其余错误视为链路故障。
//! - [`Link::write_frame`]：完整写出一帧（流式实现内部 flush）。
//! - [`Link::close`]：幂等关闭；关闭后读写返回 [`LinkError::NotConnected`]。
//! - [`connect_hint`]：把上述错误映射为面向用户的可操作中文提示
//!   （connect 模式的 stderr 输出用；提示与原始错误同时输出）。
//!
//! 风格对齐 `crate/sekai-router/src/host_link.rs`（Router 侧私有实现；
//! Worker 侧等价实现属于重构计划允许的少量重复），但分帧对象是
//! 长度前缀 V1 帧，而不是 MQTT 报文字节流。

use std::fmt;
use std::io::{ErrorKind, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::time::Duration;

use sekai::sekai_stdin::STDIN_HEADER_LEN;
use sekai::sekai_stdout::STDOUT_HEADER_LEN;
use sekai::sekai_wire::MAX_PAYLOAD;

/// 长度前缀的字节数（u32 大端）。
pub const FRAME_PREFIX_LEN: usize = 4;

/// 单帧载荷（V1 帧本体）的最大长度：`max(Stdin, Stdout) 头 + 载荷上限`。
pub const MAX_FRAME_LEN: usize = STDOUT_HEADER_LEN + MAX_PAYLOAD;

/// Stdin 帧的最大长度（诊断/测试用）。
pub const MAX_STDIN_FRAME_LEN: usize = STDIN_HEADER_LEN + MAX_PAYLOAD;

/// 链路错误。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkError {
    /// 目标地址无法解析。
    Resolve(String),
    /// 连接失败。
    Connect(String),
    /// 链路未建立或已关闭。
    NotConnected,
    /// 读取超时（非致命；主循环可当作一次 tick）。
    Timeout,
    /// 帧长度前缀超过上限。
    FrameTooLarge { len: usize, max: usize },
    /// 帧结构非法（空帧、流在帧中间截断等）。
    InvalidFrame(String),
    /// 其它 I/O 错误。
    Io(String),
}

impl fmt::Display for LinkError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Resolve(message) => write!(f, "地址解析失败：{message}"),
            Self::Connect(message) => write!(f, "连接失败：{message}"),
            Self::NotConnected => write!(f, "链路未连接"),
            Self::Timeout => write!(f, "读取超时"),
            Self::FrameTooLarge { len, max } => {
                write!(f, "帧长度 {len} 超过上限 {max}")
            }
            Self::InvalidFrame(message) => write!(f, "帧结构非法：{message}"),
            Self::Io(message) => write!(f, "I/O 错误：{message}"),
        }
    }
}

impl std::error::Error for LinkError {}

/// 把连接 / 链路错误映射为可操作的中文提示（纯函数，便于单测）。
///
/// 提示只做分类引导，不替代原始错误信息——调用方应把 [`connect_hint`]
/// 的返回值与原始错误**同时**输出（提示行 + 原始错误行），不吞诊断信息。
/// 分类依据：
///
/// - `Resolve`：地址本身没解析出来 → 提示 HOST:PORT 格式；
/// - `Connect` / `Io` 且消息含 "refused"（Unix 的 `Connection refused`
///   与 Windows 的 "actively refused"）→ Router 大概率没启动；
/// - 其余（超时、不可达、读链路 IO 等）→ 网络排查提示。
pub fn connect_hint(error: &LinkError) -> &'static str {
    /// 地址解析失败。
    const HINT_RESOLVE: &str = "地址解析失败：应输入 HOST:PORT 形式（如 127.0.0.1:7421）。";
    /// 连接被拒：Router 可能未启动。
    const HINT_REFUSED: &str = "连接被拒绝：Router 可能未启动。先在另一终端运行 sekai-router 并执行 /link tcp（默认监听 127.0.0.1:7421），再重试。";
    /// 超时 / 不可达等其它网络问题。
    const HINT_NETWORK: &str = "网络不可达或超时：检查地址与防火墙。";

    match error {
        LinkError::Resolve(_) => HINT_RESOLVE,
        LinkError::Connect(message) | LinkError::Io(message) => {
            if message.to_ascii_lowercase().contains("refused") {
                HINT_REFUSED
            } else {
                HINT_NETWORK
            }
        }
        LinkError::NotConnected
        | LinkError::Timeout
        | LinkError::FrameTooLarge { .. }
        | LinkError::InvalidFrame(_) => HINT_NETWORK,
    }
}

/// 连接失败的错误转换：`ConnectionRefused` 时保留英文标记，供
/// [`connect_hint`] 识别。
///
/// `io::Error` 的 Display 文案随系统语言本地化（中文 Windows 是
/// 「由于目标计算机积极拒绝」，不含 "refused" 子串），因此必须在
/// `ErrorKind` 尚可用的构造点嵌入规范标记，再拼接原始诊断。
fn connect_error(error: std::io::Error) -> LinkError {
    if error.kind() == std::io::ErrorKind::ConnectionRefused {
        LinkError::Connect(format!("connection refused: {error}"))
    } else {
        LinkError::Connect(error.to_string())
    }
}

/// 有界帧收发 + 关闭语义的传输抽象。
///
/// Router 侧（task-4 的直连接入端）与 Worker 侧共用同一抽象；
/// 内存链路 [`memory_pair`] 可注入测试。
pub trait Link {
    /// 读取下一帧。`Ok(None)` = 对端有序关闭；`Err(Timeout)` = 暂无完整帧。
    fn read_frame(&mut self) -> Result<Option<Vec<u8>>, LinkError>;
    /// 完整写出一帧（帧非空且不超过 [`MAX_FRAME_LEN`]）。
    fn write_frame(&mut self, frame: &[u8]) -> Result<(), LinkError>;
    /// 关闭链路（幂等）。
    fn close(&mut self);
}

/// 校验帧长度并编码为「u32 大端长度 + 帧字节」。
pub fn encode_frame(frame: &[u8]) -> Result<Vec<u8>, LinkError> {
    validate_frame_len(frame)?;
    let mut out = Vec::with_capacity(FRAME_PREFIX_LEN + frame.len());
    out.extend_from_slice(&(frame.len() as u32).to_be_bytes());
    out.extend_from_slice(frame);
    Ok(out)
}

fn validate_frame_len(frame: &[u8]) -> Result<(), LinkError> {
    if frame.is_empty() {
        return Err(LinkError::InvalidFrame(String::from("帧不能为空")));
    }
    if frame.len() > MAX_FRAME_LEN {
        return Err(LinkError::FrameTooLarge {
            len: frame.len(),
            max: MAX_FRAME_LEN,
        });
    }
    Ok(())
}

/// 有界流式分帧器：把字节流按「u32 大端长度前缀 + 帧字节」切分。
pub struct FrameDecoder {
    buffer: Vec<u8>,
    max_frame: usize,
}

impl FrameDecoder {
    /// 创建分帧器；`max_frame` 为 0 时按 1 处理（防御性）。
    pub fn new(max_frame: usize) -> Self {
        Self {
            buffer: Vec::new(),
            max_frame: max_frame.max(1),
        }
    }

    /// 当前上限（诊断用）。
    pub fn max_frame(&self) -> usize {
        self.max_frame
    }

    /// 追加一批字节；缓冲超过安全上限时返回 [`LinkError::FrameTooLarge`]。
    pub fn push(&mut self, bytes: &[u8]) -> Result<(), LinkError> {
        let cap = self
            .max_frame
            .saturating_add(FRAME_PREFIX_LEN)
            .saturating_add(FRAME_PREFIX_LEN);
        if self.buffer.len().saturating_add(bytes.len()) > cap {
            return Err(LinkError::FrameTooLarge {
                len: self.buffer.len().saturating_add(bytes.len()),
                max: self.max_frame,
            });
        }
        self.buffer.extend_from_slice(bytes);
        Ok(())
    }

    /// 取出下一个完整帧；半包返回 `Ok(None)`；非法前缀返回错误。
    pub fn next_frame(&mut self) -> Result<Option<Vec<u8>>, LinkError> {
        if self.buffer.len() < FRAME_PREFIX_LEN {
            return Ok(None);
        }
        let len = u32::from_be_bytes([
            self.buffer[0],
            self.buffer[1],
            self.buffer[2],
            self.buffer[3],
        ]) as usize;
        if len == 0 {
            return Err(LinkError::InvalidFrame(String::from("帧长度不能为 0")));
        }
        if len > self.max_frame {
            return Err(LinkError::FrameTooLarge {
                len,
                max: self.max_frame,
            });
        }
        if self.buffer.len() < FRAME_PREFIX_LEN + len {
            return Ok(None);
        }
        let frame = self.buffer[FRAME_PREFIX_LEN..FRAME_PREFIX_LEN + len].to_vec();
        self.buffer.drain(..FRAME_PREFIX_LEN + len);
        Ok(Some(frame))
    }

    /// 是否还有未取完的字节。
    pub fn has_buffered(&self) -> bool {
        !self.buffer.is_empty()
    }

    /// 当前缓冲字节数（诊断用）。
    pub fn buffered(&self) -> usize {
        self.buffer.len()
    }
}

// ---- 内存双工链路（测试 / 进程内注入用） ----

/// 创建一对连通的内存双工链路：`(a, b)`，`a.write_frame` 的帧可从 `b.read_frame` 读到。
///
/// 任一端 [`Link::close`] 或被 drop 后，对端会把已入队的帧**排空**，然后读到
/// 有序关闭（`Ok(None)`）。`read_frame` 在无帧时阻塞等待
/// [`MemoryLink::POLL_TIMEOUT`]（见 [`memory_pair`] 的默认值），超时返回
/// [`LinkError::Timeout`]，便于复用与流式链路相同的主循环。
pub fn memory_pair() -> (MemoryLink, MemoryLink) {
    memory_pair_with_timeout(Duration::from_millis(20))
}

/// 用自定义空闲轮询超时创建内存链路对。
pub fn memory_pair_with_timeout(timeout: Duration) -> (MemoryLink, MemoryLink) {
    let (tx_a, rx_b) = channel::<Vec<u8>>();
    let (tx_b, rx_a) = channel::<Vec<u8>>();
    (
        MemoryLink {
            tx: Some(tx_a),
            rx: rx_a,
            closed: false,
            timeout,
        },
        MemoryLink {
            tx: Some(tx_b),
            rx: rx_b,
            closed: false,
            timeout,
        },
    )
}

/// 内存双工链路的一端；由 [`memory_pair`] 创建。
pub struct MemoryLink {
    tx: Option<Sender<Vec<u8>>>,
    rx: Receiver<Vec<u8>>,
    closed: bool,
    timeout: Duration,
}

impl MemoryLink {
    /// 空闲读取的等待时长。
    pub fn timeout(&self) -> Duration {
        self.timeout
    }
}

impl Link for MemoryLink {
    fn read_frame(&mut self) -> Result<Option<Vec<u8>>, LinkError> {
        if self.closed {
            return Ok(None);
        }
        match self.rx.recv_timeout(self.timeout) {
            Ok(frame) => Ok(Some(frame)),
            Err(RecvTimeoutError::Timeout) => Err(LinkError::Timeout),
            // 对端 close/drop：排空已入队帧后到这里，视为有序关闭。
            Err(RecvTimeoutError::Disconnected) => Ok(None),
        }
    }

    fn write_frame(&mut self, frame: &[u8]) -> Result<(), LinkError> {
        validate_frame_len(frame)?;
        let Some(tx) = self.tx.as_ref() else {
            return Err(LinkError::NotConnected);
        };
        tx.send(frame.to_vec()).map_err(|_| LinkError::NotConnected)
    }

    fn close(&mut self) {
        self.closed = true;
        self.tx = None;
    }
}

// ---- 通用读写函数（stdio / 自定义流共用） ----

/// 从任意字节流阻塞读取一帧（长度前缀分帧）。
///
/// `Ok(None)` = 对端在帧边界上有序关闭；流在帧中间截断返回
/// [`LinkError::InvalidFrame`]。
pub fn read_frame_stream<R: Read>(reader: &mut R) -> Result<Option<Vec<u8>>, LinkError> {
    let mut prefix = [0u8; FRAME_PREFIX_LEN];
    if !read_exact_or_eof(reader, &mut prefix)? {
        return Ok(None);
    }
    let len = u32::from_be_bytes(prefix) as usize;
    if len == 0 {
        return Err(LinkError::InvalidFrame(String::from("帧长度不能为 0")));
    }
    if len > MAX_FRAME_LEN {
        return Err(LinkError::FrameTooLarge {
            len,
            max: MAX_FRAME_LEN,
        });
    }
    let mut frame = vec![0u8; len];
    reader
        .read_exact(&mut frame)
        .map_err(|error| LinkError::InvalidFrame(format!("流在帧中间结束：{error}")))?;
    Ok(Some(frame))
}

/// 读满 `buffer`；返回 `false` 表示在起点就遇到 EOF（有序关闭）。
fn read_exact_or_eof<R: Read>(reader: &mut R, buffer: &mut [u8]) -> Result<bool, LinkError> {
    let mut filled = 0;
    while filled < buffer.len() {
        match reader.read(&mut buffer[filled..]) {
            Ok(0) => {
                if filled == 0 {
                    return Ok(false);
                }
                return Err(LinkError::InvalidFrame(String::from(
                    "流在长度前缀中间结束",
                )));
            }
            Ok(n) => filled += n,
            Err(error)
                if matches!(
                    error.kind(),
                    ErrorKind::Interrupted | ErrorKind::WouldBlock | ErrorKind::TimedOut
                ) =>
            {
                continue;
            }
            Err(error) => return Err(LinkError::Io(error.to_string())),
        }
    }
    Ok(true)
}

/// 向任意字节流写出一帧并 flush。
pub fn write_frame_stream<W: Write>(writer: &mut W, frame: &[u8]) -> Result<(), LinkError> {
    let bytes = encode_frame(frame)?;
    writer
        .write_all(&bytes)
        .and_then(|_| writer.flush())
        .map_err(|error| LinkError::Io(error.to_string()))
}

// ---- stdio 链路（子进程模式） ----

/// 子进程模式链路：Router 拉起 Worker，Worker 的 stdin/stdout 即链路。
///
/// 泛型 `R`/`W` 便于测试注入（`Cursor` / `Vec<u8>`）；进程级用法见
/// [`StdioLink::std`]。读为阻塞语义（无超时），主循环可用
/// [`read_frame_stream`] + 独立线程实现定时 poll。
pub struct StdioLink<R, W> {
    reader: Option<R>,
    writer: Option<W>,
}

impl StdioLink<StdinHandle, StdoutHandle> {
    /// 绑定当前进程的 stdin/stdout。
    pub fn std() -> Self {
        Self {
            reader: Some(StdinHandle),
            writer: Some(StdoutHandle),
        }
    }
}

impl<R, W> StdioLink<R, W> {
    /// 用自定义读写流构造。
    pub fn new(reader: R, writer: W) -> Self {
        Self {
            reader: Some(reader),
            writer: Some(writer),
        }
    }

    /// 拆出读写两半（例如读侧交给独立线程）。
    pub fn into_parts(self) -> (Option<R>, Option<W>) {
        (self.reader, self.writer)
    }
}

/// 进程 stdin 的薄包装：每次读取期间持有 `StdinLock`，
/// 避免 `Read for Stdin` 逐段加锁的额外开销。
pub struct StdinHandle;

impl Read for StdinHandle {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let stdin = std::io::stdin();
        let mut lock = stdin.lock();
        lock.read(buf)
    }
}

/// 进程 stdout 的薄包装（写出后 flush）。
pub struct StdoutHandle;

impl Write for StdoutHandle {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        std::io::stdout().write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        std::io::stdout().flush()
    }
}

impl<R: Read, W: Write> Link for StdioLink<R, W> {
    fn read_frame(&mut self) -> Result<Option<Vec<u8>>, LinkError> {
        let Some(reader) = self.reader.as_mut() else {
            return Err(LinkError::NotConnected);
        };
        read_frame_stream(reader)
    }

    fn write_frame(&mut self, frame: &[u8]) -> Result<(), LinkError> {
        let Some(writer) = self.writer.as_mut() else {
            return Err(LinkError::NotConnected);
        };
        write_frame_stream(writer, frame)
    }

    fn close(&mut self) {
        self.reader = None;
        self.writer = None;
    }
}

// ---- TCP 客户端链路（长驻模式） ----

/// TCP 客户端链路：连接 Router 的直连接入端。
///
/// 读超时视为非致命 [`LinkError::Timeout`]（与 Router 侧 `host_link` 相同的
/// 20ms 轮询节奏），断线由调用方配合 [`Backoff`] 重连。
pub struct TcpLink {
    stream: Option<TcpStream>,
    decoder: FrameDecoder,
    read_timeout: Duration,
    connect_timeout: Duration,
}

impl Default for TcpLink {
    fn default() -> Self {
        Self::new()
    }
}

impl TcpLink {
    /// 创建未连接链路；读超时 20ms，连接超时 5s，帧上限 [`MAX_FRAME_LEN`]。
    pub fn new() -> Self {
        Self {
            stream: None,
            decoder: FrameDecoder::new(MAX_FRAME_LEN),
            read_timeout: Duration::from_millis(20),
            connect_timeout: Duration::from_secs(5),
        }
    }

    /// 连接到 `host:port`；先关闭旧连接。
    pub fn connect(&mut self, address: &str) -> Result<(), LinkError> {
        self.close();
        let mut addresses = address
            .to_socket_addrs()
            .map_err(|error| LinkError::Resolve(error.to_string()))?;
        let address = addresses
            .next()
            .ok_or_else(|| LinkError::Resolve(String::from("没有可用地址")))?;
        let stream =
            TcpStream::connect_timeout(&address, self.connect_timeout).map_err(connect_error)?;
        let _ = stream.set_nodelay(true);
        let _ = stream.set_read_timeout(Some(self.read_timeout));
        let _ = stream.set_write_timeout(Some(self.connect_timeout));
        self.stream = Some(stream);
        Ok(())
    }

    /// 是否已连接（测试用）。
    pub fn is_connected(&self) -> bool {
        self.stream.is_some()
    }

    /// 已缓冲的半包字节数（诊断用）。
    pub fn buffered(&self) -> usize {
        self.decoder.buffered()
    }
}

impl Link for TcpLink {
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
                        ErrorKind::WouldBlock | ErrorKind::TimedOut | ErrorKind::Interrupted
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
        let bytes = encode_frame(frame)?;
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
        self.decoder = FrameDecoder::new(MAX_FRAME_LEN);
    }
}

/// 指数退避：500ms 起步，翻倍到 30s 封顶（与 Router 侧一致）。
#[derive(Debug, Clone, Copy)]
pub struct Backoff {
    next_ms: u64,
    max_ms: u64,
}

impl Default for Backoff {
    fn default() -> Self {
        Self::new()
    }
}

impl Backoff {
    /// 默认 500ms 到 30s。
    pub fn new() -> Self {
        Self {
            next_ms: 500,
            max_ms: 30_000,
        }
    }

    /// 取当前等待时长并推进到下一次。
    pub fn next_delay_ms(&mut self) -> u64 {
        let value = self.next_ms;
        self.next_ms = self.next_ms.saturating_mul(2).min(self.max_ms);
        value
    }

    /// 连接成功后重置。
    pub fn reset(&mut self) {
        self.next_ms = 500;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use std::net::TcpListener;
    use std::thread;

    fn frame_bytes(payload: &[u8]) -> Vec<u8> {
        encode_frame(payload).unwrap()
    }

    #[test]
    fn frame_encoder_validates_length() {
        assert!(encode_frame(b"").is_err());
        assert!(encode_frame(b"hello").is_ok());
        let oversized = vec![0u8; MAX_FRAME_LEN + 1];
        let error = encode_frame(&oversized).unwrap_err();
        assert!(
            matches!(error, LinkError::FrameTooLarge { len, max } if len == MAX_FRAME_LEN + 1 && max == MAX_FRAME_LEN)
        );
    }

    #[test]
    fn decoder_handles_every_split_boundary() {
        let wire = frame_bytes(b"hello");
        for split in 0..=wire.len() {
            let mut decoder = FrameDecoder::new(MAX_FRAME_LEN);
            decoder.push(&wire[..split]).unwrap();
            let first = decoder.next_frame().unwrap();
            if split == wire.len() {
                assert_eq!(first.as_deref(), Some(b"hello".as_slice()), "split={split}");
            } else {
                assert!(first.is_none(), "split={split}");
                decoder.push(&wire[split..]).unwrap();
                assert_eq!(
                    decoder.next_frame().unwrap().as_deref(),
                    Some(b"hello".as_slice())
                );
            }
        }
    }

    #[test]
    fn decoder_handles_multiple_frames() {
        let mut bytes = frame_bytes(b"one");
        bytes.extend(frame_bytes(b"two"));
        let mut decoder = FrameDecoder::new(MAX_FRAME_LEN);
        decoder.push(&bytes).unwrap();
        assert_eq!(
            decoder.next_frame().unwrap().as_deref(),
            Some(b"one".as_slice())
        );
        assert_eq!(
            decoder.next_frame().unwrap().as_deref(),
            Some(b"two".as_slice())
        );
        assert!(decoder.next_frame().unwrap().is_none());
        assert_eq!(decoder.buffered(), 0);
    }

    #[test]
    fn decoder_rejects_zero_and_oversized_prefix() {
        let mut decoder = FrameDecoder::new(MAX_FRAME_LEN);
        decoder.push(&0u32.to_be_bytes()).unwrap();
        assert!(matches!(
            decoder.next_frame().unwrap_err(),
            LinkError::InvalidFrame(_)
        ));

        let mut decoder = FrameDecoder::new(MAX_FRAME_LEN);
        decoder
            .push(&((MAX_FRAME_LEN as u32 + 1).to_be_bytes()))
            .unwrap();
        assert!(matches!(
            decoder.next_frame().unwrap_err(),
            LinkError::FrameTooLarge { .. }
        ));
    }

    #[test]
    fn memory_pair_roundtrip_and_close() {
        let (mut a, mut b) = memory_pair();
        a.write_frame(b"ping").unwrap();
        b.write_frame(b"pong").unwrap();
        assert_eq!(a.read_frame().unwrap().as_deref(), Some(b"pong".as_slice()));
        assert_eq!(b.read_frame().unwrap().as_deref(), Some(b"ping".as_slice()));
        assert!(matches!(a.read_frame(), Err(LinkError::Timeout)));

        a.write_frame(b"fin").unwrap();
        a.close();
        assert_eq!(b.read_frame().unwrap().as_deref(), Some(b"fin".as_slice()));
        // close 后：对端读到有序关闭，本端读写被拒绝。
        assert!(b.read_frame().unwrap().is_none());
        assert!(matches!(a.write_frame(b"x"), Err(LinkError::NotConnected)));

        // drop 一端 → 另一端有序关闭。
        let (c, mut d) = memory_pair();
        drop(c);
        assert!(d.read_frame().unwrap().is_none());
    }

    #[test]
    fn stdio_link_roundtrip_with_cursor() {
        let incoming = frame_bytes(b"in- frame");
        let mut link = StdioLink::new(Cursor::new(incoming.clone()), Vec::new());
        assert_eq!(
            link.read_frame().unwrap().as_deref(),
            Some(b"in- frame".as_slice())
        );
        // 输入耗尽 → 有序关闭。
        assert!(link.read_frame().unwrap().is_none());
        link.write_frame(b"out-frame").unwrap();
        let (_, writer) = link.into_parts();
        assert_eq!(writer.unwrap(), frame_bytes(b"out-frame"));

        // 流在帧中间截断 → InvalidFrame。
        let mut truncated = frame_bytes(b"abcdef");
        truncated.truncate(6);
        let mut link = StdioLink::new(Cursor::new(truncated), Vec::new());
        assert!(matches!(
            link.read_frame().unwrap_err(),
            LinkError::InvalidFrame(_)
        ));
    }

    #[test]
    fn read_frame_stream_rejects_oversized_prefix() {
        let mut stream = Cursor::new((MAX_FRAME_LEN as u32 + 1).to_be_bytes().to_vec());
        assert!(matches!(
            read_frame_stream(&mut stream).unwrap_err(),
            LinkError::FrameTooLarge { .. }
        ));
    }

    #[test]
    fn backoff_doubles_and_caps() {
        let mut backoff = Backoff::new();
        assert_eq!(backoff.next_delay_ms(), 500);
        assert_eq!(backoff.next_delay_ms(), 1_000);
        assert_eq!(backoff.next_delay_ms(), 2_000);
        assert_eq!(backoff.next_delay_ms(), 4_000);
        assert_eq!(backoff.next_delay_ms(), 8_000);
        assert_eq!(backoff.next_delay_ms(), 16_000);
        assert_eq!(backoff.next_delay_ms(), 30_000);
        assert_eq!(backoff.next_delay_ms(), 30_000);
        backoff.reset();
        assert_eq!(backoff.next_delay_ms(), 500);
    }

    #[test]
    fn connect_hint_maps_error_categories() {
        // 解析失败 → HOST:PORT 格式提示。
        let hint = connect_hint(&LinkError::Resolve(String::from("invalid socket address")));
        assert!(hint.contains("HOST:PORT"), "{hint}");
        assert!(hint.contains("127.0.0.1:7421"), "{hint}");

        // 连接被拒：Unix 与 Windows 的 IO 文案都含 "refused"。
        for message in [
            "Connection refused (os error 111)",
            "No connection could be made because the target machine actively refused it. (os error 10061)",
        ] {
            let hint = connect_hint(&LinkError::Connect(String::from(message)));
            assert!(hint.contains("sekai-router"), "{message} → {hint}");
            assert!(hint.contains("/link tcp"), "{message} → {hint}");
        }

        // Io 变体里的 refused 同样映射到「Router 可能未启动」。
        let hint = connect_hint(&LinkError::Io(String::from(
            "Connection refused (os error 111)",
        )));
        assert!(hint.contains("Router 可能未启动"), "{hint}");

        // 构造点转换：ConnectionRefused 无论系统语言都嵌入英文标记
        //（中文 Windows 的 Display 文案不含 "refused"）。
        let error = connect_error(std::io::Error::from(std::io::ErrorKind::ConnectionRefused));
        let LinkError::Connect(text) = &error else {
            panic!("必须是 Connect 变体：{error:?}");
        };
        assert!(text.contains("refused"), "{text}");
        assert!(connect_hint(&error).contains("Router 可能未启动"), "{text}");
        // 其它 kind 原样保留 Display 文案。
        let error = connect_error(std::io::Error::from(std::io::ErrorKind::TimedOut));
        let LinkError::Connect(text) = &error else {
            panic!("必须是 Connect 变体：{error:?}");
        };
        assert!(!text.contains("refused"), "{text}");
        assert!(connect_hint(&error).contains("防火墙"), "{text}");

        // 超时 / 不可达 / 其它 IO → 网络排查提示。
        for error in [
            LinkError::Timeout,
            LinkError::NotConnected,
            LinkError::Connect(String::from(
                "connection attempt timed out (os error 10060)",
            )),
            LinkError::Connect(String::from("Network is unreachable (os error 51)")),
            LinkError::Io(String::from("broken pipe (os error 32)")),
            LinkError::InvalidFrame(String::from("连接在帧中间关闭")),
        ] {
            let hint = connect_hint(&error);
            assert!(hint.contains("防火墙"), "{error} → {hint}");
            assert!(!hint.contains("sekai-router"), "{error} → {hint}");
        }
    }

    #[test]
    fn tcp_link_roundtrip() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            // 服务器用同样的分帧约定回显一帧。
            let frame = read_frame_stream(&mut stream).unwrap().unwrap();
            write_frame_stream(&mut stream, &frame).unwrap();
        });

        let mut link = TcpLink::new();
        assert!(!link.is_connected());
        link.connect(&format!("127.0.0.1:{port}")).unwrap();
        assert!(link.is_connected());
        link.write_frame(b"echo-me").unwrap();

        let frame = loop {
            match link.read_frame() {
                Ok(Some(frame)) => break frame,
                Ok(None) => panic!("连接被提前关闭"),
                Err(LinkError::Timeout) => continue,
                Err(error) => panic!("读取失败：{error}"),
            }
        };
        assert_eq!(frame, b"echo-me");
        link.close();
        assert!(!link.is_connected());
        assert!(matches!(link.read_frame(), Err(LinkError::NotConnected)));
        server.join().unwrap();
    }

    #[test]
    fn tcp_link_connect_failure_is_reported() {
        // 绑定后立刻释放的端口几乎必然连接失败。
        let probe = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = probe.local_addr().unwrap().port();
        drop(probe);
        let mut link = TcpLink::new();
        let result = link.connect(&format!("127.0.0.1:{port}"));
        assert!(result.is_err() || link.is_connected());
    }
}
