//! prompt 通道（TCP ingress）集成测试：临时端口（127.0.0.1:0）起服务，
//! TCP 客户端发 ping / prompt / 畸形行，断言完整响应 JSON。
//!
//! 全部离线：chat 客户端与 Worker 网关均为进程内 mock。

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Shutdown, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use sekai_router::agent::AgentConfig;
use sekai_router::control::CommandRequest;
use sekai_router::gateway::{CommandReceipt, GatewayError, GatewayHandle, WorkerGateway};
use sekai_router::json::Json;
use sekai_router::llm::{ChatClient, ChatRequest, ChatResponse, LlmError, Usage};
use sekai_router::prompt_server::{self, PromptCore};
use sekai_router::tools::Policy;
use sekai_router::trace::TraceWriter;
use sekai_router::worker::WorkerInfo;

// ---- 测试替身 ----

/// 顺序返回预设响应的 chat 客户端。
#[derive(Clone)]
struct ScriptedChat {
    responses: Arc<Mutex<VecDeque<Result<ChatResponse, LlmError>>>>,
    requests: Arc<Mutex<Vec<ChatRequest>>>,
}

impl ScriptedChat {
    fn one(response: ChatResponse) -> Self {
        Self {
            responses: Arc::new(Mutex::new(vec![Ok(response)].into())),
            requests: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

impl ChatClient for ScriptedChat {
    fn chat(&mut self, request: &ChatRequest) -> Result<ChatResponse, LlmError> {
        self.requests.lock().unwrap().push(request.clone());
        self.responses
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| Err(LlmError::Protocol(String::from("scripted chat 已耗尽"))))
    }
}

/// 可注入 Worker 快照的内存网关。
struct MockGateway {
    workers: Arc<Mutex<Vec<WorkerInfo>>>,
}

impl WorkerGateway for MockGateway {
    fn list_workers(&mut self) -> Vec<WorkerInfo> {
        self.workers.lock().unwrap().clone()
    }

    fn submit(&mut self, _request: CommandRequest) -> Result<CommandReceipt, GatewayError> {
        let (sender, receiver) = std::sync::mpsc::channel();
        let _ = sender.send(Err(GatewayError::new(
            "worker_offline",
            "测试网关不支持 call",
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

// ---- 构造辅助 ----

fn worker(worker_id: &str) -> WorkerInfo {
    WorkerInfo {
        worker_id: String::from(worker_id),
        online: true,
        boot_id: 5,
        firmware: String::from("0.1.0"),
        uptime_ms: 10,
        unix_ms: 1_700_000_000_000,
        tools: vec![String::from("gpio.info"), String::from("pc.ask")],
        tasks: vec![String::from("echo")],
        workers: Vec::new(),
        active_tasks: Vec::new(),
        last_seen_mono_ms: 1,
        hello_seq: 1,
    }
}

fn text_response(text: &str) -> ChatResponse {
    ChatResponse {
        model: String::from("deepseek-flash"),
        content: Some(String::from(text)),
        reasoning_content: None,
        tool_calls: Vec::new(),
        finish_reason: Some(String::from("stop")),
        usage: Usage {
            prompt_tokens: 21,
            completion_tokens: 6,
            total_tokens: 27,
        },
    }
}

fn agent_config() -> AgentConfig {
    AgentConfig {
        model: String::from("deepseek-flash"),
        max_steps: Some(4),
        ddl_ms: 1_000,
        timeout_ms: None,
        temperature: 0.0,
        max_tokens: 4_096,
    }
}

/// 建一个带链路（含 1 个 Worker）的 hub + ingress（127.0.0.1:0 临时端口）。
fn start_server_with_link(chat: ScriptedChat) -> std::net::SocketAddr {
    let workers = Arc::new(Mutex::new(vec![worker("pc-1")]));
    start_server(chat, Some(workers))
}

/// 建 hub + ingress；workers None 表示不挂网关（未连接态）。
fn start_server(
    chat: ScriptedChat,
    workers: Option<Arc<Mutex<Vec<WorkerInfo>>>>,
) -> std::net::SocketAddr {
    let core = PromptCore::new(
        chat,
        Policy::default(),
        agent_config(),
        TraceWriter::disabled(),
    );
    if let Some(workers) = workers {
        let handle: GatewayHandle = Arc::new(Mutex::new(
            Box::new(MockGateway { workers }) as Box<dyn WorkerGateway>
        ));
        core.set_gateway(Some(handle));
    }
    let core = Arc::new(core);
    prompt_server::start_ingress(core, "127.0.0.1:0").expect("ingress 应绑定临时端口")
}

/// 连接、发送若干行（半关写端），读到 EOF 并解析每行为 JSON。
fn send_lines(addr: std::net::SocketAddr, lines: &[&str]) -> Vec<Json> {
    let mut stream = TcpStream::connect(addr).expect("应能连接 ingress");
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("设置读超时");
    for line in lines {
        stream
            .write_all(format!("{line}\n").as_bytes())
            .expect("写请求行");
    }
    stream.flush().expect("flush");
    stream.shutdown(Shutdown::Write).expect("半关写端");
    let mut raw = String::new();
    BufReader::new(stream)
        .read_to_string(&mut raw)
        .expect("读到 EOF");
    raw.lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            Json::parse(line).unwrap_or_else(|error| panic!("行应为 JSON：{line}（{error}）"))
        })
        .collect()
}

// ---- 测试 ----

/// ping → pong；prompt（mock chat + 注入网关）→ 完整 result JSON。
#[test]
fn ingress_ping_and_prompt_roundtrip() {
    let addr = start_server_with_link(ScriptedChat::one(text_response("一切正常")));
    let responses = send_lines(
        addr,
        &[
            r#"{"id":1,"method":"ping","params":{}}"#,
            r#"{"id":2,"method":"prompt","params":{"text":"看看状态","max_steps":4}}"#,
        ],
    );
    assert_eq!(responses.len(), 2, "{responses:?}");

    assert_eq!(responses[0].get("id").and_then(Json::as_i64), Some(1));
    assert_eq!(responses[0].get("ok").and_then(Json::as_bool), Some(true));
    assert_eq!(
        responses[0]
            .get_path(&["result", "pong"])
            .and_then(Json::as_bool),
        Some(true)
    );

    let prompt = &responses[1];
    assert_eq!(prompt.get("id").and_then(Json::as_i64), Some(2));
    assert_eq!(prompt.get("ok").and_then(Json::as_bool), Some(true));
    let result = prompt.get("result").expect("应有 result");
    assert_eq!(
        result.get("answer").and_then(Json::as_str),
        Some("一切正常")
    );
    assert_eq!(
        result.get("status").and_then(Json::as_str),
        Some("completed")
    );
    assert_eq!(result.get("steps").and_then(Json::as_u64), Some(1));
    assert_eq!(
        result
            .get_path(&["usage", "prompt_tokens"])
            .and_then(Json::as_u64),
        Some(21)
    );
    assert_eq!(
        result
            .get_path(&["usage", "completion_tokens"])
            .and_then(Json::as_u64),
        Some(6)
    );
    assert_eq!(
        result
            .get_path(&["usage", "total_tokens"])
            .and_then(Json::as_u64),
        Some(27)
    );
}

/// 未连接：prompt → not_linked；参数非法 → invalid_arguments。
#[test]
fn ingress_prompt_errors_carry_codes() {
    let addr = start_server(ScriptedChat::one(text_response("unused")), None);
    let responses = send_lines(
        addr,
        &[
            r#"{"id":1,"method":"prompt","params":{"text":"hi"}}"#,
            r#"{"id":2,"method":"prompt","params":{"text":""}}"#,
            r#"{"id":3,"method":"prompt","params":{"text":"hi","max_steps":99}}"#,
        ],
    );
    let codes: Vec<Option<&str>> = responses
        .iter()
        .map(|response| response.get_path(&["error", "code"]).and_then(Json::as_str))
        .collect();
    assert_eq!(
        codes,
        vec![
            Some("not_linked"),
            Some("invalid_arguments"),
            Some("invalid_arguments"),
        ],
        "{responses:?}"
    );
    // id 透传。
    assert_eq!(responses[0].get("id").and_then(Json::as_i64), Some(1));
    assert_eq!(responses[0].get("ok").and_then(Json::as_bool), Some(false));
}

/// 未知方法与畸形行：unknown_method / invalid_json 通知，连接不崩。
#[test]
fn ingress_unknown_method_and_malformed_lines() {
    let addr = start_server_with_link(ScriptedChat::one(text_response("unused")));
    let responses = send_lines(
        addr,
        &[
            r#"{"id":5,"method":"bogus","params":{}}"#,
            "{oops",
            r#""not an object""#,
            r#"{"id":"x","method":"ping"}"#,
            "",
            "   ",
        ],
    );
    assert_eq!(responses.len(), 4, "{responses:?}");
    let error = responses[0].get("error").unwrap();
    assert_eq!(
        error.get("code").and_then(Json::as_str),
        Some("unknown_method")
    );
    for response in &responses[1..] {
        assert_eq!(
            response.get("event").and_then(Json::as_str),
            Some("invalid_json"),
            "{response:?}"
        );
    }
    // 空白行不产生输出（上面只应有 4 条响应）。

    // 同一服务器此后仍可用（新连接正常 ping）。
    let responses = send_lines(addr, &[r#"{"id":9,"method":"ping","params":{}}"#]);
    assert_eq!(
        responses[0]
            .get_path(&["result", "pong"])
            .and_then(Json::as_bool),
        Some(true)
    );
}

/// ingress 端口 0 展开为实际端口；ping 在无 LLM 配置的 hub 上也可用。
#[test]
fn ingress_binds_ephemeral_port_and_pings_without_llm() {
    let core = PromptCore::<ScriptedChat>::lazy(
        Box::new(|| None::<ScriptedChat>),
        Policy::default(),
        agent_config(),
        TraceWriter::disabled(),
    );
    let addr = prompt_server::start_ingress(Arc::new(core), "127.0.0.1:0").expect("绑定临时端口");
    assert_ne!(addr.port(), 0, "端口 0 应展开为实际端口");
    let responses = send_lines(addr, &[r#"{"id":1,"method":"ping","params":{}}"#]);
    assert_eq!(
        responses[0]
            .get_path(&["result", "pong"])
            .and_then(Json::as_bool),
        Some(true)
    );
}

/// 提交后永不回复的网关（模拟一条挂着的长命令，如等待 prompt 回答的
/// `pc.ask` 任务）。
struct HangingGateway;

impl WorkerGateway for HangingGateway {
    fn list_workers(&mut self) -> Vec<WorkerInfo> {
        vec![worker("pc-1")]
    }

    fn submit(&mut self, _request: CommandRequest) -> Result<CommandReceipt, GatewayError> {
        // 永不发送回复：等待方只能等到预算耗尽。
        let (_sender, receiver) = std::sync::mpsc::channel();
        Ok(CommandReceipt::new(receiver, 500, "内存"))
    }

    fn wait_ready(&mut self, _timeout_ms: u64) -> Result<(), GatewayError> {
        Ok(())
    }

    fn is_ready(&self) -> bool {
        true
    }
}

/// 回归测试（pc.ask 分布式死锁）：`GatewayHandle::call` 等待最终事件期间
/// **不得**持有网关互斥锁——否则「命令等 prompt、prompt 等锁、锁等命令」
/// 互相卡死，只能等 DDL 超时收场。
#[test]
fn gateway_handle_lock_free_during_long_wait() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};

    let handle: GatewayHandle = Arc::new(Mutex::new(Box::new(HangingGateway)));
    let caller = {
        let mut handle = Arc::clone(&handle);
        std::thread::spawn(move || {
            let request = CommandRequest {
                timeout_ms: None,
                ddl_ms: 200,
                worker_id: String::from("pc-1"),
                kind: sekai_router::control::CommandKind::Task,
                name: String::from("pc.ask"),
                args: Vec::new(),
            };
            // 挂起等待（500ms 预算）；返回值无关紧要。
            let _ = handle.call(request);
        })
    };
    // 等待线程进入等待段。
    let entered = Arc::new(AtomicBool::new(false));
    let pinger = {
        let mut handle = Arc::clone(&handle);
        let entered = Arc::clone(&entered);
        std::thread::spawn(move || {
            entered.store(true, Ordering::SeqCst);
            let start = Instant::now();
            let workers = handle.list_workers();
            (workers.len(), start.elapsed())
        })
    };
    while !entered.load(Ordering::SeqCst) {
        std::thread::sleep(Duration::from_millis(1));
    }
    std::thread::sleep(Duration::from_millis(50));
    let (count, elapsed) = pinger.join().expect("list 线程应正常返回");
    assert_eq!(count, 1, "挂起命令期间 list_workers 应可用");
    assert!(
        elapsed < Duration::from_millis(400),
        "list_workers 不应被挂起命令阻塞（耗时 {elapsed:?}）"
    );
    let _ = caller.join();
}

// ---------------------------------------------------------------------------
// B9：ingress 访问控制
// ---------------------------------------------------------------------------

/// 以显式安全配置起服务（绕过环境变量，避免测试间互相影响）。
fn start_server_with_security(
    chat: ScriptedChat,
    security: prompt_server::IngressSecurity,
) -> std::net::SocketAddr {
    let core = PromptCore::new(
        chat,
        Policy::default(),
        agent_config(),
        TraceWriter::disabled(),
    );
    prompt_server::start_ingress_with(Arc::new(core), "127.0.0.1:0", security)
        .expect("应绑定临时端口")
}

/// 非回环绑定默认被拒绝；允许远程但没 token 也被拒绝。
#[test]
fn ingress_rejects_remote_bind_without_explicit_allow() {
    let core = PromptCore::new(
        ScriptedChat::one(text_response("unused")),
        Policy::default(),
        agent_config(),
        TraceWriter::disabled(),
    );
    let denied = prompt_server::start_ingress_with(
        Arc::new(core),
        "0.0.0.0:0",
        prompt_server::IngressSecurity::loopback_open(),
    );
    let error = denied.expect_err("默认必须拒绝非回环绑定");
    assert_eq!(
        error.kind(),
        std::io::ErrorKind::PermissionDenied,
        "{error}"
    );

    let core = PromptCore::new(
        ScriptedChat::one(text_response("unused")),
        Policy::default(),
        agent_config(),
        TraceWriter::disabled(),
    );
    let no_token = prompt_server::start_ingress_with(
        Arc::new(core),
        "0.0.0.0:0",
        prompt_server::IngressSecurity {
            allow_remote: true,
            ..prompt_server::IngressSecurity::loopback_open()
        },
    );
    assert!(no_token.is_err(), "允许远程但没有 token 也必须拒绝");
}

/// 配置 token 后：首个请求必须是 hello 握手；token 错误直接断开；
/// 正确握手之后 ping 可用。
#[test]
fn ingress_requires_token_handshake() {
    let addr = start_server_with_security(
        ScriptedChat::one(text_response("unused")),
        prompt_server::IngressSecurity {
            token: Some(String::from("s3cret")),
            ..prompt_server::IngressSecurity::loopback_open()
        },
    );

    // 未握手直接发 prompt → unauthorized。
    let responses = send_lines(
        addr,
        &[r#"{"id":1,"method":"prompt","params":{"text":"hi"}}"#],
    );
    assert_eq!(responses.len(), 1, "{responses:?}");
    assert_eq!(
        responses[0]
            .get_path(&["error", "code"])
            .and_then(Json::as_str),
        Some("unauthorized")
    );

    // token 错误 → unauthorized。
    let responses = send_lines(
        addr,
        &[r#"{"id":2,"method":"hello","params":{"token":"nope"}}"#],
    );
    assert_eq!(
        responses[0]
            .get_path(&["error", "code"])
            .and_then(Json::as_str),
        Some("unauthorized")
    );

    // 正确握手后同一连接可用。
    let responses = send_lines(
        addr,
        &[
            r#"{"id":3,"method":"hello","params":{"token":"s3cret"}}"#,
            r#"{"id":4,"method":"ping","params":{}}"#,
        ],
    );
    assert_eq!(responses.len(), 2, "{responses:?}");
    assert_eq!(
        responses[0]
            .get_path(&["result", "authenticated"])
            .and_then(Json::as_bool),
        Some(true)
    );
    assert_eq!(
        responses[1]
            .get_path(&["result", "pong"])
            .and_then(Json::as_bool),
        Some(true)
    );
}

/// 并发连接超限：第二个连接收到 too_many_connections。
#[test]
fn ingress_rejects_connections_over_cap() {
    let addr = start_server_with_security(
        ScriptedChat::one(text_response("unused")),
        prompt_server::IngressSecurity {
            max_connections: 1,
            ..prompt_server::IngressSecurity::loopback_open()
        },
    );

    let mut first = TcpStream::connect(addr).expect("连接 ingress");
    first
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("设置读超时");
    first
        .write_all(b"{\"id\":1,\"method\":\"ping\",\"params\":{}}\n")
        .expect("写 ping");
    first.flush().expect("flush");
    let mut line = String::new();
    BufReader::new(first.try_clone().expect("克隆连接"))
        .read_line(&mut line)
        .expect("读 pong");
    assert!(line.contains("pong"), "{line}");

    let second = TcpStream::connect(addr).expect("连接 ingress");
    second
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("设置读超时");
    let mut raw = String::new();
    BufReader::new(second)
        .read_to_string(&mut raw)
        .expect("读到拒绝行");
    assert!(raw.contains("too_many_connections"), "{raw}");
    drop(first);
}

/// ingress 计数：认证失败与 prompt 次数可观测。
#[test]
fn ingress_stats_track_rejections() {
    let core = Arc::new(PromptCore::new(
        ScriptedChat::one(text_response("unused")),
        Policy::default(),
        agent_config(),
        TraceWriter::disabled(),
    ));
    let addr = prompt_server::start_ingress_with(
        Arc::clone(&core),
        "127.0.0.1:0",
        prompt_server::IngressSecurity {
            token: Some(String::from("s3cret")),
            ..prompt_server::IngressSecurity::loopback_open()
        },
    )
    .expect("应绑定临时端口");

    let responses = send_lines(
        addr,
        &[r#"{"id":1,"method":"prompt","params":{"text":"hi"}}"#],
    );
    assert_eq!(
        responses[0]
            .get_path(&["error", "code"])
            .and_then(Json::as_str),
        Some("unauthorized")
    );
    let stats = core.ingress_stats();
    assert_eq!(stats.connections, 1, "{stats:?}");
    assert_eq!(stats.rejected_auth, 1, "{stats:?}");
    assert_eq!(stats.prompts, 0, "{stats:?}");
}
