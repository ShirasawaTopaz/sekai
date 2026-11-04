//! 零依赖 HTTP 客户端。
//!
//! # 为什么不是 ureq
//!
//! 本 crate 的目标是能在任意 host 环境（包括受 Windows 沙箱限制、无法由 cargo
//! 启动 rustc 的调试机器）里做真实构建与真实 API 测试，因此不引入第三方依赖。
//! HTTP 客户端分两条路径：
//!
//! - http://：进程内实现 HTTP/1.1（StdHttpClient），用于本地 mock server 测试，
//!   不依赖外部程序；
//! - https://：把请求交给外部 helper（优先 Node.js，其次 OpenSSL 版 curl），
//!   通过临时文件而不是管道交换请求/响应体。helper 进程使用环境变量接收
//!   Authorization，密钥不落盘、不出现在命令行参数中。
//!
//! 该设计只用于 host 侧工具；协议/Worker侧代码不依赖本模块。

use std::fs;
use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crate::json::Json;

/// 环境变量：覆盖 Node.js 可执行文件路径。
pub const ENV_NODE: &str = "SEKAI_NODE";
/// 环境变量：覆盖 curl 可执行文件路径。
pub const ENV_CURL: &str = "SEKAI_CURL";

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// HTTP 请求。
#[derive(Debug, Clone)]
pub struct HttpRequest {
    /// HTTP 方法（如 POST）。
    pub method: String,
    /// 完整 URL（http:// 或 https://）。
    pub url: String,
    /// 头部列表；顺序保持。
    pub headers: Vec<(String, String)>,
    /// 请求体。
    pub body: Vec<u8>,
    /// 端到端超时。
    pub timeout: Duration,
}

impl HttpRequest {
    /// 构造请求；默认方法为 POST 且空 body。
    pub fn new(method: impl Into<String>, url: impl Into<String>) -> Self {
        Self {
            method: method.into(),
            url: url.into(),
            headers: Vec::new(),
            body: Vec::new(),
            timeout: Duration::from_secs(60),
        }
    }

    /// 追加一个请求头。
    pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }

    /// 设置 JSON 请求体。
    pub fn json_body(mut self, value: &Json) -> Self {
        self.body = value.to_compact_string().into_bytes();
        self.headers.push((
            String::from("Content-Type"),
            String::from("application/json"),
        ));
        self
    }

    /// 设置超时。
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }
}

/// HTTP 响应。
#[derive(Debug, Clone)]
pub struct HttpResponse {
    /// 状态码。
    pub status: u16,
    /// 响应头（键统一小写）。
    pub headers: Vec<(String, String)>,
    /// 响应体。
    pub body: Vec<u8>,
}

impl HttpResponse {
    /// 把响应体按 UTF-8（有损）解析为文本。
    pub fn body_text(&self) -> String {
        String::from_utf8_lossy(&self.body).to_string()
    }

    /// 读取某个响应头（大小写不敏感）。
    pub fn header(&self, name: &str) -> Option<&str> {
        let name = name.to_ascii_lowercase();
        self.headers
            .iter()
            .find(|(key, _)| *key == name)
            .map(|(_, value)| value.as_str())
    }
}

/// HTTP 错误。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HttpError {
    /// URL 非法。
    InvalidUrl(String),
    /// TCP 连接失败。
    Connect(String),
    /// 读写错误。
    Io(String),
    /// 超时。
    Timeout,
    /// 响应不符合 HTTP/1.1。
    Protocol(String),
    /// 外部 helper 执行失败。
    Helper(String),
    /// 需要 HTTPS helper 但当前环境找不到。
    NoHttpsHelper,
}

impl std::fmt::Display for HttpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidUrl(message) => write!(f, "URL 非法：{message}"),
            Self::Connect(message) => write!(f, "连接失败：{message}"),
            Self::Io(message) => write!(f, "I/O 错误：{message}"),
            Self::Timeout => write!(f, "HTTP 请求超时"),
            Self::Protocol(message) => write!(f, "HTTP 响应非法：{message}"),
            Self::Helper(message) => write!(f, "HTTPS helper 失败：{message}"),
            Self::NoHttpsHelper => write!(
                f,
                "当前环境没有可用的 HTTPS helper（需要 node 或 OpenSSL 版 curl；可用 {ENV_NODE}/{ENV_CURL} 指定路径）"
            ),
        }
    }
}

impl std::error::Error for HttpError {}

/// HTTP 传输抽象；测试中可注入 mock 实现。
///
/// `Send` 约束让 `Arc<Mutex<Box<dyn HttpClient>>>`（可克隆客户端）跨线程安全。
pub trait HttpClient: Send {
    /// 执行一次请求。
    fn execute(&mut self, request: &HttpRequest) -> Result<HttpResponse, HttpError>;
}

/// 外部 helper 选择策略。
#[derive(Debug, Clone, Default)]
pub enum HttpsHelper {
    /// 自动探测 Node.js，其次 curl。
    #[default]
    Auto,
    /// 强制使用 Node.js。
    Node,
    /// 强制使用指定 curl。
    Curl(PathBuf),
    /// 禁用 HTTPS（测试 mock 用）。
    Disabled,
}

/// 默认 HTTP 客户端：明文直连 + HTTPS 外部 helper。
pub struct StdHttpClient {
    helper: HttpsHelper,
    node_path: Option<PathBuf>,
    curl_path: Option<PathBuf>,
    helper_dir: Option<PathBuf>,
}

impl StdHttpClient {
    /// 创建自动探测 helper 的客户端。
    pub fn new() -> Self {
        Self::with_helper(HttpsHelper::Auto)
    }

    /// 用指定 helper 策略创建客户端。
    pub fn with_helper(helper: HttpsHelper) -> Self {
        Self {
            helper,
            node_path: resolve_node(),
            curl_path: resolve_curl(),
            helper_dir: None,
        }
    }

    /// 当前可用的 HTTPS helper 名称；没有则 None。
    pub fn https_helper_name(&self) -> Option<&'static str> {
        match &self.helper {
            HttpsHelper::Disabled => None,
            HttpsHelper::Node => self.node_path.as_ref().map(|_| "node"),
            HttpsHelper::Curl(_) => Some("curl"),
            HttpsHelper::Auto => {
                if self.node_path.is_some() {
                    Some("node")
                } else if self.curl_path.is_some() {
                    Some("curl")
                } else {
                    None
                }
            }
        }
    }

    fn execute_https(&mut self, request: &HttpRequest) -> Result<HttpResponse, HttpError> {
        match &self.helper {
            HttpsHelper::Disabled => Err(HttpError::NoHttpsHelper),
            HttpsHelper::Curl(path) => self.execute_with_curl(path.clone(), request),
            HttpsHelper::Node => match self.node_path.clone() {
                Some(path) => self.execute_with_node(path, request),
                None => Err(HttpError::NoHttpsHelper),
            },
            HttpsHelper::Auto => {
                if let Some(path) = self.node_path.clone() {
                    self.execute_with_node(path, request)
                } else if let Some(path) = self.curl_path.clone() {
                    self.execute_with_curl(path, request)
                } else {
                    Err(HttpError::NoHttpsHelper)
                }
            }
        }
    }

    fn execute_with_node(
        &mut self,
        node: PathBuf,
        request: &HttpRequest,
    ) -> Result<HttpResponse, HttpError> {
        let dir = self.temp_dir()?;
        let stem = unique_stem();
        let body_path = dir.join(format!("{stem}-req.bin"));
        let resp_path = dir.join(format!("{stem}-resp.bin"));
        let meta_path = dir.join(format!("{stem}-meta.json"));
        let stderr_path = dir.join(format!("{stem}-stderr.txt"));
        let script_path = dir.join("sekai-http-helper.js");
        fs::write(&script_path, NODE_HELPER_JS)
            .map_err(|error| HttpError::Helper(format!("写 helper 脚本失败：{error}")))?;
        fs::write(&body_path, &request.body)
            .map_err(|error| HttpError::Helper(format!("写请求体失败：{error}")))?;
        let headers = Json::Object(
            request
                .headers
                .iter()
                .map(|(name, value)| (name.clone(), Json::String(value.clone())))
                .collect(),
        );
        let stderr = fs::File::create(&stderr_path)
            .map_err(|error| HttpError::Helper(format!("创建 stderr 文件失败：{error}")))?;
        let mut child = Command::new(&node)
            .arg(&script_path)
            .arg(&body_path)
            .arg(&resp_path)
            .arg(&meta_path)
            .env("SEKAI_HTTP_URL", &request.url)
            .env("SEKAI_HTTP_METHOD", &request.method)
            .env("SEKAI_HTTP_HEADERS", headers.to_compact_string())
            .env(
                "SEKAI_HTTP_TIMEOUT_MS",
                request.timeout.as_millis().to_string(),
            )
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(stderr))
            .spawn()
            .map_err(|error| {
                HttpError::Helper(format!("启动 node 失败（{}）：{error}", node.display()))
            })?;
        wait_child(&mut child, request.timeout + Duration::from_secs(5))?;
        let result = read_node_meta(&meta_path, &resp_path, &stderr_path);
        let _ = fs::remove_file(&body_path);
        let _ = fs::remove_file(&resp_path);
        let _ = fs::remove_file(&meta_path);
        let _ = fs::remove_file(&stderr_path);
        result
    }

    fn execute_with_curl(
        &mut self,
        curl: PathBuf,
        request: &HttpRequest,
    ) -> Result<HttpResponse, HttpError> {
        let dir = self.temp_dir()?;
        let stem = unique_stem();
        let body_path = dir.join(format!("{stem}-req.bin"));
        let resp_path = dir.join(format!("{stem}-resp.bin"));
        let status_path = dir.join(format!("{stem}-status.txt"));
        let stderr_path = dir.join(format!("{stem}-stderr.txt"));
        fs::write(&body_path, &request.body)
            .map_err(|error| HttpError::Helper(format!("写请求体失败：{error}")))?;
        let body_spec = format!("@{}", body_path.display());
        let mut command = Command::new(&curl);
        command
            .arg("--silent")
            .arg("--show-error")
            .arg("--max-time")
            .arg(request.timeout.as_secs().max(1).to_string())
            .arg("--request")
            .arg(&request.method)
            .arg("--output")
            .arg(&resp_path)
            .arg("--write-out")
            .arg("%{http_code}");
        for (name, value) in &request.headers {
            command.arg("--header").arg(format!("{name}: {value}"));
        }
        command
            .arg("--data-binary")
            .arg(body_spec)
            .arg(&request.url)
            .stdin(Stdio::null())
            .stdout(Stdio::from(fs::File::create(&status_path).map_err(
                |error| HttpError::Helper(format!("创建状态文件失败：{error}")),
            )?))
            .stderr(Stdio::from(fs::File::create(&stderr_path).map_err(
                |error| HttpError::Helper(format!("创建 stderr 文件失败：{error}")),
            )?));
        let mut child = command.spawn().map_err(|error| {
            HttpError::Helper(format!("启动 curl 失败（{}）：{error}", curl.display()))
        })?;
        wait_child(&mut child, request.timeout + Duration::from_secs(5))?;
        let result = (|| {
            let status_text = fs::read_to_string(&status_path).unwrap_or_default();
            let status = status_text.trim().parse::<u16>().map_err(|_| {
                let stderr = fs::read_to_string(&stderr_path).unwrap_or_default();
                HttpError::Helper(format!("curl 未返回状态码：{stderr}"))
            })?;
            let body = fs::read(&resp_path).unwrap_or_default();
            Ok(HttpResponse {
                status,
                headers: Vec::new(),
                body,
            })
        })();
        let _ = fs::remove_file(&body_path);
        let _ = fs::remove_file(&resp_path);
        let _ = fs::remove_file(&status_path);
        let _ = fs::remove_file(&stderr_path);
        result
    }

    fn temp_dir(&mut self) -> Result<PathBuf, HttpError> {
        if let Some(dir) = &self.helper_dir {
            return Ok(dir.clone());
        }
        let mut dir = std::env::temp_dir();
        dir.push(format!(
            "sekai-router-http-{}-{}",
            std::process::id(),
            unique_stem()
        ));
        fs::create_dir_all(&dir)
            .map_err(|error| HttpError::Helper(format!("创建临时目录失败：{error}")))?;
        self.helper_dir = Some(dir.clone());
        Ok(dir)
    }
}

impl Default for StdHttpClient {
    fn default() -> Self {
        Self::new()
    }
}

impl HttpClient for StdHttpClient {
    fn execute(&mut self, request: &HttpRequest) -> Result<HttpResponse, HttpError> {
        if request.url.starts_with("http://") {
            execute_plain(request)
        } else if request.url.starts_with("https://") {
            self.execute_https(request)
        } else {
            Err(HttpError::InvalidUrl(format!(
                "只支持 http/https：{}",
                request.url
            )))
        }
    }
}

// ---------------------------------------------------------------------------
// 明文 HTTP/1.1
// ---------------------------------------------------------------------------

fn execute_plain(request: &HttpRequest) -> Result<HttpResponse, HttpError> {
    let parsed = ParsedHttpUrl::parse(&request.url)?;
    let address = format!("{}:{}", parsed.host, parsed.port);
    let mut addresses = address
        .to_socket_addrs()
        .map_err(|error| HttpError::Connect(error.to_string()))?;
    let socket = addresses
        .next()
        .ok_or_else(|| HttpError::Connect(String::from("没有可用地址")))?;
    let started = Instant::now();
    let stream = TcpStream::connect_timeout(&socket, request.timeout)
        .map_err(|error| HttpError::Connect(error.to_string()))?;
    let _ = stream.set_nodelay(true);
    let remaining = request
        .timeout
        .saturating_sub(Instant::now().saturating_duration_since(started));
    let _ = stream.set_read_timeout(Some(remaining.max(Duration::from_millis(1))));
    let _ = stream.set_write_timeout(Some(remaining.max(Duration::from_millis(1))));
    let mut stream = stream;
    let mut head = format!(
        "{} {} HTTP/1.1\r\nHost: {}:{}\r\n",
        request.method, parsed.path, parsed.host, parsed.port
    );
    for (name, value) in &request.headers {
        head.push_str(name);
        head.push_str(": ");
        head.push_str(value);
        head.push_str("\r\n");
    }
    head.push_str(&format!(
        "Content-Length: {}\r\nConnection: close\r\n\r\n",
        request.body.len()
    ));
    stream
        .write_all(head.as_bytes())
        .map_err(|error| HttpError::Io(error.to_string()))?;
    if !request.body.is_empty() {
        stream
            .write_all(&request.body)
            .map_err(|error| HttpError::Io(error.to_string()))?;
    }
    let mut raw = Vec::new();
    let mut buf = [0u8; 8192];
    loop {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(read) => raw.extend_from_slice(&buf[..read]),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                return Err(HttpError::Timeout);
            }
            Err(error) => return Err(HttpError::Io(error.to_string())),
        }
    }
    parse_response(&raw)
}

struct ParsedHttpUrl {
    host: String,
    port: u16,
    path: String,
}

impl ParsedHttpUrl {
    fn parse(url: &str) -> Result<Self, HttpError> {
        let rest = url
            .strip_prefix("http://")
            .ok_or_else(|| HttpError::InvalidUrl(String::from("不是 http:// URL")))?;
        let (authority, path) = match rest.find('/') {
            Some(index) => (&rest[..index], &rest[index..]),
            None => (rest, "/"),
        };
        if authority.is_empty() || authority.contains('@') {
            return Err(HttpError::InvalidUrl(String::from("authority 非法")));
        }
        let (host, port) = match authority.rsplit_once(':') {
            Some((host, port)) => (
                host.to_string(),
                port.parse::<u16>()
                    .map_err(|_| HttpError::InvalidUrl(String::from("端口非法")))?,
            ),
            None => (authority.to_string(), 80),
        };
        if host.is_empty() || port == 0 {
            return Err(HttpError::InvalidUrl(String::from("host/port 非法")));
        }
        Ok(Self {
            host,
            port,
            path: path.to_string(),
        })
    }
}

fn parse_response(raw: &[u8]) -> Result<HttpResponse, HttpError> {
    let header_end = raw
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .ok_or_else(|| HttpError::Protocol(String::from("缺少响应头结束标记")))?;
    let head = std::str::from_utf8(&raw[..header_end])
        .map_err(|_| HttpError::Protocol(String::from("响应头不是合法 UTF-8")))?;
    let mut lines = head.split("\r\n");
    let status_line = lines
        .next()
        .ok_or_else(|| HttpError::Protocol(String::from("缺少状态行")))?;
    let mut status_parts = status_line.split_whitespace();
    let version = status_parts.next().unwrap_or_default();
    if !version.starts_with("HTTP/1.") {
        return Err(HttpError::Protocol(format!("不支持的版本：{version}")));
    }
    let status: u16 = status_parts
        .next()
        .and_then(|value| value.parse().ok())
        .ok_or_else(|| HttpError::Protocol(String::from("状态码非法")))?;
    let mut headers = Vec::new();
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            headers.push((name.trim().to_ascii_lowercase(), value.trim().to_string()));
        }
    }
    let mut body = raw[header_end + 4..].to_vec();
    let chunked = headers
        .iter()
        .any(|(name, value)| name == "transfer-encoding" && value.contains("chunked"));
    if chunked {
        body = decode_chunked(&body)?;
    } else if let Some((_, length)) = headers.iter().find(|(name, _)| name == "content-length")
        && let Ok(length) = length.parse::<usize>()
    {
        body.truncate(length);
    }
    Ok(HttpResponse {
        status,
        headers,
        body,
    })
}

fn decode_chunked(raw: &[u8]) -> Result<Vec<u8>, HttpError> {
    let mut out = Vec::new();
    let mut rest = raw;
    loop {
        let line_end = rest
            .windows(2)
            .position(|window| window == b"\r\n")
            .ok_or_else(|| HttpError::Protocol(String::from("chunked 长度行缺失")))?;
        let line = std::str::from_utf8(&rest[..line_end])
            .map_err(|_| HttpError::Protocol(String::from("chunked 长度行非法")))?;
        let size_text = line.split(';').next().unwrap_or_default().trim();
        let size = usize::from_str_radix(size_text, 16)
            .map_err(|_| HttpError::Protocol(String::from("chunked 长度非法")))?;
        rest = &rest[line_end + 2..];
        if size == 0 {
            return Ok(out);
        }
        if rest.len() < size + 2 {
            return Err(HttpError::Protocol(String::from("chunked 数据不完整")));
        }
        out.extend_from_slice(&rest[..size]);
        rest = &rest[size + 2..];
    }
}

// ---------------------------------------------------------------------------
// helper 进程
// ---------------------------------------------------------------------------

const NODE_HELPER_JS: &str = r#"
const fs = require('fs');
const https = require('https');
const http = require('http');
const [bodyFile, respFile, metaFile] = process.argv.slice(2);
function writeMeta(value) { fs.writeFileSync(metaFile, JSON.stringify(value)); }
try {
  const url = process.env.SEKAI_HTTP_URL;
  const method = (process.env.SEKAI_HTTP_METHOD || 'POST').toUpperCase();
  const headers = JSON.parse(process.env.SEKAI_HTTP_HEADERS || '{}');
  const timeout = Number(process.env.SEKAI_HTTP_TIMEOUT_MS || 60000);
  const body = fs.readFileSync(bodyFile);
  const isHttps = url.startsWith('https://');
  const mod = isHttps ? https : http;
  const req = mod.request(url, { method, headers }, (res) => {
    const chunks = [];
    res.on('data', (chunk) => chunks.push(chunk));
    res.on('end', () => {
      fs.writeFileSync(respFile, Buffer.concat(chunks));
      writeMeta({ status: res.statusCode, error: null });
    });
    res.on('error', (error) => writeMeta({ status: 0, error: String(error) }));
  });
  req.on('error', (error) => writeMeta({ status: 0, error: String(error) }));
  req.setTimeout(timeout, () => { req.destroy(new Error('timeout')); });
  if (body.length > 0) { req.write(body); }
  req.end();
} catch (error) {
  writeMeta({ status: 0, error: String(error) });
}
"#;

fn read_node_meta(
    meta_path: &Path,
    resp_path: &Path,
    stderr_path: &Path,
) -> Result<HttpResponse, HttpError> {
    let meta_text = fs::read_to_string(meta_path).map_err(|error| {
        let stderr = fs::read_to_string(stderr_path).unwrap_or_default();
        HttpError::Helper(format!(
            "读取 node helper 结果失败：{error}；stderr={stderr}"
        ))
    })?;
    let meta = Json::parse(&meta_text)
        .map_err(|error| HttpError::Helper(format!("node helper 返回非法 JSON：{error}")))?;
    if let Some(message) = meta.get("error").and_then(Json::as_str) {
        return Err(HttpError::Helper(String::from(message)));
    }
    let status = meta
        .get("status")
        .and_then(Json::as_u64)
        .ok_or_else(|| HttpError::Helper(String::from("node helper 未返回状态码")))?;
    let body = fs::read(resp_path).unwrap_or_default();
    Ok(HttpResponse {
        status: status as u16,
        headers: Vec::new(),
        body,
    })
}

fn wait_child(child: &mut Child, timeout: Duration) -> Result<(), HttpError> {
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if status.success() {
                    return Ok(());
                }
                return Err(HttpError::Helper(format!("helper 退出码 {status}")));
            }
            Ok(None) => {
                if started.elapsed() >= timeout {
                    let _ = child.kill();
                    return Err(HttpError::Timeout);
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(error) => return Err(HttpError::Helper(format!("等待 helper 失败：{error}"))),
        }
    }
}

fn unique_stem() -> String {
    let counter = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{}-{}", std::process::id(), counter)
}

fn resolve_node() -> Option<PathBuf> {
    if let Ok(value) = std::env::var(ENV_NODE)
        && !value.trim().is_empty()
    {
        let path = PathBuf::from(value);
        if path.is_file() {
            return Some(path);
        }
    }
    let mut candidates: Vec<PathBuf> = Vec::new();
    candidates.push(PathBuf::from("node"));
    if cfg!(windows) {
        candidates.push(PathBuf::from(r"C:\Program Files\nodejs\node.exe"));
        candidates.push(PathBuf::from(r"C:\Program Files (x86)\nodejs\node.exe"));
    } else {
        candidates.push(PathBuf::from("/usr/bin/node"));
        candidates.push(PathBuf::from("/usr/local/bin/node"));
    }
    for candidate in candidates {
        if candidate.is_absolute() {
            if candidate.is_file() {
                return Some(candidate);
            }
        } else if command_exists(&candidate) {
            return Some(candidate);
        }
    }
    None
}

fn resolve_curl() -> Option<PathBuf> {
    if let Ok(value) = std::env::var(ENV_CURL)
        && !value.trim().is_empty()
    {
        let path = PathBuf::from(value);
        if path.is_file() {
            return Some(path);
        }
    }
    let mut candidates: Vec<PathBuf> = Vec::new();
    if cfg!(windows) {
        candidates.push(PathBuf::from(r"C:\Program Files\Git\mingw64\bin\curl.exe"));
        candidates.push(PathBuf::from(r"C:\msys64\mingw64\bin\curl.exe"));
        candidates.push(PathBuf::from(r"C:\msys64\usr\bin\curl.exe"));
    } else {
        candidates.push(PathBuf::from("/usr/bin/curl"));
        candidates.push(PathBuf::from("/usr/local/bin/curl"));
    }
    for candidate in candidates {
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    if command_exists(Path::new("curl")) {
        return Some(PathBuf::from("curl"));
    }
    None
}

fn command_exists(name: &Path) -> bool {
    if name.components().count() > 1 {
        return name.is_file();
    }
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    let exts: Vec<String> = if cfg!(windows) {
        vec![
            String::new(),
            String::from(".exe"),
            String::from(".cmd"),
            String::from(".bat"),
        ]
    } else {
        vec![String::new()]
    };
    for dir in std::env::split_paths(&path) {
        for ext in &exts {
            let candidate = dir.join(format!("{}{ext}", name.display()));
            if candidate.is_file() {
                return true;
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    fn mock_server(
        response: &'static [u8],
        expected_body: &'static str,
    ) -> (String, std::thread::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buffer = [0u8; 4096];
            let mut request = Vec::new();
            loop {
                let read = stream.read(&mut buffer).unwrap();
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..read]);
                if let Some(index) = find_header_end(&request) {
                    let head = String::from_utf8_lossy(&request[..index]).to_string();
                    let body_start = index + 4;
                    let content_length = head
                        .lines()
                        .find_map(|line| {
                            line.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .map(|value| value.trim().parse::<usize>().unwrap())
                        })
                        .unwrap_or(0);
                    if request.len() >= body_start + content_length {
                        let body = String::from_utf8_lossy(
                            &request[body_start..body_start + content_length],
                        )
                        .to_string();
                        assert_eq!(body, expected_body);
                        break;
                    }
                }
            }
            stream.write_all(response).unwrap();
            stream.flush().unwrap();
            String::from_utf8_lossy(&request).to_string()
        });
        (format!("http://127.0.0.1:{}", address.port()), handle)
    }

    fn find_header_end(bytes: &[u8]) -> Option<usize> {
        bytes.windows(4).position(|window| window == b"\r\n\r\n")
    }

    #[test]
    fn plain_post_roundtrip() {
        let response =
            b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 8\r\n\r\n{\"ok\":1}";
        let (url, handle) = mock_server(response, "{\"a\":1}");
        let mut client = StdHttpClient::with_helper(HttpsHelper::Disabled);
        let mut request = HttpRequest::new("POST", format!("{url}/v1/chat"));
        request.body = b"{\"a\":1}".to_vec();
        request.headers.push((
            String::from("Content-Type"),
            String::from("application/json"),
        ));
        let response = client.execute(&request).unwrap();
        assert_eq!(response.status, 200);
        assert_eq!(response.header("content-type"), Some("application/json"));
        assert_eq!(response.body_text(), "{\"ok\":1}");
        let request_text = handle.join().unwrap();
        assert!(request_text.starts_with("POST /v1/chat HTTP/1.1"));
        assert!(request_text.contains("Content-Type: application/json"));
    }

    #[test]
    fn plain_chunked_response() {
        let response = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n3\r\n ok\r\n0\r\n\r\n";
        let (url, handle) = mock_server(response, "");
        let mut client = StdHttpClient::with_helper(HttpsHelper::Disabled);
        let request = HttpRequest::new("POST", url);
        let response = client.execute(&request).unwrap();
        assert_eq!(response.body_text(), "hello ok");
        handle.join().unwrap();
    }

    #[test]
    fn https_requires_helper() {
        let mut client = StdHttpClient::with_helper(HttpsHelper::Disabled);
        let request = HttpRequest::new("POST", "https://example.com/x");
        assert!(matches!(
            client.execute(&request),
            Err(HttpError::NoHttpsHelper)
        ));
        assert_eq!(client.https_helper_name(), None);
    }

    #[test]
    fn invalid_urls_are_rejected() {
        let mut client = StdHttpClient::with_helper(HttpsHelper::Disabled);
        let request = HttpRequest::new("GET", "ftp://example.com/x");
        assert!(matches!(
            client.execute(&request),
            Err(HttpError::InvalidUrl(_))
        ));
    }
}
