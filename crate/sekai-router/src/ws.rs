//! RFC 6455 WebSocket 客户端子集（飞书长连接专用）。
//!
//! 只实现 Router 作为客户端所需的协议面（docs/sekai-feishu.md §3.2）：
//! 握手与 `Sec-WebSocket-Accept` 校验、二进制/文本帧收发、客户端帧 mask、
//! ping/pong/close 控制帧、接收侧 continuation 分片重组；不协商任何扩展
//! （收到 `Sec-WebSocket-Extensions` 响应头即视为握手失败），不实现服务端逻辑。
//!
//! 为什么自备实现而不用 `tungstenite`：全仓是同步 std 线程模型，引入现成库
//! 会拖进 tokio 运行时或额外 TLS 栈，而所需协议面只有数百行（见
//! docs/sekai-feishu.md §7.3 零依赖取舍）。SHA-1/base64 复用同批的
//! [`crate::sha1`]；握手随机量由系统时间与进程内计数器混合生成，不追求
//! 密码学强度（仅用于避免缓存串扰），也不引入外部 crate。
//!
//! 典型用法：先用 [`crate::http::connect_tls_stream`] 拿到裸流，再调用
//! [`WsStream::handshake`] 完成升级，之后循环 [`WsStream::read_message`] /
//! [`WsStream::send_binary`]；读超时由调用方按心跳节拍设置，超时只返回
//! [`WsError::Timeout`] 且不丢弃内部缓冲里的半帧字节。

use std::fmt;
use std::io::{self, Read, Write};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::sha1::{base64_encode, sha1};

/// RFC 6455 §1.3 规定的固定 GUID，用于推导 `Sec-WebSocket-Accept`。
pub const GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

/// 单帧（及单条消息）载荷上限：16 MiB，防止对端用谎言长度把内存撑爆。
const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;

/// 握手响应头上限：64 KiB，防止对端无限发送响应头。
const MAX_HANDSHAKE_BYTES: usize = 64 * 1024;

/// 握手被拒时保留的响应体上限（字节），只用于错误诊断。
const MAX_REJECTION_BODY_BYTES: usize = 512;

/// 读缓冲单次 `read` 的块大小。
const READ_CHUNK_BYTES: usize = 8 * 1024;

// 帧 opcode（RFC 6455 §5.2）。
const OPCODE_CONTINUATION: u8 = 0x0;
const OPCODE_TEXT: u8 = 0x1;
const OPCODE_BINARY: u8 = 0x2;
const OPCODE_CLOSE: u8 = 0x8;
const OPCODE_PING: u8 = 0x9;
const OPCODE_PONG: u8 = 0xA;

/// 进程内伪随机计数器：与系统时间混合，保证同一纳秒内的多次取数不同。
static RANDOM_COUNTER: AtomicU64 = AtomicU64::new(0);

/// WebSocket 客户端错误。
///
/// 错误文本一律中文；不说出对端 URL/地址（长连接 URL 的 query 含凭据，
/// 见 docs/sekai-feishu.md §6.3），[`WsError::Io`] 只记录错误类别。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WsError {
    /// 握手响应非法（缺头/Accept 不符/协商了扩展/状态行非法）。
    Handshake(String),
    /// 服务端以 HTTP 非 101 拒绝升级（403/514 等，由调用方分类永久/暂时）。
    HandshakeRejected {
        /// HTTP 状态码。
        status: u16,
        /// 响应体（已剔除换行，最多 512 字节）。
        body: String,
    },
    /// 底层 IO 错误（消息只写类别，不回显对端地址/URL）。
    Io(String),
    /// 读超时（不再有数据；调用方据此做心跳节拍）。
    Timeout,
    /// 协议违规（帧非法、服务端发 masked 帧、控制帧超长等）。
    Protocol(String),
    /// 连接已关闭（EOF / 收到 close）。
    Closed,
}

impl fmt::Display for WsError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WsError::Handshake(detail) => write!(formatter, "WebSocket 握手失败：{detail}"),
            WsError::HandshakeRejected { status, body } => {
                if body.is_empty() {
                    write!(formatter, "WebSocket 握手被拒绝：HTTP {status}")
                } else {
                    write!(formatter, "WebSocket 握手被拒绝：HTTP {status} {body}")
                }
            }
            WsError::Io(detail) => write!(formatter, "WebSocket 网络错误：{detail}"),
            WsError::Timeout => formatter.write_str("WebSocket 读超时"),
            WsError::Protocol(detail) => write!(formatter, "WebSocket 协议违规：{detail}"),
            WsError::Closed => formatter.write_str("WebSocket 连接已关闭"),
        }
    }
}

impl std::error::Error for WsError {}

/// 一条完整的 WebSocket 应用消息。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WsMessage {
    /// 二进制消息（飞书长连接里是一条 pbbp2.Frame）。
    Binary(Vec<u8>),
    /// 文本消息（UTF-8 已校验）。
    Text(String),
    /// 收到 close 帧（已自动回帧）。
    Close,
    /// 收到协议层 pong（心跳应答）：调用方据此判定连接仍然存活。
    Pong,
}

/// 已握手完成的 WebSocket 客户端流。
///
/// 泛型于底层 `Read + Write` 字节流（TLS 流、明文 TCP 流或测试内存流）；
/// 内部缓冲保留未消费字节，因此读超时不会丢帧。
pub struct WsStream<S: Read + Write> {
    /// 底层字节流。
    stream: S,
    /// 已读入但未消费的字节（握手残留与半帧都留在这里）。
    read_buf: Vec<u8>,
    /// [`Self::read_buf`] 中已消费的前缀长度。
    read_pos: usize,
    /// 正在重组的 continuation 分片消息（跨 read_message 调用保留）。
    partial: Option<PartialMessage>,
    /// 是否已经送出 close 帧（此后不再允许发送任何帧）。
    close_sent: bool,
    /// 是否已读到 EOF 或 close 帧（此后 read_message 返回 Closed）。
    read_done: bool,
}

impl<S: Read + Write> fmt::Debug for WsStream<S> {
    /// 手写 Debug：不要求底层流实现 Debug（TLS 流/测试假流通常没有），
    /// 只暴露缓冲与状态字段的长度，避免调试输出泄漏帧内容。
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WsStream")
            .field("buffered_bytes", &self.read_buf.len())
            .field("read_pos", &self.read_pos)
            .field(
                "partial_bytes",
                &self.partial.as_ref().map(|partial| partial.data.len()),
            )
            .field("close_sent", &self.close_sent)
            .field("read_done", &self.read_done)
            .finish_non_exhaustive()
    }
}

/// 正在重组的 continuation 分片消息。
struct PartialMessage {
    /// 首帧 opcode（0x1 文本 / 0x2 二进制）。
    opcode: u8,
    /// 已累积的载荷。
    data: Vec<u8>,
}

/// 从缓冲里解析出的一帧。
struct Frame {
    /// FIN 位。
    fin: bool,
    /// opcode。
    opcode: u8,
    /// 解 mask 后的载荷。
    payload: Vec<u8>,
}

impl<S: Read + Write> WsStream<S> {
    /// 在已建立的字节流上完成 WS 握手（发 GET + Upgrade，校验 101 与 Accept）。
    ///
    /// `host` 用于 `Host` 头（含端口时按主机名:端口传入），`path_and_query`
    /// 是请求目标（含 `?query`）。握手不带任何扩展；服务端若回应扩展头则失败。
    /// 握手响应之后若还残留字节，会作为首批帧数据留在内部缓冲里。
    pub fn handshake(mut stream: S, host: &str, path_and_query: &str) -> Result<Self, WsError> {
        if host.contains(['\r', '\n']) || path_and_query.contains(['\r', '\n']) {
            return Err(WsError::Handshake(String::from(
                "host 或请求路径含非法换行",
            )));
        }
        let path = if path_and_query.is_empty() {
            "/"
        } else {
            path_and_query
        };
        let key = websocket_key();
        let request = format!(
            "GET {path} HTTP/1.1\r\n\
             Host: {host}\r\n\
             Upgrade: websocket\r\n\
             Connection: Upgrade\r\n\
             Sec-WebSocket-Key: {key}\r\n\
             Sec-WebSocket-Version: 13\r\n\
             \r\n"
        );
        stream
            .write_all(request.as_bytes())
            .map_err(|error| map_write_error(error, "写握手请求"))?;
        stream
            .flush()
            .map_err(|error| map_write_error(error, "刷新握手请求"))?;

        let (raw, head_end) = read_handshake_response(&mut stream)?;
        let status = parse_status_line(&raw)?;
        if status != 101 {
            // 非 101：按 HTTP 拒绝升级处理，调用方据此分类永久/暂时。
            let body_start = head_end.map_or(raw.len(), |index| index + 4);
            return Err(WsError::HandshakeRejected {
                status,
                body: sanitize_rejection_body(&raw[body_start..]),
            });
        }
        let Some(head_end) = head_end else {
            return Err(WsError::Handshake(String::from(
                "握手响应不完整（101 响应头未结束连接即关闭）",
            )));
        };
        let headers = parse_headers(&raw[..head_end])?;
        validate_handshake_headers(&headers, &key)?;
        let leftover = raw[head_end + 4..].to_vec();
        Ok(Self {
            stream,
            read_buf: leftover,
            read_pos: 0,
            partial: None,
            close_sent: false,
            read_done: false,
        })
    }

    /// 发一条二进制消息（单帧，客户端侧必须 mask）。
    pub fn send_binary(&mut self, payload: &[u8]) -> Result<(), WsError> {
        if self.close_sent {
            return Err(WsError::Closed);
        }
        if payload.len() > MAX_FRAME_BYTES {
            return Err(WsError::Protocol(String::from(
                "发送载荷超过 16 MiB 单帧上限",
            )));
        }
        self.write_frame(OPCODE_BINARY, payload)
    }

    /// 发 ping（空 payload）。
    pub fn send_ping(&mut self) -> Result<(), WsError> {
        if self.close_sent {
            return Err(WsError::Closed);
        }
        self.write_frame(OPCODE_PING, &[])
    }

    /// 发 close（空 payload）；此后所有 `send_*` 返回 [`WsError::Closed`]。
    pub fn send_close(&mut self) -> Result<(), WsError> {
        if self.close_sent {
            return Err(WsError::Closed);
        }
        self.write_frame(OPCODE_CLOSE, &[])?;
        self.close_sent = true;
        Ok(())
    }

    /// 读一条完整应用消息。
    ///
    /// 协议层 ping 自动回 pong 后继续等；协议层 pong 作为 [`WsMessage::Pong`]
    /// 返回（心跳存活信号，不触发回帧）；continuation 分片按顺序重组（控制帧
    /// 可穿插其间）；收到 close 自动回 close 并返回 [`WsMessage::Close`]。
    /// 读超时返回 [`WsError::Timeout`] 且已读字节保留在内部缓冲，下次调用继续
    /// 解析。
    pub fn read_message(&mut self) -> Result<WsMessage, WsError> {
        loop {
            if self.read_done {
                return Err(WsError::Closed);
            }
            let Some(frame) = self.next_frame()? else {
                self.fill_read_buffer()?;
                continue;
            };
            match frame.opcode {
                OPCODE_CLOSE => {
                    // 回一帧 close（同 payload；已发过 close 时不重复发），
                    // 写失败也照样向上报告「已关闭」，避免丢信号。
                    if !self.close_sent {
                        let _ = self.write_frame(OPCODE_CLOSE, &frame.payload);
                        self.close_sent = true;
                    }
                    self.read_done = true;
                    return Ok(WsMessage::Close);
                }
                OPCODE_PING => {
                    self.write_frame(OPCODE_PONG, &frame.payload)?;
                }
                // 心跳应答：交给调用方刷新存活时间（飞书网关对 WS ping 回 WS pong）。
                OPCODE_PONG => return Ok(WsMessage::Pong),
                OPCODE_CONTINUATION => {
                    let Some(partial) = self.partial.as_mut() else {
                        return Err(WsError::Protocol(String::from(
                            "收到 continuation 帧但没有待续消息",
                        )));
                    };
                    if partial.data.len() + frame.payload.len() > MAX_FRAME_BYTES {
                        return Err(WsError::Protocol(String::from(
                            "分片消息总长超过 16 MiB 上限",
                        )));
                    }
                    partial.data.extend_from_slice(&frame.payload);
                    if frame.fin {
                        let partial = self
                            .partial
                            .take()
                            .expect("上一行已确认 partial 存在（逻辑不可达）");
                        return finish_message(partial.opcode, partial.data);
                    }
                }
                OPCODE_TEXT | OPCODE_BINARY => {
                    if self.partial.is_some() {
                        return Err(WsError::Protocol(String::from(
                            "上一条分片消息尚未结束又收到新的数据帧",
                        )));
                    }
                    if frame.fin {
                        return finish_message(frame.opcode, frame.payload);
                    }
                    self.partial = Some(PartialMessage {
                        opcode: frame.opcode,
                        data: frame.payload,
                    });
                }
                other => {
                    return Err(WsError::Protocol(format!("未知 opcode 0x{other:X}")));
                }
            }
        }
    }

    /// 从缓冲解析下一帧；数据不足时返回 `None`（等待更多字节）。
    ///
    /// 所有长度/控制帧校验都在解析帧头时完成，超大声明长度不会触发分配。
    fn next_frame(&mut self) -> Result<Option<Frame>, WsError> {
        let bytes = &self.read_buf[self.read_pos..];
        if bytes.len() < 2 {
            return Ok(None);
        }
        let first = bytes[0];
        let second = bytes[1];
        // 未协商扩展，RSV 位必须为 0。
        if first & 0x70 != 0 {
            return Err(WsError::Protocol(String::from(
                "RSV 位非零（未协商任何扩展）",
            )));
        }
        let fin = first & 0x80 != 0;
        let opcode = first & 0x0F;
        if !matches!(
            opcode,
            OPCODE_CONTINUATION
                | OPCODE_TEXT
                | OPCODE_BINARY
                | OPCODE_CLOSE
                | OPCODE_PING
                | OPCODE_PONG
        ) {
            return Err(WsError::Protocol(format!("未知 opcode 0x{opcode:X}")));
        }
        // 服务端→客户端帧不得 mask。
        if second & 0x80 != 0 {
            return Err(WsError::Protocol(String::from("服务端帧不得使用 mask")));
        }
        let length_marker = second & 0x7F;
        let (header_len, payload_len) = match length_marker {
            126 => {
                if bytes.len() < 4 {
                    return Ok(None);
                }
                let length = u16::from_be_bytes([bytes[2], bytes[3]]) as usize;
                (4, length)
            }
            127 => {
                if bytes.len() < 10 {
                    return Ok(None);
                }
                let mut raw = [0u8; 8];
                raw.copy_from_slice(&bytes[2..10]);
                let length = u64::from_be_bytes(raw);
                // RFC 6455 §5.2：64 位长度的最高位必须为 0。
                if length & 0x8000_0000_0000_0000 != 0 {
                    return Err(WsError::Protocol(String::from("64 位帧长度最高位非零")));
                }
                if length > u64::try_from(MAX_FRAME_BYTES).unwrap_or(u64::MAX) {
                    return Err(WsError::Protocol(String::from("帧长度超过 16 MiB 上限")));
                }
                (10, length as usize)
            }
            other => (2, other as usize),
        };
        if payload_len > MAX_FRAME_BYTES {
            return Err(WsError::Protocol(String::from("帧长度超过 16 MiB 上限")));
        }
        if opcode & 0x08 != 0 {
            // 控制帧：不得分片、载荷不超过 125 字节。
            if !fin {
                return Err(WsError::Protocol(String::from(
                    "控制帧不得分片（FIN 必须为 1）",
                )));
            }
            if payload_len > 125 {
                return Err(WsError::Protocol(String::from("控制帧载荷超过 125 字节")));
            }
        }
        let total = header_len + payload_len;
        if bytes.len() < total {
            return Ok(None);
        }
        let payload = bytes[header_len..total].to_vec();
        self.consume(total);
        Ok(Some(Frame {
            fin,
            opcode,
            payload,
        }))
    }

    /// 从底层流补读一块数据；超时/EOF 分别映射为 Timeout/Closed。
    fn fill_read_buffer(&mut self) -> Result<(), WsError> {
        let mut chunk = [0u8; READ_CHUNK_BYTES];
        loop {
            match self.stream.read(&mut chunk) {
                Ok(0) => {
                    self.read_done = true;
                    return Err(WsError::Closed);
                }
                Ok(read) => {
                    self.read_buf.extend_from_slice(&chunk[..read]);
                    return Ok(());
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(map_read_error(error, "读取帧")),
            }
        }
    }

    /// 消费缓冲前缀 `count` 字节；整块消费完则清空，避免缓冲长期占内存。
    fn consume(&mut self, count: usize) {
        self.read_pos += count;
        if self.read_pos >= self.read_buf.len() {
            self.read_buf.clear();
            self.read_pos = 0;
        } else if self.read_pos >= 64 * 1024 {
            self.read_buf.drain(..self.read_pos);
            self.read_pos = 0;
        }
    }

    /// 编码并写出一帧；长度按 7/16/64 位选择，载荷必须 mask。
    fn write_frame(&mut self, opcode: u8, payload: &[u8]) -> Result<(), WsError> {
        let mut frame = Vec::with_capacity(payload.len() + 14);
        frame.push(0x80 | opcode);
        let length = payload.len();
        if length <= 125 {
            frame.push(0x80 | length as u8);
        } else if length <= usize::from(u16::MAX) {
            frame.push(0x80 | 126);
            frame.extend_from_slice(&(length as u16).to_be_bytes());
        } else {
            frame.push(0x80 | 127);
            frame.extend_from_slice(&(length as u64).to_be_bytes());
        }
        let mask = random_mask();
        frame.extend_from_slice(&mask);
        frame.extend(
            payload
                .iter()
                .enumerate()
                .map(|(index, byte)| byte ^ mask[index % 4]),
        );
        self.stream
            .write_all(&frame)
            .map_err(|error| map_write_error(error, "发送帧"))?;
        self.stream
            .flush()
            .map_err(|error| map_write_error(error, "刷新发送缓冲"))?;
        Ok(())
    }
}

impl WsStream<Box<dyn crate::http::ReadWrite + Send>> {
    /// 设置底层读超时（None = 阻塞）。
    ///
    /// 用 UFCS 显式调用 [`crate::http::ReadWrite`] 的同名 trait 方法，避免与
    /// 本方法产生解析歧义。
    pub fn set_read_timeout(&mut self, timeout: Option<Duration>) -> Result<(), WsError> {
        crate::http::ReadWrite::set_read_timeout(&*self.stream, timeout)
            .map_err(|error| map_timeout_error(error, "设置读超时"))
    }

    /// 设置底层写超时（None = 阻塞）。
    pub fn set_write_timeout(&mut self, timeout: Option<Duration>) -> Result<(), WsError> {
        crate::http::ReadWrite::set_write_timeout(&*self.stream, timeout)
            .map_err(|error| map_timeout_error(error, "设置写超时"))
    }
}

/// `Sec-WebSocket-Accept` 推导：base64(sha1(key + GUID))。
pub fn accept_key(key: &str) -> String {
    let mut seed = Vec::with_capacity(key.len() + GUID.len());
    seed.extend_from_slice(key.as_bytes());
    seed.extend_from_slice(GUID.as_bytes());
    base64_encode(&sha1(&seed))
}

/// 生成握手用 16 字节随机 key 的 base64 文本（非密码学强度）。
fn websocket_key() -> String {
    let mut key = [0u8; 16];
    random_fill(&mut key);
    base64_encode(&key)
}

/// 生成 4 字节帧 mask key（非密码学强度）。
fn random_mask() -> [u8; 4] {
    let mut mask = [0u8; 4];
    random_fill(&mut mask);
    mask
}

/// 用系统时间纳秒与进程内计数器混合后填充随机字节。
///
/// 禁用外部 crate：这里只需要「不同连接/不同帧的 mask 不重复」，不需要密码学
/// 安全性；时间低位 + 全局单调计数器已足以避免缓存串扰。
fn random_fill(target: &mut [u8]) {
    for chunk in target.chunks_mut(8) {
        let word = random_u64().to_le_bytes();
        let take = chunk.len();
        chunk.copy_from_slice(&word[..take]);
    }
}

/// 生成一个 64 位伪随机字（时间纳秒 ⊕ 计数器，经 xorshift64* 混合）。
fn random_u64() -> u64 {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos() as u64)
        .unwrap_or(0);
    let counter = RANDOM_COUNTER.fetch_add(1, Ordering::Relaxed);
    let mut state = nanos ^ counter.rotate_left(32) ^ 0x9E37_79B9_7F4A_7C15;
    if state == 0 {
        state = 0x2545_F491_4F6C_DD1D;
    }
    for _ in 0..2 {
        state ^= state >> 12;
        state ^= state << 25;
        state ^= state >> 27;
    }
    state.wrapping_mul(0x2545_F491_4F6C_DD1D)
}

/// 读握手响应直到出现 `\r\n\r\n`；返回（已读字节，头部结束位置）。
fn read_handshake_response<S: Read>(stream: &mut S) -> Result<(Vec<u8>, Option<usize>), WsError> {
    let mut raw = Vec::new();
    loop {
        if let Some(index) = find_subslice(&raw, b"\r\n\r\n") {
            return Ok((raw, Some(index)));
        }
        if raw.len() >= MAX_HANDSHAKE_BYTES {
            return Err(WsError::Handshake(String::from(
                "握手响应头超过 64 KiB 上限",
            )));
        }
        let mut chunk = [0u8; 1024];
        match stream.read(&mut chunk) {
            Ok(0) => return Ok((raw, None)),
            Ok(read) => raw.extend_from_slice(&chunk[..read]),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(map_read_error(error, "读取握手响应")),
        }
    }
}

/// 解析状态行，返回 HTTP 状态码；状态行/版本/状态码非法均为握手失败。
fn parse_status_line(raw: &[u8]) -> Result<u16, WsError> {
    let line_end = find_subslice(raw, b"\r\n").unwrap_or(raw.len());
    let line = String::from_utf8_lossy(&raw[..line_end]);
    let mut parts = line.split_whitespace();
    let Some(version) = parts.next() else {
        return Err(WsError::Handshake(String::from("握手响应缺少状态行")));
    };
    let Some(rest) = version.strip_prefix("HTTP/1.") else {
        return Err(WsError::Handshake(String::from("状态行协议版本非法")));
    };
    if rest.len() != 1 || !rest.as_bytes()[0].is_ascii_digit() {
        return Err(WsError::Handshake(String::from("状态行协议版本非法")));
    }
    let Some(code_text) = parts.next() else {
        return Err(WsError::Handshake(String::from("状态行缺少状态码")));
    };
    code_text
        .parse::<u16>()
        .map_err(|_| WsError::Handshake(String::from("状态行状态码非法")))
}

/// 解析头部行（跳过状态行）；头名统一转小写，头值 trim。
fn parse_headers(head: &[u8]) -> Result<Vec<(String, String)>, WsError> {
    let text = String::from_utf8_lossy(head);
    let mut headers = Vec::new();
    for (index, line) in text.split("\r\n").enumerate() {
        if index == 0 || line.is_empty() {
            continue;
        }
        let Some((name, value)) = line.split_once(':') else {
            return Err(WsError::Handshake(format!(
                "握手响应头非法（缺少冒号）：{line}"
            )));
        };
        headers.push((name.trim().to_ascii_lowercase(), value.trim().to_string()));
    }
    Ok(headers)
}

/// 校验 101 响应的必选头：Upgrade / Connection / Accept，且不得协商扩展。
fn validate_handshake_headers(headers: &[(String, String)], key: &str) -> Result<(), WsError> {
    if headers
        .iter()
        .any(|(name, _)| name == "sec-websocket-extensions")
    {
        return Err(WsError::Handshake(String::from(
            "服务端协商了未请求的扩展（Sec-WebSocket-Extensions）",
        )));
    }
    if !header_contains_token(headers, "upgrade", "websocket") {
        return Err(WsError::Handshake(String::from(
            "响应缺少 Upgrade: websocket 头",
        )));
    }
    if !header_contains_token(headers, "connection", "upgrade") {
        return Err(WsError::Handshake(String::from(
            "响应 Connection 头不含 upgrade",
        )));
    }
    let expected = accept_key(key);
    let accept_matched = headers
        .iter()
        .filter(|(name, _)| name == "sec-websocket-accept")
        .any(|(_, value)| value.trim() == expected);
    if !accept_matched {
        return Err(WsError::Handshake(String::from(
            "Sec-WebSocket-Accept 与服务端应回值不符",
        )));
    }
    Ok(())
}

/// 头值按逗号拆成 token 后做大小写不敏感匹配（用于 Upgrade/Connection）。
fn header_contains_token(headers: &[(String, String)], name: &str, token: &str) -> bool {
    headers
        .iter()
        .filter(|(header_name, _)| header_name == name)
        .flat_map(|(_, value)| value.split(','))
        .any(|item| item.trim().eq_ignore_ascii_case(token))
}

/// 把拒绝升级的响应体压成单行错误诊断：剔除换行、最多 512 字节。
fn sanitize_rejection_body(body: &[u8]) -> String {
    let flattened: String = String::from_utf8_lossy(body)
        .chars()
        .filter(|character| *character != '\r' && *character != '\n')
        .collect();
    let mut truncated = String::with_capacity(flattened.len().min(MAX_REJECTION_BODY_BYTES));
    for character in flattened.chars() {
        if truncated.len() + character.len_utf8() > MAX_REJECTION_BODY_BYTES {
            break;
        }
        truncated.push(character);
    }
    truncated.trim().to_string()
}

/// 把完整数据消息转换成 [`WsMessage`]（文本必须为合法 UTF-8）。
fn finish_message(opcode: u8, data: Vec<u8>) -> Result<WsMessage, WsError> {
    match opcode {
        OPCODE_BINARY => Ok(WsMessage::Binary(data)),
        OPCODE_TEXT => match String::from_utf8(data) {
            Ok(text) => Ok(WsMessage::Text(text)),
            Err(_) => Err(WsError::Protocol(String::from("文本消息不是合法 UTF-8"))),
        },
        other => Err(WsError::Protocol(format!(
            "数据消息 opcode 非法：0x{other:X}"
        ))),
    }
}

/// 读错误映射：超时/回退到 [`WsError::Timeout`]，其余只报错误类别。
fn map_read_error(error: io::Error, action: &str) -> WsError {
    match error.kind() {
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut => WsError::Timeout,
        kind => WsError::Io(format!("{action}失败（{kind:?}）")),
    }
}

/// 写错误映射：超时/回退到 [`WsError::Timeout`]，其余只报错误类别。
fn map_write_error(error: io::Error, action: &str) -> WsError {
    match error.kind() {
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut => WsError::Timeout,
        kind => WsError::Io(format!("{action}失败（{kind:?}）")),
    }
}

/// 设置超时错误映射：这类失败只可能是本地流配置问题，按 IO 类别报告。
fn map_timeout_error(error: io::Error, action: &str) -> WsError {
    WsError::Io(format!("{action}失败（{:?}）", error.kind()))
}

/// 在字节串里查找子串的起始位置。
fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    // ---------------------------------------------------------------------
    // 内存假流：rx 是「服务端→客户端」字节，tx 记录「客户端→服务端」字节。
    // ---------------------------------------------------------------------

    /// 握手测试用的自动响应器：按已写出的请求生成响应字节。
    type Responder = Box<dyn Fn(&[u8]) -> Vec<u8> + Send>;

    /// 假流的共享状态（测试端持有 Arc 以便在 handshake 消费流之后观察 tx）。
    #[derive(Default)]
    struct MemState {
        /// 服务端待发字节。
        rx: Vec<u8>,
        /// rx 的读取位置（测试可重置以追加数据）。
        rx_pos: usize,
        /// 客户端写出的字节。
        tx: Vec<u8>,
        /// 为 true 且 rx 耗尽时 `read` 返回 WouldBlock（模拟读超时）。
        timeout: bool,
        /// 首次 read 且 rx 耗尽时，按已写出的请求生成响应（握手测试用）。
        responder: Option<Responder>,
        /// 记录最后一次 set_read_timeout 的值。
        read_timeout: Option<Duration>,
        /// 记录最后一次 set_write_timeout 的值。
        write_timeout: Option<Duration>,
    }

    /// 内存里的双向字节流。
    struct MemIo {
        shared: Arc<Mutex<MemState>>,
    }

    impl MemIo {
        /// 用预置的服务端字节构造（无自动响应）。
        fn new(rx: Vec<u8>) -> (Self, Arc<Mutex<MemState>>) {
            let shared = Arc::new(Mutex::new(MemState {
                rx,
                ..MemState::default()
            }));
            (
                Self {
                    shared: Arc::clone(&shared),
                },
                shared,
            )
        }

        /// 用「按请求生成握手响应」的闭包构造（用于握手测试）。
        fn with_responder(
            responder: impl Fn(&[u8]) -> Vec<u8> + Send + 'static,
        ) -> (Self, Arc<Mutex<MemState>>) {
            let shared = Arc::new(Mutex::new(MemState {
                responder: Some(Box::new(responder)),
                ..MemState::default()
            }));
            (
                Self {
                    shared: Arc::clone(&shared),
                },
                shared,
            )
        }

        /// 取共享状态（测试辅助）。
        fn state(shared: &Arc<Mutex<MemState>>) -> std::sync::MutexGuard<'_, MemState> {
            shared.lock().expect("测试锁不应中毒")
        }
    }

    impl Read for MemIo {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let mut state = self.shared.lock().expect("测试锁不应中毒");
            // 第一次读且还没预置响应时，按已写出的请求生成握手响应。
            let responder = if state.rx_pos >= state.rx.len() {
                state.responder.take()
            } else {
                None
            };
            if let Some(responder) = responder {
                let response = responder(&state.tx);
                state.rx = response;
                state.rx_pos = 0;
            }
            if state.rx_pos >= state.rx.len() {
                return if state.timeout {
                    Err(io::Error::new(io::ErrorKind::WouldBlock, "模拟读超时"))
                } else {
                    Ok(0)
                };
            }
            let take = (state.rx.len() - state.rx_pos).min(buf.len());
            buf[..take].copy_from_slice(&state.rx[state.rx_pos..state.rx_pos + take]);
            state.rx_pos += take;
            Ok(take)
        }
    }

    impl Write for MemIo {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            MemIo::state(&self.shared).tx.extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl crate::http::ReadWrite for MemIo {
        fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
            MemIo::state(&self.shared).read_timeout = timeout;
            Ok(())
        }

        fn set_write_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
            MemIo::state(&self.shared).write_timeout = timeout;
            Ok(())
        }
    }

    // ---------------------------------------------------------------------
    // 测试辅助：构造服务端帧、解析客户端帧、构造已完成握手的流。
    // ---------------------------------------------------------------------

    /// 构造服务端→客户端帧（服务端帧不 mask，FIN/opcode/长度按需）。
    fn server_frame(fin: bool, opcode: u8, payload: &[u8]) -> Vec<u8> {
        let mut frame = Vec::new();
        frame.push(if fin { 0x80 } else { 0x00 } | opcode);
        let length = payload.len();
        if length <= 125 {
            frame.push(length as u8);
        } else if length <= usize::from(u16::MAX) {
            frame.push(126);
            frame.extend_from_slice(&(length as u16).to_be_bytes());
        } else {
            frame.push(127);
            frame.extend_from_slice(&(length as u64).to_be_bytes());
        }
        frame.extend_from_slice(payload);
        frame
    }

    /// 从客户端写出的字节里解析一帧（校验 MASK 位、4 字节 mask key 并解 mask）。
    /// 返回（帧，消费的字节数）。
    fn parse_client_frame(bytes: &[u8]) -> (Frame, usize) {
        assert!(bytes.len() >= 2, "客户端帧至少 2 字节");
        let first = bytes[0];
        let second = bytes[1];
        let fin = first & 0x80 != 0;
        let opcode = first & 0x0F;
        assert_eq!(second & 0x80, 0x80, "客户端帧必须置 MASK 位");
        let (header_len, payload_len) = match second & 0x7F {
            126 => {
                assert!(bytes.len() >= 4, "16 位长度帧头不完整");
                (4, u16::from_be_bytes([bytes[2], bytes[3]]) as usize)
            }
            127 => {
                assert!(bytes.len() >= 10, "64 位长度帧头不完整");
                let mut raw = [0u8; 8];
                raw.copy_from_slice(&bytes[2..10]);
                (10, u64::from_be_bytes(raw) as usize)
            }
            other => (2, other as usize),
        };
        let consumed = header_len + 4 + payload_len;
        assert!(bytes.len() >= consumed, "帧长度声明所需字节超过实际字节");
        let mut mask = [0u8; 4];
        mask.copy_from_slice(&bytes[header_len..header_len + 4]);
        let payload: Vec<u8> = bytes[header_len + 4..header_len + 4 + payload_len]
            .iter()
            .enumerate()
            .map(|(index, byte)| byte ^ mask[index % 4])
            .collect();
        (
            Frame {
                fin,
                opcode,
                payload,
            },
            consumed,
        )
    }

    /// 构造一条已握手完成的流：rx 为服务端字节，内部缓冲为空。
    fn stream_with_rx(rx: Vec<u8>) -> (WsStream<MemIo>, Arc<Mutex<MemState>>) {
        let (io, shared) = MemIo::new(rx);
        (
            WsStream {
                stream: io,
                read_buf: Vec::new(),
                read_pos: 0,
                partial: None,
                close_sent: false,
                read_done: false,
            },
            shared,
        )
    }

    /// 构造 101 响应（`extra_headers` 需自带结尾 CRLF，可为空）。
    fn handshake_response(accept: &str, extra_headers: &str) -> Vec<u8> {
        format!(
            "HTTP/1.1 101 Switching Protocols\r\n\
             Upgrade: websocket\r\n\
             Connection: Upgrade\r\n\
             Sec-WebSocket-Accept: {accept}\r\n\
             {extra_headers}\r\n"
        )
        .into_bytes()
    }

    /// 构造 HTTP 拒绝升级响应。
    fn rejection_response(status_line: &str, body: &str) -> Vec<u8> {
        format!(
            "HTTP/1.1 {status_line}\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        )
        .into_bytes()
    }

    /// 从请求文本里取出某个头的值（头名大小写不敏感）。
    fn request_header(request: &[u8], name: &str) -> Option<String> {
        String::from_utf8_lossy(request)
            .split("\r\n")
            .find_map(|line| {
                let (key, value) = line.split_once(':')?;
                if key.trim().eq_ignore_ascii_case(name) {
                    Some(value.trim().to_string())
                } else {
                    None
                }
            })
    }

    // ---------------------------------------------------------------------
    // 握手
    // ---------------------------------------------------------------------

    /// RFC 6455 §1.3 的官方示例向量。
    #[test]
    fn accept_key符合rfc6455示例() {
        assert_eq!(
            accept_key("dGhlIHNhbXBsZSBub25jZQ=="),
            "s3pPLMBiTxaQ9kYGzzhZRbK+xOo="
        );
    }

    /// 握手成功：请求头齐全、随机 key 形状正确、不带任何扩展头。
    #[test]
    fn 握手成功且请求头不含扩展() {
        let (io, shared) = MemIo::with_responder(|request| {
            let key = request_header(request, "sec-websocket-key")
                .expect("请求必须携带 Sec-WebSocket-Key");
            handshake_response(&accept_key(&key), "")
        });
        let _stream = WsStream::handshake(io, "open.feishu.cn", "/callback/ws/endpoint?x=1")
            .expect("握手应成功");

        let state = MemIo::state(&shared);
        let request = String::from_utf8_lossy(&state.tx).to_string();
        assert!(
            request.starts_with("GET /callback/ws/endpoint?x=1 HTTP/1.1\r\n"),
            "请求行不符：{request}"
        );
        assert!(request.contains("\r\nHost: open.feishu.cn\r\n"));
        assert!(request.contains("\r\nUpgrade: websocket\r\n"));
        assert!(request.contains("\r\nConnection: Upgrade\r\n"));
        assert!(request.contains("\r\nSec-WebSocket-Version: 13\r\n"));
        let key = request_header(&state.tx, "sec-websocket-key").expect("应能解析出 key");
        assert_eq!(key.len(), 24, "16 字节 key 的 base64 长度应为 24");
        assert!(key.ends_with("=="), "16 字节 key 应带两个 = 填充");
        let lowered = request.to_ascii_lowercase();
        assert!(
            !lowered.contains("sec-websocket-extensions"),
            "不得请求扩展"
        );
        assert!(!lowered.contains("permessage-deflate"), "不得请求压缩扩展");
    }

    /// 握手响应里 Accept 与服务端应回值不符 → Handshake。
    #[test]
    fn 握手失败_accept不符() {
        let (io, _shared) = MemIo::with_responder(|_request| {
            handshake_response("AAAAAAAAAAAAAAAAAAAAAAAAAAA=", "")
        });
        let error = WsStream::handshake(io, "example.com", "/ws").expect_err("应握手失败");
        match error {
            WsError::Handshake(detail) => assert!(detail.contains("Accept"), "细节：{detail}"),
            other => panic!("应返回 Handshake，实际：{other:?}"),
        }
    }

    /// HTTP 403（永久拒绝）：HandshakeRejected 携带状态码与单行响应体。
    #[test]
    fn 握手失败_403返回拒绝体() {
        let (io, _shared) = MemIo::with_responder(|_request| {
            rejection_response("403 Forbidden", "forbidden\n第二行")
        });
        let error = WsStream::handshake(io, "example.com", "/ws").expect_err("应握手失败");
        match error {
            WsError::HandshakeRejected { status, body } => {
                assert_eq!(status, 403);
                assert_eq!(body, "forbidden第二行", "响应体应剔除换行");
            }
            other => panic!("应返回 HandshakeRejected，实际：{other:?}"),
        }
    }

    /// 拒绝体最多保留 512 字节。
    #[test]
    fn 握手失败_403体截断到512字节() {
        let long_body = "a".repeat(600);
        let (io, _shared) =
            MemIo::with_responder(move |_request| rejection_response("403 Forbidden", &long_body));
        let error = WsStream::handshake(io, "example.com", "/ws").expect_err("应握手失败");
        match error {
            WsError::HandshakeRejected { status, body } => {
                assert_eq!(status, 403);
                assert_eq!(body.len(), 512, "拒绝体应截断到 512 字节");
            }
            other => panic!("应返回 HandshakeRejected，实际：{other:?}"),
        }
    }

    /// 响应里出现 Sec-WebSocket-Extensions → 握手失败（不协商扩展）。
    #[test]
    fn 握手失败_响应带扩展头() {
        let (io, _shared) = MemIo::with_responder(|request| {
            let key = request_header(request, "sec-websocket-key").expect("请求应带 key");
            handshake_response(
                &accept_key(&key),
                "Sec-WebSocket-Extensions: permessage-deflate\r\n",
            )
        });
        let error = WsStream::handshake(io, "example.com", "/ws").expect_err("应握手失败");
        match error {
            WsError::Handshake(detail) => {
                assert!(detail.contains("扩展"), "细节：{detail}")
            }
            other => panic!("应返回 Handshake，实际：{other:?}"),
        }
    }

    /// 状态行非 101（如 200）→ HandshakeRejected，状态码如实带回。
    #[test]
    fn 握手失败_状态行非101() {
        let (io, _shared) = MemIo::with_responder(|_request| rejection_response("200 OK", ""));
        let error = WsStream::handshake(io, "example.com", "/ws").expect_err("应握手失败");
        assert_eq!(
            error,
            WsError::HandshakeRejected {
                status: 200,
                body: String::new()
            }
        );
    }

    /// 状态行本身非法（不是 HTTP/1.x）→ Handshake。
    #[test]
    fn 握手失败_状态行非法() {
        let (io, _shared) = MemIo::with_responder(|_request| b"NOT-HTTP\r\n\r\n".to_vec());
        let error = WsStream::handshake(io, "example.com", "/ws").expect_err("应握手失败");
        assert!(matches!(error, WsError::Handshake(_)), "实际：{error:?}");
    }

    /// 101 响应之后残留的字节属于 WebSocket 帧，必须留给后续读取。
    #[test]
    fn 握手响应后的残留字节作为首批帧数据() {
        let (io, _shared) = MemIo::with_responder(|request| {
            let key = request_header(request, "sec-websocket-key").expect("请求应带 key");
            let mut response = handshake_response(&accept_key(&key), "");
            response.extend_from_slice(&server_frame(true, OPCODE_BINARY, b"leftover"));
            response
        });
        let mut stream = WsStream::handshake(io, "example.com", "/ws").expect("握手应成功");
        assert_eq!(
            stream.read_message().expect("应读到残留帧"),
            WsMessage::Binary(b"leftover".to_vec())
        );
    }

    // ---------------------------------------------------------------------
    // 帧编解码
    // ---------------------------------------------------------------------

    /// 长度编码边界：125（7 位上限）、126/65535（16 位）、65536（64 位）。
    #[test]
    fn 帧长度边界_125_126_65535_65536() {
        for length in [0usize, 125, 126, 65_535, 65_536] {
            let payload: Vec<u8> = (0..length).map(|index| (index % 251) as u8).collect();
            let (mut stream, _shared) = stream_with_rx(server_frame(true, OPCODE_BINARY, &payload));
            let message = stream.read_message().expect("读取应成功");
            assert_eq!(
                message,
                WsMessage::Binary(payload),
                "载荷长度 {length} 读取结果不符"
            );
        }
    }

    /// 客户端发送的帧必须 mask；解 mask 后应还原原文；长度分别走 7/16/64 位。
    #[test]
    fn 发送二进制帧必须mask且可还原() {
        let (mut stream, shared) = stream_with_rx(Vec::new());
        let short = vec![0x11, 0x22, 0x33, 0x44, 0x55];
        let medium: Vec<u8> = (0..130).map(|index| (index % 251) as u8).collect();
        let large: Vec<u8> = (0..65_536).map(|index| (index % 251) as u8).collect();
        stream.send_binary(&short).expect("发送短帧应成功");
        stream.send_binary(&medium).expect("发送 16 位长度帧应成功");
        stream.send_binary(&large).expect("发送 64 位长度帧应成功");

        let state = MemIo::state(&shared);
        let (first, first_len) = parse_client_frame(&state.tx);
        assert_eq!(first.opcode, OPCODE_BINARY);
        assert!(first.fin, "数据帧必须 FIN=1");
        assert_eq!(first.payload, short);
        let (second, second_len) = parse_client_frame(&state.tx[first_len..]);
        assert_eq!(second.opcode, OPCODE_BINARY);
        assert_eq!(second.payload, medium, "130 字节帧应走 16 位长度");
        let (third, third_len) = parse_client_frame(&state.tx[first_len + second_len..]);
        assert_eq!(third.payload, large, "65536 字节帧应走 64 位长度");
        assert_eq!(
            first_len + second_len + third_len,
            state.tx.len(),
            "不应有残余字节"
        );
    }

    /// 二进制 continuation 三片重组为一条消息。
    #[test]
    fn continuation分片重组为二进制消息() {
        let mut rx = Vec::new();
        rx.extend_from_slice(&server_frame(false, OPCODE_BINARY, b"hel"));
        rx.extend_from_slice(&server_frame(false, OPCODE_CONTINUATION, b"lo w"));
        rx.extend_from_slice(&server_frame(true, OPCODE_CONTINUATION, b"orld"));
        let (mut stream, _shared) = stream_with_rx(rx);
        assert_eq!(
            stream.read_message().expect("读分片应成功"),
            WsMessage::Binary(b"hello world".to_vec())
        );
    }

    /// 文本 continuation 重组，且控制帧可穿插在分片之间（ping 立刻回 pong）。
    #[test]
    fn continuation分片重组为文本且控制帧可穿插() {
        let mut rx = Vec::new();
        rx.extend_from_slice(&server_frame(false, OPCODE_TEXT, b"hello "));
        rx.extend_from_slice(&server_frame(true, OPCODE_PING, b"ping"));
        rx.extend_from_slice(&server_frame(true, OPCODE_CONTINUATION, b"world"));
        let (mut stream, shared) = stream_with_rx(rx);
        assert_eq!(
            stream.read_message().expect("读分片应成功"),
            WsMessage::Text(String::from("hello world"))
        );
        let state = MemIo::state(&shared);
        let (pong, _) = parse_client_frame(&state.tx);
        assert_eq!(pong.opcode, OPCODE_PONG);
        assert_eq!(pong.payload, b"ping");
    }

    /// ping 自动回 pong 后继续读下一条应用消息（不把 ping 当消息返回）。
    #[test]
    fn 协议ping自动回pong后继续读取() {
        let mut rx = Vec::new();
        rx.extend_from_slice(&server_frame(true, OPCODE_PING, b"hi"));
        rx.extend_from_slice(&server_frame(true, OPCODE_BINARY, b"data"));
        let (mut stream, shared) = stream_with_rx(rx);
        assert_eq!(
            stream.read_message().expect("应跳过 ping 读到 data"),
            WsMessage::Binary(b"data".to_vec())
        );
        let state = MemIo::state(&shared);
        let (pong, consumed) = parse_client_frame(&state.tx);
        assert_eq!(pong.opcode, OPCODE_PONG);
        assert_eq!(pong.payload, b"hi");
        assert_eq!(consumed, state.tx.len());
    }

    /// pong 作为心跳存活信号返回，不触发回帧，也不影响后续消息。
    #[test]
    fn pong作为心跳信号返回() {
        let mut rx = Vec::new();
        rx.extend_from_slice(&server_frame(true, OPCODE_PONG, b"whatever"));
        rx.extend_from_slice(&server_frame(true, OPCODE_TEXT, b"hello"));
        let (mut stream, shared) = stream_with_rx(rx);
        assert_eq!(stream.read_message().expect("应返回 pong"), WsMessage::Pong);
        assert_eq!(
            stream.read_message().expect("pong 后继续读到文本"),
            WsMessage::Text(String::from("hello"))
        );
        assert!(MemIo::state(&shared).tx.is_empty(), "pong 不应触发回帧");
    }

    /// 收到 close：自动回 close 帧（同 payload），返回 Close；此后读写均视为关闭。
    #[test]
    fn 收到close回close并标记关闭() {
        let (mut stream, shared) = stream_with_rx(server_frame(true, OPCODE_CLOSE, b"\x03\xe8"));
        assert_eq!(
            stream.read_message().expect("应返回 Close"),
            WsMessage::Close
        );
        {
            let state = MemIo::state(&shared);
            let (close, consumed) = parse_client_frame(&state.tx);
            assert_eq!(close.opcode, OPCODE_CLOSE);
            assert_eq!(close.payload, b"\x03\xe8", "回帧应带回对端 payload");
            assert_eq!(consumed, state.tx.len());
        }
        assert_eq!(
            stream.send_binary(b"x").expect_err("关闭后发送应失败"),
            WsError::Closed
        );
        assert_eq!(
            stream.read_message().expect_err("关闭后再读应失败"),
            WsError::Closed
        );
    }

    /// 发送 ping/close 的帧结构；close 之后 send_* 一律 Closed。
    #[test]
    fn 发送ping与close帧结构正确() {
        let (mut stream, shared) = stream_with_rx(Vec::new());
        stream.send_ping().expect("发 ping 应成功");
        stream.send_close().expect("发 close 应成功");
        assert_eq!(
            stream.send_ping().expect_err("close 后发 ping 应失败"),
            WsError::Closed
        );
        let state = MemIo::state(&shared);
        let (ping, ping_len) = parse_client_frame(&state.tx);
        assert_eq!(ping.opcode, OPCODE_PING);
        assert!(ping.fin);
        assert!(ping.payload.is_empty());
        let (close, close_len) = parse_client_frame(&state.tx[ping_len..]);
        assert_eq!(close.opcode, OPCODE_CLOSE);
        assert!(close.payload.is_empty());
        assert_eq!(ping_len + close_len, state.tx.len());
    }

    // ---------------------------------------------------------------------
    // 超时与关闭
    // ---------------------------------------------------------------------

    /// 读超时返回 Timeout，且半帧字节保留在内部缓冲，补齐后可继续读出。
    #[test]
    fn 读超时返回timeout且不丢半帧数据() {
        let (mut stream, shared) = stream_with_rx(Vec::new());
        MemIo::state(&shared).timeout = true;
        assert_eq!(
            stream.read_message().expect_err("空流应读超时"),
            WsError::Timeout
        );

        // 只给半帧：帧头 + 部分载荷，再次读超时。
        let frame = server_frame(true, OPCODE_BINARY, b"abcdef");
        {
            let mut state = MemIo::state(&shared);
            state.rx = frame[..4].to_vec();
            state.rx_pos = 0;
        }
        assert_eq!(
            stream.read_message().expect_err("半帧应读超时"),
            WsError::Timeout
        );

        // 补齐剩余字节：能从缓冲 + 新数据拼出完整帧，证明没有丢字节。
        {
            let mut state = MemIo::state(&shared);
            state.rx = frame[4..].to_vec();
            state.rx_pos = 0;
            state.timeout = false;
        }
        assert_eq!(
            stream.read_message().expect("补齐后应读取成功"),
            WsMessage::Binary(b"abcdef".to_vec())
        );
    }

    /// EOF（对端直接关闭）→ Closed。
    #[test]
    fn 读到eof返回closed() {
        let (mut stream, _shared) = stream_with_rx(Vec::new());
        assert_eq!(
            stream.read_message().expect_err("EOF 应返回 Closed"),
            WsError::Closed
        );
    }

    // ---------------------------------------------------------------------
    // 协议违规
    // ---------------------------------------------------------------------

    /// 服务端帧带 mask 位 → Protocol（校验先于长度解析）。
    #[test]
    fn 协议违规_服务端mask帧() {
        let mut frame = server_frame(true, OPCODE_BINARY, b"x");
        frame[1] |= 0x80;
        let (mut stream, _shared) = stream_with_rx(frame);
        let error = stream.read_message().expect_err("masked 帧应失败");
        assert!(matches!(error, WsError::Protocol(_)), "实际：{error:?}");
    }

    /// 控制帧载荷超过 125 字节、或控制帧分片 → Protocol。
    #[test]
    fn 协议违规_控制帧超长或分片() {
        let oversized = server_frame(true, OPCODE_PING, &[0u8; 126]);
        let (mut stream, _shared) = stream_with_rx(oversized);
        assert!(
            matches!(
                stream.read_message().expect_err("超长控制帧应失败"),
                WsError::Protocol(_)
            ),
            "126 字节控制帧应判协议违规"
        );

        let fragmented = server_frame(false, OPCODE_PING, b"");
        let (mut stream, _shared) = stream_with_rx(fragmented);
        assert!(
            matches!(
                stream.read_message().expect_err("分片控制帧应失败"),
                WsError::Protocol(_)
            ),
            "控制帧 FIN=0 应判协议违规"
        );
    }

    /// 声明长度超过 16 MiB → Protocol（不等待、不分配载荷）。
    #[test]
    fn 协议违规_超大帧长度() {
        let mut frame = vec![0x80 | OPCODE_BINARY, 127];
        frame.extend_from_slice(&(MAX_FRAME_BYTES as u64 + 1).to_be_bytes());
        let (mut stream, _shared) = stream_with_rx(frame);
        let error = stream.read_message().expect_err("超大长度应失败");
        match error {
            WsError::Protocol(detail) => assert!(detail.contains("16 MiB"), "细节：{detail}"),
            other => panic!("应返回 Protocol，实际：{other:?}"),
        }
    }

    /// 64 位长度最高位非零（RFC 禁止）→ Protocol。
    #[test]
    fn 协议违规_64位长度最高位非零() {
        let mut frame = vec![0x80 | OPCODE_BINARY, 127];
        frame.extend_from_slice(&[0x80, 0, 0, 0, 0, 0, 0, 1]);
        let (mut stream, _shared) = stream_with_rx(frame);
        assert!(
            matches!(
                stream.read_message().expect_err("最高位非零应失败"),
                WsError::Protocol(_)
            ),
            "64 位长度最高位非零应判协议违规"
        );
    }

    /// 文本消息非法 UTF-8 → Protocol。
    #[test]
    fn 协议违规_文本非utf8() {
        let (mut stream, _shared) = stream_with_rx(server_frame(true, OPCODE_TEXT, &[0xFF, 0xFE]));
        let error = stream.read_message().expect_err("非法 UTF-8 应失败");
        match error {
            WsError::Protocol(detail) => assert!(detail.contains("UTF-8"), "细节：{detail}"),
            other => panic!("应返回 Protocol，实际：{other:?}"),
        }
    }

    /// 未知 opcode（0x3）→ Protocol。
    #[test]
    fn 协议违规_无法识别的opcode() {
        let (mut stream, _shared) = stream_with_rx(server_frame(true, 0x3, b"x"));
        assert!(
            matches!(
                stream.read_message().expect_err("未知 opcode 应失败"),
                WsError::Protocol(_)
            ),
            "未知 opcode 应判协议违规"
        );
    }

    // ---------------------------------------------------------------------
    // 其它
    // ---------------------------------------------------------------------

    /// 错误 Display 为中文，且可作为 `Box<dyn Error>` 使用。
    #[test]
    fn 错误display为中文且可作为error() {
        assert_eq!(
            WsError::Handshake(String::from("缺 Accept")).to_string(),
            "WebSocket 握手失败：缺 Accept"
        );
        assert_eq!(
            WsError::HandshakeRejected {
                status: 403,
                body: String::from("forbidden")
            }
            .to_string(),
            "WebSocket 握手被拒绝：HTTP 403 forbidden"
        );
        assert_eq!(
            WsError::Io(String::from("网络写失败（WouldBlock）")).to_string(),
            "WebSocket 网络错误：网络写失败（WouldBlock）"
        );
        assert_eq!(WsError::Timeout.to_string(), "WebSocket 读超时");
        assert_eq!(
            WsError::Protocol(String::from("帧非法")).to_string(),
            "WebSocket 协议违规：帧非法"
        );
        assert_eq!(WsError::Closed.to_string(), "WebSocket 连接已关闭");
        let boxed: Box<dyn std::error::Error> = Box::new(WsError::Timeout);
        assert_eq!(boxed.to_string(), "WebSocket 读超时");
    }

    /// 专用 impl 块的读写超时设置会透传到底层流。
    #[test]
    fn 设置读写超时透传到底层流() {
        let (io, shared) = MemIo::new(Vec::new());
        let mut stream: WsStream<Box<dyn crate::http::ReadWrite + Send>> = WsStream {
            stream: Box::new(io),
            read_buf: Vec::new(),
            read_pos: 0,
            partial: None,
            close_sent: false,
            read_done: false,
        };
        stream
            .set_read_timeout(Some(Duration::from_millis(250)))
            .expect("设置读超时应成功");
        stream.set_write_timeout(None).expect("设置写超时应成功");
        let state = MemIo::state(&shared);
        assert_eq!(state.read_timeout, Some(Duration::from_millis(250)));
        assert_eq!(state.write_timeout, None);
    }
}
