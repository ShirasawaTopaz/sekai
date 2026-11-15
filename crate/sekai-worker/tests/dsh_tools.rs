//! DSH 工具桥集成测试：内存 V1 链路 + 内存 JSONL 假 DSH 插件。
//!
//! Router 端直接读写 V1 帧（support::Loop），Worker 端由真实 Runtime 驱动；
//! dsh.call / dsh.tools 的带外 JSONL 通道用 mpsc 内存行对模拟 DSH worker
//! 插件（与 llm_channel::LineIo 的生产实现同构）。覆盖：Hello 能力通告、
//! dsh.tools 清单、dsh.call 成功/工具失败/桥层失败映射、DDL 到期取消并回投
//! dsh_cancel。

mod support;

use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use sekai::sekai_stdin::{Kind, Stdin, TaskBody};
use sekai::sekai_stdout::{ErrorPayload, Hello, Phase, Stdout};
use sekai::sekai_wire::ErrorCode;
use sekai_worker::link::{Link, LinkError};
use sekai_worker::llm_channel::{DshPromptChannel, DshToolTransport, LineIo};
use sekai_worker::mini_json::Json;

use support::{BASE_UNIX_MS, Loop};

// ---- 帧构造（Router 侧视角） ----

fn stdin_bytes(id: u64, kind: Kind, payload: Vec<u8>, ddl_ms: u32) -> Vec<u8> {
    Stdin::new(id, kind, BASE_UNIX_MS, 0, ddl_ms, payload)
        .expect("Stdin 构造失败")
        .encode()
        .expect("Stdin 编码失败")
}

fn task_stdin(id: u64, name: &str, args: &[u8], ddl_ms: u32) -> Vec<u8> {
    let body = TaskBody::new(name, args.to_vec()).expect("任务载荷构造失败");
    stdin_bytes(
        id,
        Kind::Task,
        body.encode().expect("任务载荷编码失败"),
        ddl_ms,
    )
}

// ---- mpsc 内存行对（模拟 DSH worker 插件的 stdin/stdout） ----

#[derive(Clone)]
struct MemLineIo {
    incoming: Arc<Mutex<mpsc::Receiver<String>>>,
    outgoing: mpsc::Sender<String>,
}

fn mem_pair() -> (MemLineIo, MemLineIo) {
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

/// 响应信封（与 DSH 插件契约一致）。
fn envelope_ok(id: u64, result: Json) -> String {
    Json::object(vec![
        ("id", Json::from(id)),
        ("ok", Json::Bool(true)),
        ("result", result),
    ])
    .to_compact_string()
}

fn envelope_err(id: u64, code: &str, message: &str) -> String {
    Json::object(vec![
        ("id", Json::from(id)),
        ("ok", Json::Bool(false)),
        (
            "error",
            Json::object(vec![
                ("code", Json::from(code)),
                ("message", Json::from(message)),
            ]),
        ),
    ])
    .to_compact_string()
}

/// 假 DSH worker 插件：处理 dsh_tools / dsh_tool / dsh_cancel。
///
/// dsh_tool 的 slow 工具永不响应（用于 DDL 取消路径）；其余按契约回信封。
/// 收到的取消 id 记录到 cancel_seen（断言 dsh_cancel 的 params.id）。
fn spawn_bridge(peer_io: MemLineIo, cancel_seen: Arc<Mutex<Vec<u64>>>) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let mut peer = peer_io;
        while let Some(line) = peer.read_line() {
            let Ok(value) = Json::parse(line.trim()) else {
                continue;
            };
            let Some(id) = value.get("id").and_then(Json::as_u64) else {
                // 无顶层 id：只可能是 dsh_cancel 通知（无回包）。
                if value.get("method").and_then(Json::as_str) == Some("dsh_cancel")
                    && let Some(cancel_id) =
                        value.get_path(&["params", "id"]).and_then(Json::as_u64)
                {
                    cancel_seen.lock().unwrap().push(cancel_id);
                }
                continue;
            };
            let method = value.get("method").and_then(Json::as_str).unwrap_or("");
            let response = match method {
                "dsh_tools" => envelope_ok(
                    id,
                    Json::object(vec![
                        (
                            "tools",
                            Json::Array(vec![
                                Json::object(vec![
                                    ("name", Json::from("read")),
                                    ("description", Json::from("读取文件")),
                                ]),
                                Json::object(vec![
                                    ("name", Json::from("glob")),
                                    ("description", Json::from("匹配路径")),
                                ]),
                            ]),
                        ),
                        ("count", Json::num(2)),
                    ]),
                ),
                "dsh_tool" => {
                    let tool = value
                        .get_path(&["params", "tool"])
                        .and_then(Json::as_str)
                        .unwrap_or("");
                    match tool {
                        "read" => envelope_ok(
                            id,
                            Json::object(vec![
                                ("is_error", Json::Bool(false)),
                                ("value", Json::object(vec![("path", Json::from("a.txt"))])),
                                ("text", Json::from("hello from DSH")),
                                ("truncated", Json::Bool(false)),
                            ]),
                        ),
                        "fail" => envelope_ok(
                            id,
                            Json::object(vec![
                                ("is_error", Json::Bool(true)),
                                (
                                    "error",
                                    Json::object(vec![
                                        ("name", Json::from("read")),
                                        ("code", Json::from("EACCES")),
                                    ]),
                                ),
                                ("text", Json::from("工具内部失败")),
                            ]),
                        ),
                        "disabled" => envelope_err(id, "disabled", "dshTools 未启用"),
                        // 永不响应：等对端发 dsh_cancel。
                        "slow" => continue,
                        other => envelope_err(id, "unknown_tool", other),
                    }
                }
                other => envelope_err(id, "unknown_method", other),
            };
            if !peer.write_line(&response) {
                break;
            }
        }
    })
}

// ---- 测试宿主 ----

struct Harness {
    lp: Loop,
    cancels: Arc<Mutex<Vec<u64>>>,
}

/// 构造 WorkerHost：能力含 dsh.call/dsh.tools，桥接内存 JSONL 假插件。
fn harness() -> Harness {
    let (worker_io, peer_io) = mem_pair();
    let cancels = Arc::new(Mutex::new(Vec::new()));
    spawn_bridge(peer_io, cancels.clone());

    let channel = Arc::new(DshPromptChannel::with_timeout_ms(
        boxed(worker_io.clone()),
        boxed(worker_io),
        30_000,
    ));
    let pump = channel.clone();
    thread::spawn(move || pump.pump());
    let bridge: Arc<dyn DshToolTransport> = channel;

    Harness {
        lp: Loop::new_with_dsh_tools("pc-dsh", 41, bridge),
        cancels,
    }
}

fn decode(frame: &[u8]) -> Stdout {
    Stdout::decode(frame).expect("Stdout 解码失败")
}

/// 等指定命令的指定阶段帧（期间 pump Worker、真实等待并推进手动时钟）。
fn wait_phase(lp: &mut Loop, cmd_id: u64, want: Phase, max_ms: u64) -> Stdout {
    let deadline = Instant::now() + Duration::from_millis(max_ms);
    loop {
        lp.pump_until_idle(2);
        loop {
            match lp.router.read_frame() {
                Ok(Some(frame)) => {
                    let out = decode(&frame);
                    if out.cmd_id == cmd_id && out.phase == want {
                        return out;
                    }
                }
                Ok(None) => panic!("Router 端链路被关闭"),
                Err(LinkError::Timeout) => break,
                Err(error) => panic!("Router 端读取失败：{error}"),
            }
        }
        assert!(
            Instant::now() < deadline,
            "等待命令 {cmd_id} 的 {want:?} 帧超时（{max_ms}ms）"
        );
        thread::sleep(Duration::from_millis(10));
        lp.clock.advance(10);
    }
}

/// 等指定命令的终态帧（Completed 或 Failed）。
fn wait_final(lp: &mut Loop, cmd_id: u64, max_ms: u64) -> Stdout {
    let deadline = Instant::now() + Duration::from_millis(max_ms);
    loop {
        lp.pump_until_idle(2);
        loop {
            match lp.router.read_frame() {
                Ok(Some(frame)) => {
                    let out = decode(&frame);
                    if out.cmd_id == cmd_id && matches!(out.phase, Phase::Completed | Phase::Failed)
                    {
                        return out;
                    }
                }
                Ok(None) => panic!("Router 端链路被关闭"),
                Err(LinkError::Timeout) => break,
                Err(error) => panic!("Router 端读取失败：{error}"),
            }
        }
        assert!(
            Instant::now() < deadline,
            "等待命令 {cmd_id} 的终态帧超时（{max_ms}ms）"
        );
        thread::sleep(Duration::from_millis(10));
        lp.clock.advance(10);
    }
}

fn error_of(frame: &Stdout) -> ErrorPayload {
    ErrorPayload::decode(&frame.payload).expect("错误载荷解码失败")
}

// ---- 用例 ----

#[test]
fn hello_advertises_dsh_bridge_tasks() {
    let mut h = harness();
    let hello_frame = h.lp.router_read();
    let hello_out = decode(&hello_frame);
    assert_eq!(hello_out.phase, Phase::Hello);
    let hello = Hello::decode(&hello_out.payload).expect("Hello 载荷解码失败");
    for name in ["dsh.call", "dsh.tools"] {
        assert!(
            hello.tasks.iter().any(|task| task == name),
            "Hello 缺少任务 {name}"
        );
        assert!(
            !hello.tools.iter().any(|tool| tool == name),
            "{name} 不应是 Tool（会阻塞帧循环）"
        );
    }
    h.lp.assert_router_idle();
}

#[test]
fn dsh_tools_lists_tools_over_v1_frames() {
    let mut h = harness();
    let _hello = h.lp.router_read();

    h.lp.router
        .write_frame(&task_stdin(3, "dsh.tools", b"", 5_000))
        .expect("写链路失败");
    let out = wait_final(&mut h.lp, 3, 5_000);
    assert_eq!(out.phase, Phase::Completed, "dsh.tools 应成功完成");
    let text = String::from_utf8(out.payload).expect("清单必须是 UTF-8");
    assert_eq!(text, "read — 读取文件\nglob — 匹配路径", "{text}");
}

#[test]
fn dsh_call_success_and_error_mapping_over_v1_frames() {
    let mut h = harness();
    let _hello = h.lp.router_read();

    // 成功：Done(result.text)。
    h.lp.router
        .write_frame(&task_stdin(
            4,
            "dsh.call",
            br#"{"tool":"read","arguments":{"path":"a.txt"}}"#,
            5_000,
        ))
        .expect("写链路失败");
    let completed = wait_final(&mut h.lp, 4, 5_000);
    assert_eq!(completed.phase, Phase::Completed, "dsh.call 应成功");
    assert_eq!(completed.payload, b"hello from DSH");

    // 工具自身失败：Failed(Internal) + 文本。
    h.lp.router
        .write_frame(&task_stdin(5, "dsh.call", br#"{"tool":"fail"}"#, 5_000))
        .expect("写链路失败");
    let failed = wait_final(&mut h.lp, 5, 5_000);
    assert_eq!(failed.phase, Phase::Failed);
    let error = error_of(&failed);
    assert_eq!(error.code, ErrorCode::Internal, "{}", error.message);
    assert!(error.message.contains("工具内部失败"), "{}", error.message);

    // 桥层失败 disabled → Failed(Unsupported)（错误码按冻映射透传）。
    h.lp.router
        .write_frame(&task_stdin(6, "dsh.call", br#"{"tool":"disabled"}"#, 5_000))
        .expect("写链路失败");
    let failed = wait_final(&mut h.lp, 6, 5_000);
    assert_eq!(failed.phase, Phase::Failed);
    let error = error_of(&failed);
    assert_eq!(error.code, ErrorCode::Unsupported, "{}", error.message);
    assert!(
        error.message.contains("dshTools 未启用"),
        "{}",
        error.message
    );
}

#[test]
fn dsh_call_ddl_expiry_cancels_upstream_over_jsonl() {
    let mut h = harness();
    let _hello = h.lp.router_read();

    // slow 工具永不响应；DDL 只有 100ms。
    h.lp.router
        .write_frame(&task_stdin(7, "dsh.call", br#"{"tool":"slow"}"#, 100))
        .expect("写链路失败");
    let started = wait_phase(&mut h.lp, 7, Phase::Started, 5_000);
    assert_eq!(started.phase, Phase::Started);

    // 越过 DDL：运行时丢弃任务（Drop 置取消标志）并回 Failed(Timeout)。
    h.lp.clock.advance(101);
    let failed = wait_final(&mut h.lp, 7, 5_000);
    assert_eq!(failed.phase, Phase::Failed);
    let error = error_of(&failed);
    assert_eq!(error.code, ErrorCode::Timeout);

    // 工作线程观察到取消后，通道回投无 id 的 dsh_cancel（params.id 是请求 id）。
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if !h.cancels.lock().unwrap().is_empty() {
            break;
        }
        assert!(Instant::now() < deadline, "取消后未回投 dsh_cancel");
        thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(h.cancels.lock().unwrap().clone(), vec![1]);
}
