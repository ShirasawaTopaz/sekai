//! 飞书对话入口（`feishu.rs`）端到端集成测试（docs/sekai-feishu.md §10）。
//!
//! 全部离线：进程内起两个假服务端（均绑定 `127.0.0.1:0` 临时端口）
//!
//! - **假 HTTP**：`/callback/ws/endpoint`、`/open-apis/auth/v3/tenant_access_token/internal`、
//!   `/open-apis/im/v1/messages`（主动发送）与 `/open-apis/im/v1/messages/{id}/reply`
//!   （回复原消息），并把发送/回复的 `(会话/消息, 文本)` 经 mpsc 交给测试断言；
//! - **假 WS**：完成 RFC 6455 升级（用 [`sekai_router::ws::accept_key`] 算
//!   `Sec-WebSocket-Accept`），服务端帧不 mask（7/16 位长度），解析客户端
//!   mask 帧（ack/ping/close），支持推事件、断线与重连。
//!
//! chat 客户端与 Worker 网关均为进程内 mock（风格同 `tests/prompt_ingress.rs`）。
//!
//! 断言用的回复文案是 `feishu.rs` 里的私有常量，这里按设计文档 §5/§6 的
//! 用户可见文本逐字复刻；改动实现文案时本文件需同步。

use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use sekai_router::agent::AgentConfig;
use sekai_router::config::LinkMode;
use sekai_router::control::CommandRequest;
use sekai_router::feishu::{self, FeishuOptions};
use sekai_router::gateway::{CommandReceipt, GatewayError, GatewayHandle, WorkerGateway};
use sekai_router::json::Json;
use sekai_router::llm::{ChatClient, ChatRequest, ChatResponse, LlmError, Role, Usage};
use sekai_router::pbbp::{Frame, METHOD_DATA};
use sekai_router::prompt_server::PromptCore;
use sekai_router::tools::Policy;
use sekai_router::trace::TraceWriter;
use sekai_router::worker::WorkerInfo;
use sekai_router::ws::accept_key;

// ---------------------------------------------------------------------------
// 回复文案（`feishu.rs` 私有常量，逐字复刻）
// ---------------------------------------------------------------------------

const ACK_TEXT: &str = "已收到，正在处理…";
const DENIED_TEXT: &str = "未授权：本机器人未向你开放";
const RATE_TEXT: &str = "请求过于频繁，请稍后再试";
const NON_TEXT_TEXT: &str = "V1 仅支持文本消息";
const USAGE_TEXT: &str = "直接发文本，或 /status /cancel";
const TOO_LONG_TEXT: &str = "消息过长：请控制在 8192 字节以内";
const BUSY_CHAT_TEXT: &str = "上一条还在处理中，可发送 /cancel 中止";
const CANCEL_OK_TEXT: &str = "已请求取消";
const CANCEL_IDLE_TEXT: &str = "当前没有运行中的任务";

/// mock chat 的最终回答文本。
const ANSWER: &str = "答案";

// ---------------------------------------------------------------------------
// 进程内替身：chat 客户端 / Worker 网关
// ---------------------------------------------------------------------------

/// 固定回答的 chat 客户端；`delay` 非零即「慢聊天」（测忙拒与 /cancel）。
///
/// 所有请求（含 messages/tools）都记录在共享 `requests` 里，供多轮上下文
/// 断言读取；Clone 共享同一日志。
#[derive(Clone)]
struct TestChat {
    delay: Duration,
    requests: Arc<Mutex<Vec<ChatRequest>>>,
}

impl TestChat {
    fn instant() -> Self {
        Self {
            delay: Duration::ZERO,
            requests: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn slow() -> Self {
        Self {
            delay: Duration::from_millis(400),
            requests: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// 共享请求日志（在把 `TestChat` 移交给运行线程前取）。
    fn request_log(&self) -> Arc<Mutex<Vec<ChatRequest>>> {
        Arc::clone(&self.requests)
    }
}

impl ChatClient for TestChat {
    fn chat(&mut self, request: &ChatRequest) -> Result<ChatResponse, LlmError> {
        self.requests.lock().unwrap().push(request.clone());
        if !self.delay.is_zero() {
            thread::sleep(self.delay);
        }
        Ok(ChatResponse {
            model: String::from("test"),
            content: Some(String::from(ANSWER)),
            reasoning_content: None,
            tool_calls: Vec::new(),
            finish_reason: Some(String::from("stop")),
            usage: Usage {
                prompt_tokens: 3,
                completion_tokens: 2,
                total_tokens: 5,
            },
        })
    }
}

/// 可注入 Worker 快照的内存网关（Agent 只做一步文本回答，不会真正派工）。
struct MockGateway {
    workers: Arc<Mutex<Vec<WorkerInfo>>>,
}

impl WorkerGateway for MockGateway {
    fn list_workers(&mut self) -> Vec<WorkerInfo> {
        self.workers.lock().unwrap().clone()
    }

    fn submit(&mut self, _request: CommandRequest) -> Result<CommandReceipt, GatewayError> {
        let (sender, receiver) = mpsc::channel();
        let _ = sender.send(Err(GatewayError::new(
            "worker_offline",
            "测试网关不支持命令",
        )));
        Ok(CommandReceipt::new(receiver, 60_000, "内存"))
    }

    fn wait_ready(&mut self, _timeout_ms: u64) -> Result<(), GatewayError> {
        Ok(())
    }

    fn is_ready(&self) -> bool {
        true
    }
}

fn worker_info(worker_id: &str) -> WorkerInfo {
    WorkerInfo {
        worker_id: String::from(worker_id),
        online: true,
        boot_id: 1,
        firmware: String::from("0.1.0"),
        uptime_ms: 10,
        unix_ms: 1_700_000_000_000,
        tools: vec![String::from("gpio.info")],
        tasks: Vec::new(),
        workers: Vec::new(),
        active_tasks: Vec::new(),
        last_seen_mono_ms: 1,
        hello_seq: 1,
    }
}

fn mock_gateway() -> GatewayHandle {
    let workers = Arc::new(Mutex::new(vec![worker_info("w-1")]));
    Arc::new(Mutex::new(
        Box::new(MockGateway { workers }) as Box<dyn WorkerGateway>
    ))
}

fn agent_config() -> AgentConfig {
    AgentConfig {
        model: String::from("test"),
        max_steps: Some(4),
        ddl_ms: 1_000,
        timeout_ms: Some(3_000),
        temperature: 0.0,
        max_tokens: 256,
    }
}

fn policy() -> Policy {
    Policy {
        allow_write: false,
        allow_tasks: false,
        allow_admin: false,
        worker_filter: None,
        deny_llm_relay: true,
    }
}

// ---------------------------------------------------------------------------
// 事件 JSON 与 pbbp2 帧构造
// ---------------------------------------------------------------------------

/// 构造一条 `im.message.receive_v1` 事件 JSON（形状与 `feishu.rs` 的
/// `event_json` 单测模板一致；`content` 是二次编码的 JSON 字符串）。
fn event_json(
    event_id: &str,
    chat_type: &str,
    chat_id: &str,
    message_id: &str,
    message_type: &str,
    text: &str,
    mentions: &[&str],
) -> Vec<u8> {
    let content = if message_type == "text" {
        Json::object(vec![("text", Json::string(text))]).to_compact_string()
    } else {
        String::from("{}")
    };
    let mention_items: Vec<Json> = mentions
        .iter()
        .map(|key| {
            Json::object(vec![
                ("key", Json::string(*key)),
                ("name", Json::string("机器人")),
                (
                    "id",
                    Json::object(vec![("open_id", Json::string("ou_bot"))]),
                ),
            ])
        })
        .collect();
    let root = Json::object(vec![
        ("schema", Json::string("2.0")),
        (
            "header",
            Json::object(vec![
                ("event_id", Json::string(event_id)),
                ("event_type", Json::string("im.message.receive_v1")),
                ("create_time", Json::string("1700000000000")),
                ("token", Json::string("t")),
                ("app_id", Json::string("cli_x")),
                ("tenant_key", Json::string("tk")),
            ]),
        ),
        (
            "event",
            Json::object(vec![
                (
                    "sender",
                    Json::object(vec![
                        (
                            "sender_id",
                            Json::object(vec![
                                ("union_id", Json::string("on_1")),
                                ("user_id", Json::string("u_1")),
                                ("open_id", Json::string("ou_1")),
                            ]),
                        ),
                        ("sender_type", Json::string("user")),
                        ("tenant_key", Json::string("tk")),
                    ]),
                ),
                (
                    "message",
                    Json::object(vec![
                        ("message_id", Json::string(message_id)),
                        ("root_id", Json::string("")),
                        ("parent_id", Json::string("")),
                        ("create_time", Json::string("1700000000000")),
                        ("chat_id", Json::string(chat_id)),
                        ("chat_type", Json::string(chat_type)),
                        ("message_type", Json::string(message_type)),
                        ("content", Json::string(content)),
                        ("mentions", Json::array(mention_items)),
                    ]),
                ),
            ]),
        ),
    ]);
    root.to_compact_string().into_bytes()
}

/// 构造一条事件 DATA 帧（`sum`/`seq` 分片头按需附加）。
fn data_frame(message_id: &str, payload: Vec<u8>, sum: u32, seq: u32) -> Frame {
    let mut frame = Frame {
        seq_id: u64::from(seq) + 1,
        log_id: 0,
        service: 0,
        method: METHOD_DATA,
        headers: Vec::new(),
        payload_encoding: Some(String::from("json")),
        payload_type: Some(String::from("event/v2.0")),
        payload,
        log_id_new: None,
    };
    frame.set_header("type", "event");
    frame.set_header("message_id", message_id);
    if sum > 1 {
        frame.set_header("sum", sum.to_string());
        frame.set_header("seq", seq.to_string());
    }
    frame
}

/// 一条完整的 p2p/群聊文本事件帧。
fn text_event(
    event_id: &str,
    chat_type: &str,
    chat_id: &str,
    message_id: &str,
    text: &str,
) -> Frame {
    data_frame(
        message_id,
        event_json(event_id, chat_type, chat_id, message_id, "text", text, &[]),
        1,
        0,
    )
}

// ---------------------------------------------------------------------------
// 假 HTTP 服务端（endpoint / token / 发送 / 回复）
// ---------------------------------------------------------------------------

/// 测试观察到的飞书 REST 行为（mpsc 记录，按发生顺序）。
#[derive(Debug, Clone, PartialEq, Eq)]
enum Recorded {
    /// `POST /open-apis/im/v1/messages?receive_id_type=chat_id`（提示/ack）。
    Sent { chat_id: String, text: String },
    /// `POST /open-apis/im/v1/messages/{message_id}/reply`（答案）。
    Replied { message_id: String, text: String },
}

fn sent(record: &Recorded) -> Option<(&str, &str)> {
    match record {
        Recorded::Sent { chat_id, text } => Some((chat_id.as_str(), text.as_str())),
        Recorded::Replied { .. } => None,
    }
}

fn replied(record: &Recorded) -> Option<(&str, &str)> {
    match record {
        Recorded::Replied { message_id, text } => Some((message_id.as_str(), text.as_str())),
        Recorded::Sent { .. } => None,
    }
}

/// 假 HTTP 服务端共享状态。
struct HttpContext {
    /// WS 假服务端端口（拼进 endpoint 响应的连接 URL）。
    ws_port: u16,
    /// endpoint 调用次数（重连断言用）。
    endpoint_count: Arc<AtomicUsize>,
    /// 发送/回复记录出口。
    records: mpsc::Sender<Recorded>,
}

fn find_header_end(bytes: &[u8]) -> Option<usize> {
    bytes.windows(4).position(|window| window == b"\r\n\r\n")
}

/// 读一个 HTTP/1.1 请求（请求行 + headers + Content-Length body）。
fn read_http_request(stream: &mut TcpStream) -> Option<(String, String, Vec<u8>)> {
    let mut raw: Vec<u8> = Vec::new();
    let header_end = loop {
        if let Some(index) = find_header_end(&raw) {
            break index;
        }
        let mut buffer = [0u8; 4096];
        match stream.read(&mut buffer) {
            Ok(0) => return None,
            Ok(read) => raw.extend_from_slice(&buffer[..read]),
            Err(_) => return None,
        }
    };
    let head = String::from_utf8_lossy(&raw[..header_end]).to_string();
    let mut lines = head.split("\r\n");
    let request_line = lines.next()?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next()?.to_string();
    let path = parts.next()?.to_string();
    let mut content_length = 0usize;
    for line in lines {
        if let Some((name, value)) = line.split_once(':')
            && name.trim().eq_ignore_ascii_case("content-length")
        {
            content_length = value.trim().parse().unwrap_or(0);
        }
    }
    let mut body = raw[header_end + 4..].to_vec();
    while body.len() < content_length {
        let mut buffer = [0u8; 4096];
        match stream.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => body.extend_from_slice(&buffer[..read]),
            Err(_) => break,
        }
    }
    body.truncate(content_length);
    Some((method, path, body))
}

/// 回 JSON 响应并优雅关闭连接（客户端会一直读到 EOF）。
fn respond_json(stream: &mut TcpStream, body: &str) {
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.flush();
    let _ = stream.shutdown(Shutdown::Write);
    // 读掉客户端剩余字节直到 EOF：避免 Windows 上未读数据触发 RST 丢掉响应。
    let _ = stream.set_read_timeout(Some(Duration::from_millis(200)));
    let mut sink = [0u8; 256];
    loop {
        match stream.read(&mut sink) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
    }
}

/// 从请求 body 解析 `{"content":"{\"text\":\"...\"}"}` 里的文本。
fn body_content_text(body: &[u8]) -> Option<String> {
    let root = Json::parse(std::str::from_utf8(body).ok()?).ok()?;
    let content = root.get("content")?.as_str()?;
    let inner = Json::parse(content).ok()?;
    inner
        .get("text")
        .and_then(|value| value.as_str())
        .map(str::to_string)
}

/// 回复端点路径 → message_id（`/open-apis/im/v1/messages/{id}/reply`）。
fn reply_message_id(path: &str) -> Option<&str> {
    let rest = path.strip_prefix("/open-apis/im/v1/messages/")?;
    let (message_id, tail) = rest.split_once('/')?;
    (tail == "reply" && !message_id.is_empty()).then_some(message_id)
}

fn handle_http(mut stream: TcpStream, context: Arc<HttpContext>) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(3)));
    let Some((_method, path, body)) = read_http_request(&mut stream) else {
        return;
    };
    if path == "/callback/ws/endpoint" {
        context.endpoint_count.fetch_add(1, Ordering::SeqCst);
        let response = format!(
            r#"{{"code":0,"data":{{"URL":"ws://127.0.0.1:{}/ws?device_id=d1&service_id=s1","ClientConfig":{{"ReconnectCount":-1,"ReconnectInterval":1,"ReconnectNonce":0,"PingInterval":120}}}}}}"#,
            context.ws_port
        );
        respond_json(&mut stream, &response);
        return;
    }
    if path == "/open-apis/auth/v3/tenant_access_token/internal" {
        respond_json(
            &mut stream,
            r#"{"code":0,"tenant_access_token":"t-test","expire_in":7200}"#,
        );
        return;
    }
    if let Some(message_id) = reply_message_id(&path) {
        if let Some(text) = body_content_text(&body) {
            let _ = context.records.send(Recorded::Replied {
                message_id: String::from(message_id),
                text,
            });
        }
        respond_json(&mut stream, r#"{"code":0,"data":{}}"#);
        return;
    }
    if path.starts_with("/open-apis/im/v1/messages") {
        let parsed = Json::parse(std::str::from_utf8(&body).unwrap_or_default()).ok();
        let chat_id = parsed
            .as_ref()
            .and_then(|root| root.get("receive_id"))
            .and_then(|value| value.as_str());
        if let (Some(chat_id), Some(text)) = (chat_id, body_content_text(&body)) {
            let _ = context.records.send(Recorded::Sent {
                chat_id: String::from(chat_id),
                text,
            });
        }
        respond_json(&mut stream, r#"{"code":0,"data":{}}"#);
        return;
    }
    respond_json(&mut stream, r#"{"code":0,"data":{}}"#);
}

fn http_loop(listener: TcpListener, context: Arc<HttpContext>, stop: Arc<AtomicBool>) {
    loop {
        if stop.load(Ordering::SeqCst) {
            return;
        }
        match listener.accept() {
            Ok((stream, _)) => {
                let context = Arc::clone(&context);
                thread::spawn(move || handle_http(stream, context));
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(5));
            }
            Err(_) => return,
        }
    }
}

// ---------------------------------------------------------------------------
// 假 WS 服务端（握手 + 推事件 + 收 ack + 断线重连）
// ---------------------------------------------------------------------------

/// 假 WS 服务端共享状态。
struct WsContext {
    /// 当前连接的服务端写半（测试线程推事件用；断线后为 None）。
    writer: Arc<Mutex<Option<TcpStream>>>,
    /// 已完成的 WS 握手次数（重连断言用）。
    accept_count: Arc<AtomicUsize>,
    /// 收到的 ack 帧数（payload `{"code":200}`）。
    ack_count: Arc<AtomicUsize>,
}

/// 完成 WS 握手：读 HTTP 升级请求 → 用 `Sec-WebSocket-Key` 算 Accept → 回 101。
fn ws_handshake(stream: &mut TcpStream, stop: &AtomicBool) -> bool {
    let _ = stream.set_read_timeout(Some(Duration::from_millis(500)));
    let mut raw: Vec<u8> = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if stop.load(Ordering::SeqCst) {
            return false;
        }
        if find_header_end(&raw).is_some() {
            break;
        }
        if raw.len() > 64 * 1024 || Instant::now() >= deadline {
            return false;
        }
        let mut buffer = [0u8; 1024];
        match stream.read(&mut buffer) {
            Ok(0) => return false,
            Ok(read) => raw.extend_from_slice(&buffer[..read]),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                continue;
            }
            Err(_) => return false,
        }
    }
    let head = String::from_utf8_lossy(&raw).to_string();
    let Some(key) = head.split("\r\n").find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.trim()
            .eq_ignore_ascii_case("sec-websocket-key")
            .then(|| value.trim().to_string())
    }) else {
        return false;
    };
    let response = format!(
        "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {}\r\n\r\n",
        accept_key(&key)
    );
    stream.write_all(response.as_bytes()).is_ok() && stream.flush().is_ok()
}

/// 客户端 → 服务端帧解析结果。
enum ClientFrame {
    /// 帧还不完整，等更多字节。
    Incomplete,
    /// 协议非法（应断开）。
    Bad,
    /// 完整帧。
    Frame {
        opcode: u8,
        payload: Vec<u8>,
        consumed: usize,
    },
}

/// 解析一条客户端帧（RFC 6455 客户端帧必须 mask，读 4 字节 mask key 解掩）。
fn parse_client_frame(bytes: &[u8]) -> ClientFrame {
    if bytes.len() < 2 {
        return ClientFrame::Incomplete;
    }
    let opcode = bytes[0] & 0x0F;
    if bytes[1] & 0x80 == 0 {
        return ClientFrame::Bad;
    }
    let (header_len, payload_len) = match bytes[1] & 0x7F {
        126 => {
            if bytes.len() < 4 {
                return ClientFrame::Incomplete;
            }
            (4, u16::from_be_bytes([bytes[2], bytes[3]]) as usize)
        }
        127 => {
            if bytes.len() < 10 {
                return ClientFrame::Incomplete;
            }
            let mut raw = [0u8; 8];
            raw.copy_from_slice(&bytes[2..10]);
            (10, u64::from_be_bytes(raw) as usize)
        }
        marker => (2, marker as usize),
    };
    let total = header_len + 4 + payload_len;
    if bytes.len() < total {
        return ClientFrame::Incomplete;
    }
    let mask = &bytes[header_len..header_len + 4];
    let payload = bytes[header_len + 4..total]
        .iter()
        .enumerate()
        .map(|(index, byte)| byte ^ mask[index % 4])
        .collect();
    ClientFrame::Frame {
        opcode,
        payload,
        consumed: total,
    }
}

/// 读客户端帧直到连接关闭/停止：ack（0x2）计数，ping/close 忽略。
fn ws_read_loop(stream: &mut TcpStream, context: &WsContext, stop: &AtomicBool) {
    let mut buffer: Vec<u8> = Vec::new();
    loop {
        if stop.load(Ordering::SeqCst) {
            return;
        }
        loop {
            match parse_client_frame(&buffer) {
                ClientFrame::Incomplete => break,
                ClientFrame::Bad => return,
                ClientFrame::Frame {
                    opcode,
                    payload,
                    consumed,
                } => {
                    buffer.drain(..consumed);
                    if opcode == 0x2
                        && let Ok(frame) = Frame::decode(&payload)
                        && frame.payload == br#"{"code":200}"#
                    {
                        context.ack_count.fetch_add(1, Ordering::SeqCst);
                    }
                }
            }
        }
        let mut chunk = [0u8; 4096];
        match stream.read(&mut chunk) {
            Ok(0) => return,
            Ok(read) => buffer.extend_from_slice(&chunk[..read]),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                continue;
            }
            Err(_) => return,
        }
    }
}

fn ws_loop(listener: TcpListener, context: Arc<WsContext>, stop: Arc<AtomicBool>) {
    loop {
        if stop.load(Ordering::SeqCst) {
            return;
        }
        match listener.accept() {
            Ok((mut stream, _)) => {
                let _ = stream.set_nonblocking(false);
                if !ws_handshake(&mut stream, &stop) {
                    continue;
                }
                let _ = stream.set_read_timeout(Some(Duration::from_millis(100)));
                let writer = stream.try_clone().ok();
                *context
                    .writer
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()) = writer;
                context.accept_count.fetch_add(1, Ordering::SeqCst);
                ws_read_loop(&mut stream, &context, &stop);
                *context
                    .writer
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(5));
            }
            Err(_) => return,
        }
    }
}

// ---------------------------------------------------------------------------
// MockFeishu：两个假服务端 + 记录通道
// ---------------------------------------------------------------------------

struct MockFeishu {
    /// 假 HTTP 服务端地址（拼 `FeishuOptions.domain`）。
    domain: String,
    stop: Arc<AtomicBool>,
    writer: Arc<Mutex<Option<TcpStream>>>,
    accept_count: Arc<AtomicUsize>,
    endpoint_count: Arc<AtomicUsize>,
    ack_count: Arc<AtomicUsize>,
    records: mpsc::Receiver<Recorded>,
    /// 保持记录通道发送端存活（否则接收端可能看到 Disconnected）。
    #[allow(dead_code)]
    keepalive: mpsc::Sender<Recorded>,
    http_thread: Option<thread::JoinHandle<()>>,
    ws_thread: Option<thread::JoinHandle<()>>,
}

impl MockFeishu {
    fn start() -> Self {
        let http_listener = TcpListener::bind("127.0.0.1:0").expect("绑定假 HTTP 端口");
        http_listener
            .set_nonblocking(true)
            .expect("假 HTTP listener 设为非阻塞");
        let ws_listener = TcpListener::bind("127.0.0.1:0").expect("绑定假 WS 端口");
        ws_listener
            .set_nonblocking(true)
            .expect("假 WS listener 设为非阻塞");
        let http_port = http_listener.local_addr().expect("假 HTTP 地址").port();
        let ws_port = ws_listener.local_addr().expect("假 WS 地址").port();
        let stop = Arc::new(AtomicBool::new(false));
        let (records_tx, records_rx) = mpsc::channel();
        let endpoint_count = Arc::new(AtomicUsize::new(0));
        let accept_count = Arc::new(AtomicUsize::new(0));
        let ack_count = Arc::new(AtomicUsize::new(0));
        let writer: Arc<Mutex<Option<TcpStream>>> = Arc::new(Mutex::new(None));

        let http_context = Arc::new(HttpContext {
            ws_port,
            endpoint_count: Arc::clone(&endpoint_count),
            records: records_tx.clone(),
        });
        let http_thread = {
            let stop = Arc::clone(&stop);
            thread::spawn(move || http_loop(http_listener, http_context, stop))
        };
        let ws_context = Arc::new(WsContext {
            writer: Arc::clone(&writer),
            accept_count: Arc::clone(&accept_count),
            ack_count: Arc::clone(&ack_count),
        });
        let ws_thread = {
            let stop = Arc::clone(&stop);
            thread::spawn(move || ws_loop(ws_listener, ws_context, stop))
        };

        Self {
            domain: format!("http://127.0.0.1:{http_port}"),
            stop,
            writer,
            accept_count,
            endpoint_count,
            ack_count,
            records: records_rx,
            keepalive: records_tx,
            http_thread: Some(http_thread),
            ws_thread: Some(ws_thread),
        }
    }

    /// 等待第 `count` 次 WS 握手完成（含重连后的新连接）。
    fn wait_connected(&self, count: usize, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        while self.accept_count.load(Ordering::SeqCst) < count {
            assert!(
                Instant::now() < deadline,
                "等待第 {count} 个假 WS 连接超时（当前 {}）",
                self.accept_count.load(Ordering::SeqCst)
            );
            thread::sleep(Duration::from_millis(5));
        }
    }

    /// 等待收到 `count` 个 ack 帧。
    fn wait_ack(&self, count: usize, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        while self.ack_count.load(Ordering::SeqCst) < count {
            assert!(
                Instant::now() < deadline,
                "等待第 {count} 个 ack 帧超时（当前 {}）",
                self.ack_count.load(Ordering::SeqCst)
            );
            thread::sleep(Duration::from_millis(5));
        }
    }

    fn endpoint_calls(&self) -> usize {
        self.endpoint_count.load(Ordering::SeqCst)
    }

    fn ack_frames(&self) -> usize {
        self.ack_count.load(Ordering::SeqCst)
    }

    /// 推一条 pbbp2 事件帧（编码后作为一条 WS 二进制消息）。
    fn push_event(&self, frame: Frame) {
        self.send_raw_payload(frame.encode());
    }

    /// 把一段原始字节作为一条 WS 二进制消息推给客户端。
    fn send_raw_payload(&self, payload: Vec<u8>) {
        let mut writer = self
            .writer
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(stream) = writer.as_mut() else {
            panic!("假 WS 尚未建立连接，无法推事件");
        };
        let mut frame = Vec::with_capacity(payload.len() + 10);
        frame.push(0x80 | 0x2);
        if payload.len() <= 125 {
            frame.push(payload.len() as u8);
        } else if payload.len() <= usize::from(u16::MAX) {
            frame.push(126);
            frame.extend_from_slice(&(payload.len() as u16).to_be_bytes());
        } else {
            frame.push(127);
            frame.extend_from_slice(&(payload.len() as u64).to_be_bytes());
        }
        frame.extend_from_slice(&payload);
        stream.write_all(&frame).expect("写假 WS 帧");
        stream.flush().expect("刷新假 WS 帧");
    }

    /// 模拟断线：关掉当前连接（客户端应走重连）。
    fn disconnect(&self) {
        let mut writer = self
            .writer
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(stream) = writer.take() {
            let _ = stream.shutdown(Shutdown::Both);
        }
    }

    /// 取一条记录（严格顺序；超时 panic）。
    fn next_record(&self, timeout: Duration) -> Recorded {
        self.records
            .recv_timeout(timeout)
            .unwrap_or_else(|error| panic!("等待飞书记录超时（{timeout:?}）：{error}"))
    }

    /// 在 `timeout` 内等待满足谓词的记录；不满足的记录丢弃并记录在报错里。
    fn wait_record(
        &self,
        timeout: Duration,
        mut predicate: impl FnMut(&Recorded) -> bool,
        what: &str,
    ) -> Recorded {
        let deadline = Instant::now() + timeout;
        let mut skipped: Vec<Recorded> = Vec::new();
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                panic!("等待 {what} 超时（{timeout:?}）；期间收到其它记录：{skipped:?}");
            }
            match self.records.recv_timeout(remaining) {
                Ok(record) => {
                    if predicate(&record) {
                        return record;
                    }
                    skipped.push(record);
                }
                Err(RecvTimeoutError::Timeout) => {
                    panic!("等待 {what} 超时（{timeout:?}）；期间收到其它记录：{skipped:?}");
                }
                Err(RecvTimeoutError::Disconnected) => {
                    panic!("等待 {what} 时记录通道意外断开");
                }
            }
        }
    }

    /// 断言在 `wait` 内没有任何新的飞书记录。
    fn assert_no_record(&self, wait: Duration) {
        match self.records.recv_timeout(wait) {
            Err(RecvTimeoutError::Timeout) => {}
            Ok(record) => panic!("不应再收到飞书记录，实际收到：{record:?}"),
            Err(RecvTimeoutError::Disconnected) => panic!("记录通道意外断开"),
        }
    }

    /// 等一条「发送到 `chat_id` 且文本等于 `text`」的提示。
    fn expect_sent(&self, chat_id: &str, text: &str, timeout: Duration) {
        let what = format!("发送到 {chat_id} 的提示 {text:?}");
        self.wait_record(
            timeout,
            |record| sent(record) == Some((chat_id, text)),
            &what,
        );
    }

    /// 等一条「发送到 `chat_id` 且文本包含 `needle`」的提示，返回整段文本。
    fn expect_sent_containing(&self, chat_id: &str, needle: &str, timeout: Duration) -> String {
        let what = format!("发送到 {chat_id} 且包含 {needle:?} 的提示");
        let record = self.wait_record(
            timeout,
            |record| {
                sent(record)
                    .map(|(id, text)| id == chat_id && text.contains(needle))
                    .unwrap_or(false)
            },
            &what,
        );
        match record {
            Recorded::Sent { text, .. } => text,
            Recorded::Replied { .. } => unreachable!("谓词只接受 Sent"),
        }
    }

    /// 等一条「回复 `message_id` 且文本等于 `text`」的答案。
    fn expect_replied(&self, message_id: &str, text: &str, timeout: Duration) {
        let what = format!("回复 {message_id} 的答案 {text:?}");
        self.wait_record(
            timeout,
            |record| replied(record) == Some((message_id, text)),
            &what,
        );
    }
}

impl Drop for MockFeishu {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        {
            let mut writer = self
                .writer
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if let Some(stream) = writer.take() {
                let _ = stream.shutdown(Shutdown::Both);
            }
        }
        if let Some(handle) = self.http_thread.take() {
            let _ = handle.join();
        }
        if let Some(handle) = self.ws_thread.take() {
            let _ = handle.join();
        }
    }
}

// ---------------------------------------------------------------------------
// feishu::run 线程句柄与测试脚手架
// ---------------------------------------------------------------------------

/// 一条跑在独立线程里的 `feishu::run`；Drop 时置停止标志并 join。
struct FeishuHandle {
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<i32>>,
}

impl FeishuHandle {
    /// 以给定 chat/网关起 `feishu::run`（gateway 同时注入 PromptCore，供
    /// `run_prompt` 查 Worker 快照，并用于 `/status` 的在线列表）。
    fn start(chat: TestChat, options: FeishuOptions) -> Self {
        let core = Arc::new(PromptCore::new(
            chat,
            policy(),
            agent_config(),
            TraceWriter::disabled(),
        ));
        let gateway = mock_gateway();
        core.set_gateway(Some(Arc::clone(&gateway)));
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let thread = thread::spawn(move || feishu::run(core, gateway, options, thread_stop));
        Self {
            stop,
            thread: Some(thread),
        }
    }

    /// 置停止标志并等待退出，返回 `feishu::run` 的退出码。
    fn stop(mut self) -> i32 {
        self.stop.store(true, Ordering::SeqCst);
        let handle = self.thread.take().expect("飞书线程已取出");
        handle.join().expect("飞书线程 panic")
    }
}

impl Drop for FeishuHandle {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.thread.take() {
            let _ = handle.join();
        }
    }
}

/// 基础运行参数：临时 HTTP 端口 + 默认 allowlist 空 + ack 开。
fn feishu_options(mock: &MockFeishu) -> FeishuOptions {
    FeishuOptions {
        app_id: String::from("cli_test"),
        app_secret: String::from("secret"),
        domain: mock.domain.clone(),
        allow_users: Vec::new(),
        allow_chats: Vec::new(),
        ack: true,
        max_steps: Some(4),
        context_turns: 8,
        link: LinkMode::Tcp,
        http_timeout: Duration::from_secs(3),
    }
}

/// 起一个「放行 ou_1」的 feishu 线程并等长连接就绪。
fn start_allowed(mock: &MockFeishu, chat: TestChat) -> FeishuHandle {
    let mut options = feishu_options(mock);
    options.allow_users = vec![String::from("ou_1")];
    let handle = FeishuHandle::start(chat, options);
    mock.wait_connected(1, Duration::from_secs(3));
    handle
}

/// 每次消息处理结束（回复 HTTP 已完成）后让出一点时间，等运行线程把
/// per-chat 在途标记移除，避免下一条消息被误判为「上一条还在处理中」。
fn wait_chat_idle() {
    thread::sleep(Duration::from_millis(20));
}

// ---------------------------------------------------------------------------
// 测试 1：空 allowlist → 未授权提示，每 chat 60s 只回一次
// ---------------------------------------------------------------------------

#[test]
fn unauthorized_message_denied_once_per_minute() {
    let mock = MockFeishu::start();
    let options = feishu_options(&mock); // allow_users / allow_chats 均为空
    let handle = FeishuHandle::start(TestChat::instant(), options);
    mock.wait_connected(1, Duration::from_secs(3));

    mock.push_event(text_event("ev-1", "p2p", "oc_1", "om_1", "你好"));
    mock.expect_sent("oc_1", DENIED_TEXT, Duration::from_secs(3));

    // 60s 内同 chat 再发（不同 event_id）不再重复回复。
    mock.push_event(text_event("ev-2", "p2p", "oc_1", "om_2", "再试一次"));
    mock.assert_no_record(Duration::from_millis(500));

    assert_eq!(handle.stop(), 0, "停止后应优雅退出（退出码 0）");
}

// ---------------------------------------------------------------------------
// 测试 2：放行的 p2p 消息 → 先 ack 提示，再回复答案
// ---------------------------------------------------------------------------

#[test]
fn allowed_p2p_message_answers() {
    let mock = MockFeishu::start();
    let handle = start_allowed(&mock, TestChat::instant());

    mock.push_event(text_event("ev-1", "p2p", "oc_1", "om_1", "你好"));

    match mock.next_record(Duration::from_secs(3)) {
        Recorded::Sent { chat_id, text } => {
            assert_eq!(chat_id, "oc_1");
            assert_eq!(text, ACK_TEXT);
        }
        other => panic!("第一条应为 ack 提示，实际：{other:?}"),
    }
    match mock.next_record(Duration::from_secs(3)) {
        Recorded::Replied { message_id, text } => {
            assert_eq!(message_id, "om_1");
            assert_eq!(text, ANSWER);
        }
        other => panic!("第二条应为答案回复，实际：{other:?}"),
    }

    drop(handle);
}

// ---------------------------------------------------------------------------
// 测试 3：群聊未 @ 机器人 → 完全忽略（连提示都不发）
// ---------------------------------------------------------------------------

#[test]
fn group_without_mention_ignored() {
    let mock = MockFeishu::start();
    let handle = start_allowed(&mock, TestChat::instant());

    mock.push_event(text_event("ev-1", "group", "oc_1", "om_1", "随便说说"));
    mock.assert_no_record(Duration::from_millis(500));

    drop(handle);
}

// ---------------------------------------------------------------------------
// 测试 4：非文本 / 空文本 / 超长文本各自的提示
// ---------------------------------------------------------------------------

#[test]
fn non_text_empty_and_too_long_hints() {
    let mock = MockFeishu::start();
    let handle = start_allowed(&mock, TestChat::instant());

    // 非文本消息（image）：V1 仅支持文本。
    mock.push_event(data_frame(
        "om-img",
        event_json("ev-img", "p2p", "oc_1", "om-img", "image", "", &[]),
        1,
        0,
    ));
    mock.expect_sent("oc_1", NON_TEXT_TEXT, Duration::from_secs(3));

    // 空白文本 → usage 提示。
    mock.push_event(text_event("ev-empty", "p2p", "oc_1", "om-empty", "   "));
    mock.expect_sent("oc_1", USAGE_TEXT, Duration::from_secs(3));

    // 8193 字节（> 上限 8192）→ 超限提示；同时覆盖 WS 16 位长度与 pbbp 大 payload。
    let long_text = "a".repeat(8_193);
    mock.push_event(text_event("ev-long", "p2p", "oc_1", "om-long", &long_text));
    mock.expect_sent("oc_1", TOO_LONG_TEXT, Duration::from_secs(3));

    drop(handle);
}

// ---------------------------------------------------------------------------
// 测试 5：同一 event_id 重投 → 只处理一次
// ---------------------------------------------------------------------------

#[test]
fn duplicate_event_id_processed_once() {
    let mock = MockFeishu::start();
    let handle = start_allowed(&mock, TestChat::instant());

    let frame = text_event("ev-dup", "p2p", "oc_1", "om-dup", "你好");
    mock.push_event(frame.clone());
    mock.expect_replied("om-dup", ANSWER, Duration::from_secs(3));
    wait_chat_idle();

    // 服务端 3s 未 ack 重投同事件：event_id 去重后静默丢弃。
    mock.push_event(frame);
    mock.assert_no_record(Duration::from_millis(500));

    drop(handle);
}

// ---------------------------------------------------------------------------
// 测试 6：pbbp 分片（sum=2、seq=0/1）→ 两帧各 ack 一次，重组后正常回复
// ---------------------------------------------------------------------------

#[test]
fn fragmented_event_reassembled_and_acked() {
    let mock = MockFeishu::start();
    let handle = start_allowed(&mock, TestChat::instant());

    let payload = event_json("ev-frag", "p2p", "oc_1", "om-frag", "text", "分片消息", &[]);
    let split = payload.len() / 2;
    mock.push_event(data_frame("om-frag", payload[..split].to_vec(), 2, 0));
    mock.push_event(data_frame("om-frag", payload[split..].to_vec(), 2, 1));

    // 每个分片先 ack（先 ack 再投递）；凑齐后才进 dispatch。
    mock.wait_ack(2, Duration::from_secs(3));
    mock.expect_replied("om-frag", ANSWER, Duration::from_secs(3));
    assert_eq!(
        mock.ack_frames(),
        2,
        "两条分片帧应各回一个 ack（不应多也不应少）"
    );

    drop(handle);
}

// ---------------------------------------------------------------------------
// 测试 7：同 chat 在途时新消息 → 忙拒
// ---------------------------------------------------------------------------

#[test]
fn busy_rejected_while_running() {
    let mock = MockFeishu::start();
    let handle = start_allowed(&mock, TestChat::slow());

    mock.push_event(text_event("ev-1", "p2p", "oc_1", "om-1", "第一条"));
    mock.expect_sent("oc_1", ACK_TEXT, Duration::from_secs(3));

    // 慢聊天仍在 400ms 睡眠中：第二条应立即得到忙拒提示。
    mock.push_event(text_event("ev-2", "p2p", "oc_1", "om-2", "第二条"));
    mock.expect_sent("oc_1", BUSY_CHAT_TEXT, Duration::from_secs(3));

    drop(handle);
}

// ---------------------------------------------------------------------------
// 测试 8：/cancel 空闲与在途两种回复
// ---------------------------------------------------------------------------

#[test]
fn cancel_idle_and_in_flight() {
    let mock = MockFeishu::start();
    let handle = start_allowed(&mock, TestChat::slow());

    // 1）没有运行中任务时的 /cancel。
    mock.push_event(text_event("ev-c1", "p2p", "oc_1", "om-c1", "/cancel"));
    mock.expect_sent("oc_1", CANCEL_IDLE_TEXT, Duration::from_secs(3));

    // 2）慢消息在途时的 /cancel。
    mock.push_event(text_event("ev-1", "p2p", "oc_1", "om-1", "慢任务"));
    mock.expect_sent("oc_1", ACK_TEXT, Duration::from_secs(3));
    mock.push_event(text_event("ev-c2", "p2p", "oc_1", "om-c2", "/cancel"));
    mock.expect_sent("oc_1", CANCEL_OK_TEXT, Duration::from_secs(3));

    drop(handle);
}

// ---------------------------------------------------------------------------
// 测试 9：/status 一行状态
// ---------------------------------------------------------------------------

#[test]
fn status_line() {
    let mock = MockFeishu::start();
    let handle = start_allowed(&mock, TestChat::instant());

    mock.push_event(text_event("ev-st", "p2p", "oc_1", "om-st", "/status"));
    let text = mock.expect_sent_containing("oc_1", "链路：tcp", Duration::from_secs(3));
    assert!(text.contains("链路：tcp"), "status 文本：{text}");
    assert!(
        text.contains("在线 Worker：1 个（w-1）"),
        "status 文本：{text}"
    );
    assert!(text.contains("全局运行："), "status 文本：{text}");
    assert!(text.contains("本会话：空闲"), "status 文本：{text}");

    drop(handle);
}

// ---------------------------------------------------------------------------
// 测试 10：60 msg/min 限流 → 第 61 条被限流
// ---------------------------------------------------------------------------

#[test]
fn rate_limited_after_sixty() {
    let mock = MockFeishu::start();
    let handle = start_allowed(&mock, TestChat::instant());

    // 串行发 60 条（每条等到回复再发下一条；每条 event_id/message_id 各异）。
    for index in 0..60 {
        let event_id = format!("ev-rl-{index}");
        let message_id = format!("om-rl-{index}");
        mock.push_event(text_event(&event_id, "p2p", "oc_1", &message_id, "计数"));
        mock.expect_replied(&message_id, ANSWER, Duration::from_secs(3));
        wait_chat_idle();
    }

    // 第 61 条：滑动窗口满 60 → 限流提示（不再进 Agent）。
    mock.push_event(text_event("ev-rl-60", "p2p", "oc_1", "om-rl-60", "计数"));
    mock.expect_sent("oc_1", RATE_TEXT, Duration::from_secs(3));

    drop(handle);
}

// ---------------------------------------------------------------------------
// 测试 11：断线重连后仍能收事件并回复
// ---------------------------------------------------------------------------

#[test]
fn reconnect_delivers_after_disconnect() {
    let mock = MockFeishu::start();
    let handle = start_allowed(&mock, TestChat::instant());

    mock.push_event(text_event("ev-1", "p2p", "oc_1", "om-1", "重连前"));
    mock.expect_replied("om-1", ANSWER, Duration::from_secs(3));
    wait_chat_idle();

    // 断线：客户端应重新调 endpoint（次数 +1）并完成第二次 WS 握手。
    mock.disconnect();
    mock.wait_connected(2, Duration::from_secs(5));
    assert!(
        mock.endpoint_calls() >= 2,
        "重连应重新获取连接端点（实际 {} 次）",
        mock.endpoint_calls()
    );

    mock.push_event(text_event("ev-2", "p2p", "oc_1", "om-2", "重连后"));
    mock.expect_replied("om-2", ANSWER, Duration::from_secs(3));

    drop(handle);
}

// ---------------------------------------------------------------------------
// 测试 12：ack=false 时只回答案、不发「正在处理」提示
// ---------------------------------------------------------------------------

#[test]
fn ack_can_be_disabled() {
    let mock = MockFeishu::start();
    let mut options = feishu_options(&mock);
    options.allow_users = vec![String::from("ou_1")];
    options.ack = false;
    let handle = FeishuHandle::start(TestChat::instant(), options);
    mock.wait_connected(1, Duration::from_secs(3));

    mock.push_event(text_event("ev-1", "p2p", "oc_1", "om-1", "你好"));
    match mock.next_record(Duration::from_secs(3)) {
        Recorded::Replied { message_id, text } => {
            assert_eq!(message_id, "om-1");
            assert_eq!(text, ANSWER);
        }
        other => panic!("ack=false 时首条应为答案回复，实际：{other:?}"),
    }
    mock.assert_no_record(Duration::from_millis(300));

    drop(handle);
}

// ---------------------------------------------------------------------------
// 测试 13：同会话多轮上下文（元数据 + 上一轮问答）
// ---------------------------------------------------------------------------

#[test]
fn same_chat_second_turn_sees_previous_turns_and_metadata() {
    let mock = MockFeishu::start();
    let chat = TestChat::instant();
    let log = chat.request_log();
    let handle = start_allowed(&mock, chat);

    mock.push_event(text_event("ev-1", "p2p", "oc_1", "om-1", "第一问"));
    mock.expect_replied("om-1", ANSWER, Duration::from_secs(3));
    wait_chat_idle();

    mock.push_event(text_event("ev-2", "p2p", "oc_1", "om-2", "第二问"));
    mock.expect_replied("om-2", ANSWER, Duration::from_secs(3));
    wait_chat_idle();

    let requests = log.lock().unwrap();
    assert_eq!(requests.len(), 2, "两轮各一次 LLM 请求");
    let first = &requests[0];
    assert_eq!(first.messages.len(), 2, "首轮无历史");
    let system = first.messages[0].content.as_deref().unwrap();
    for needle in [
        "当前会话（Router 注入）",
        "渠道：feishu",
        "会话 id：oc_1",
        "当前消息 id：om-1",
        "发送者 id：ou_1",
        "消息时间：1700000000000",
    ] {
        assert!(
            system.contains(needle),
            "首轮 system 缺少 {needle}：{system}"
        );
    }
    assert_eq!(first.messages[1].role, Role::User);

    let second = &requests[1];
    assert_eq!(
        second.messages.len(),
        4,
        "第二轮 = system + 上一轮 Q/A + 当前问题"
    );
    assert_eq!(second.messages[1].role, Role::User);
    assert_eq!(second.messages[1].content.as_deref(), Some("第一问"));
    assert_eq!(second.messages[2].role, Role::Assistant);
    assert_eq!(second.messages[2].content.as_deref(), Some(ANSWER));
    assert_eq!(second.messages[3].role, Role::User);
    assert_eq!(second.messages[3].content.as_deref(), Some("第二问"));
    let system = second.messages[0].content.as_deref().unwrap();
    assert!(system.contains("最近对话：1 轮"), "{system}");
    assert!(system.contains("当前消息 id：om-2"), "{system}");

    drop(requests);
    drop(handle);
}

// ---------------------------------------------------------------------------
// 测试 14：不同会话之间不串上下文
// ---------------------------------------------------------------------------

#[test]
fn context_is_isolated_between_chats() {
    let mock = MockFeishu::start();
    let chat = TestChat::instant();
    let log = chat.request_log();
    let handle = start_allowed(&mock, chat);

    mock.push_event(text_event("ev-1", "p2p", "oc_1", "om-1", "甲会话的问题"));
    mock.expect_replied("om-1", ANSWER, Duration::from_secs(3));
    wait_chat_idle();

    mock.push_event(text_event("ev-2", "p2p", "oc_2", "om-2", "乙会话的问题"));
    mock.expect_replied("om-2", ANSWER, Duration::from_secs(3));
    wait_chat_idle();

    let requests = log.lock().unwrap();
    assert_eq!(requests.len(), 2);
    let second = &requests[1];
    assert_eq!(second.messages.len(), 2, "不同 chat 不应携带对方历史");
    let system = second.messages[0].content.as_deref().unwrap();
    assert!(system.contains("会话 id：oc_2"), "{system}");
    assert!(!system.contains("oc_1"), "不应出现另一会话 id：{system}");
    assert!(
        second
            .messages
            .iter()
            .all(|message| message.content.as_deref() != Some("甲会话的问题")),
        "不应携带另一会话的历史"
    );

    drop(requests);
    drop(handle);
}

// ---------------------------------------------------------------------------
// 测试 15：context_turns=0 → 只有会话元数据，无多轮记忆
// ---------------------------------------------------------------------------

#[test]
fn context_turns_zero_keeps_metadata_only() {
    let mock = MockFeishu::start();
    let chat = TestChat::instant();
    let log = chat.request_log();
    let mut options = feishu_options(&mock);
    options.allow_users = vec![String::from("ou_1")];
    options.context_turns = 0;
    let handle = FeishuHandle::start(chat, options);
    mock.wait_connected(1, Duration::from_secs(3));

    mock.push_event(text_event("ev-1", "p2p", "oc_1", "om-1", "第一问"));
    mock.expect_replied("om-1", ANSWER, Duration::from_secs(3));
    wait_chat_idle();

    mock.push_event(text_event("ev-2", "p2p", "oc_1", "om-2", "第二问"));
    mock.expect_replied("om-2", ANSWER, Duration::from_secs(3));
    wait_chat_idle();

    let requests = log.lock().unwrap();
    assert_eq!(requests.len(), 2);
    let second = &requests[1];
    assert_eq!(second.messages.len(), 2, "关闭多轮记忆后不注入历史消息");
    let system = second.messages[0].content.as_deref().unwrap();
    assert!(system.contains("会话 id：oc_1"), "{system}");
    assert!(system.contains("当前消息 id：om-2"), "{system}");
    assert!(
        !system.contains("最近对话"),
        "0 轮不应渲染历史计数：{system}"
    );

    drop(requests);
    drop(handle);
}

// ---------------------------------------------------------------------------
// 真连冒烟（`#[ignore]`，CI 不跑；见 docs/sekai-feishu.md §10）
// ---------------------------------------------------------------------------

/// 用真实凭据验证「endpoint → WS 握手 → 长连接建立 → 干净退出」。
///
/// 运行方式（需真实企业自建应用且已发布）：
///
/// ```text
/// $env:SEKAI_REAL_FEISHU=1
/// $env:SEKAI_FEISHU_APP_ID="cli_..."
/// $env:SEKAI_FEISHU_APP_SECRET="..."
/// cargo test -p sekai-router --test feishu_mock -- --ignored real_feishu_connect_smoke
/// ```
///
/// 只验证链路（不发消息、不调 LLM）：3s 内 `run` 不返回 3/4 即视为连上；
/// 置停止标志后应返回 0。
#[test]
#[ignore = "真连冒烟：需 SEKAI_REAL_FEISHU=1 与真实 SEKAI_FEISHU_APP_ID/_APP_SECRET"]
fn real_feishu_connect_smoke() {
    if std::env::var("SEKAI_REAL_FEISHU").as_deref() != Ok("1") {
        eprintln!("跳过：未设置 SEKAI_REAL_FEISHU=1");
        return;
    }
    let app_id = std::env::var("SEKAI_FEISHU_APP_ID").expect("缺少 SEKAI_FEISHU_APP_ID");
    let app_secret =
        std::env::var("SEKAI_FEISHU_APP_SECRET").expect("缺少 SEKAI_FEISHU_APP_SECRET");
    let domain = std::env::var("SEKAI_FEISHU_DOMAIN")
        .unwrap_or_else(|_| String::from("https://open.feishu.cn"));
    let options = FeishuOptions {
        app_id,
        app_secret,
        domain,
        allow_users: Vec::new(),
        allow_chats: Vec::new(),
        ack: true,
        max_steps: None,
        context_turns: 8,
        link: LinkMode::Tcp,
        http_timeout: Duration::from_secs(10),
    };
    let handle = FeishuHandle::start(TestChat::instant(), options);
    // 真连握手与 endpoint 往返需要时间；提前返回（3/4）说明链路没建立。
    thread::sleep(Duration::from_secs(3));
    let code = handle.stop();
    assert_eq!(code, 0, "真连长连接未建立（退出码 {code}）");
}
