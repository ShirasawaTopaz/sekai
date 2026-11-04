//! serve 无头 JSONL 服务模式测试：内存网关注入 + 协议往返断言。
//!
//! 覆盖任务卡所列场景：协议往返（link→workers→tool_call→policy.set→
//! 策略拒绝 policy_denied）、未知方法 unknown_method、畸形 JSON 行跳行回错
//! 不崩、EOF / shutdown 优雅退出 exit 0、worker_online 通知。

use std::collections::VecDeque;
use std::io::{BufRead, Cursor, Read, Result as IoResult, Write};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use sekai_router::control::{CommandRequest, CommandResult};
use sekai_router::gateway::{GatewayError, GatewayHandle, WorkerGateway};
use sekai_router::json::Json;
use sekai_router::serve::{
    InjectedLink, ServeConfig, ServeState, serve_loop, worker_notifications,
};
use sekai_router::tools::Policy;
use sekai_router::worker::WorkerInfo;

// ---- 测试替身 ----

/// 可从外部改 Worker 集合、可记录命令请求的内存网关。
struct FakeGateway {
    workers: Arc<Mutex<Vec<WorkerInfo>>>,
    calls: Arc<Mutex<Vec<CommandRequest>>>,
}

impl WorkerGateway for FakeGateway {
    fn list_workers(&mut self) -> Vec<WorkerInfo> {
        self.workers.lock().unwrap().clone()
    }

    fn call(&mut self, request: CommandRequest) -> Result<CommandResult, GatewayError> {
        self.calls.lock().unwrap().push(request.clone());
        Ok(CommandResult {
            cmd_id: 7,
            worker_id: request.worker_id,
            kind: request.kind,
            name: request.name,
            payload: b"ok".to_vec(),
            phases: Vec::new(),
            elapsed_ms: 3,
            failure: None,
        })
    }

    fn wait_ready(&mut self, _timeout_ms: u64) -> Result<(), GatewayError> {
        Ok(())
    }

    fn is_ready(&self) -> bool {
        true
    }
}

/// call 恒失败的内存网关（测网关错误 → failure_result_json 映射）。
struct FailingGateway;

impl WorkerGateway for FailingGateway {
    fn list_workers(&mut self) -> Vec<WorkerInfo> {
        vec![worker("pc-1")]
    }

    fn call(&mut self, _request: CommandRequest) -> Result<CommandResult, GatewayError> {
        Err(GatewayError::new("worker_offline", "测试：Worker 已离线"))
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
        boot_id: 11,
        firmware: String::from("0.1.0"),
        uptime_ms: 100,
        unix_ms: 1_700_000_000_000,
        tools: vec![
            String::from("gpio.info"),
            String::from("gpio.read"),
            String::from("gpio.write"),
        ],
        tasks: vec![String::from("echo")],
        workers: Vec::new(),
        active_tasks: Vec::new(),
        last_seen_mono_ms: 5,
        hello_seq: 1,
    }
}

fn fake_handle(
    workers: Arc<Mutex<Vec<WorkerInfo>>>,
    calls: &Arc<Mutex<Vec<CommandRequest>>>,
) -> GatewayHandle {
    Arc::new(Mutex::new(Box::new(FakeGateway {
        workers,
        calls: Arc::clone(calls),
    }) as Box<dyn WorkerGateway>))
}

fn state() -> ServeState {
    ServeState::new(ServeConfig::minimal(), Policy::default())
}

/// 驱动一行请求；断言不退出并返回输出 JSON。
fn request(state: &mut ServeState, line: &str) -> Vec<Json> {
    let (messages, exit) = state.handle_line(line);
    assert!(!exit, "该请求不应触发退出：{line}");
    messages
}

// ---- 协议往返 ----

/// link（tcp，端口 0 自动分配）→ link_off → 注入网关 → workers →
/// tool_call → policy.set → 写工具被策略拒绝（policy_denied）。
#[test]
fn protocol_roundtrip_link_workers_tool_call_policy() {
    let mut state = state();

    // policy.get：初始策略快照。
    let messages = request(&mut state, r#"{"id":0,"method":"policy.get","params":{}}"#);
    let result = messages[0].get("result").unwrap();
    assert_eq!(
        result.get("allow_write").and_then(Json::as_bool),
        Some(false)
    );
    assert_eq!(
        result.get("allow_tasks").and_then(Json::as_bool),
        Some(false)
    );
    assert!(result.get("worker_filter").unwrap().is_null());

    // link tcp：响应 + link_state 通知（ready=true）。
    let messages = request(
        &mut state,
        r#"{"id":1,"method":"link","params":{"mode":"tcp","listen_addr":"127.0.0.1:0"}}"#,
    );
    assert_eq!(messages.len(), 2, "{messages:?}");
    assert_eq!(messages[0].get("id").and_then(Json::as_i64), Some(1));
    assert_eq!(messages[0].get("ok").and_then(Json::as_bool), Some(true));
    let result = messages[0].get("result").unwrap();
    assert_eq!(result.get("mode").and_then(Json::as_str), Some("tcp"));
    let addr = result.get("addr").and_then(Json::as_str).unwrap();
    assert!(addr.starts_with("127.0.0.1:"), "应返回实际地址：{addr}");
    let desc = result.get("desc").and_then(Json::as_str).unwrap();
    assert_eq!(desc, format!("tcp 监听（{addr}）"));
    assert_eq!(
        messages[1].get("event").and_then(Json::as_str),
        Some("link_state")
    );
    assert_eq!(messages[1].get("ready").and_then(Json::as_bool), Some(true));
    assert_eq!(messages[1].get("desc").and_then(Json::as_str), Some(desc));

    // 关闭监听（tcp 上不会有真实 Worker 连入），换成注入网关。
    let messages = request(&mut state, r#"{"id":2,"method":"link_off","params":{}}"#);
    let result = messages[0].get("result").unwrap();
    assert_eq!(result.get("mode").and_then(Json::as_str), Some("off"));
    assert_eq!(result.get("had_link").and_then(Json::as_bool), Some(true));
    assert_eq!(
        messages[1].get("event").and_then(Json::as_str),
        Some("link_state")
    );
    assert_eq!(
        messages[1].get("ready").and_then(Json::as_bool),
        Some(false)
    );

    let workers = Arc::new(Mutex::new(vec![worker("pc-1")]));
    let calls = Arc::new(Mutex::new(Vec::new()));
    state.set_gateway(
        fake_handle(Arc::clone(&workers), &calls),
        "tcp",
        String::from("测试链路（注入）"),
    );

    // workers：{count,workers[]}（workers_json 形状）。
    let messages = request(&mut state, r#"{"id":3,"method":"workers","params":{}}"#);
    let result = messages[0].get("result").unwrap();
    assert_eq!(result.get("count").and_then(Json::as_i64), Some(1));
    let list = result.get("workers").and_then(Json::as_array).unwrap();
    assert_eq!(
        list[0].get("worker_id").and_then(Json::as_str),
        Some("pc-1")
    );
    assert!(list[0].get("tools").is_some(), "Worker 对象应含能力列表");

    // tool_call：只读工具默认允许，结果为 command_result_json 形状。
    let messages = request(
        &mut state,
        r#"{"id":4,"method":"tool_call","params":{"worker_id":"pc-1","tool":"gpio.read","args":"IO4","ddl_ms":500}}"#,
    );
    let result = messages[0].get("result").unwrap();
    assert_eq!(result.get("ok").and_then(Json::as_bool), Some(true));
    assert_eq!(result.get("worker_id").and_then(Json::as_str), Some("pc-1"));
    assert_eq!(result.get("kind").and_then(Json::as_str), Some("tool"));
    assert_eq!(result.get("name").and_then(Json::as_str), Some("gpio.read"));
    assert_eq!(result.get("cmd_id").and_then(Json::as_i64), Some(7));
    assert_eq!(result.get("payload").and_then(Json::as_str), Some("ok"));
    assert!(result.get("error").is_none(), "成功结果不应有 error");
    // 请求透传到网关：ddl 与参数文本一致。
    let recorded = calls.lock().unwrap();
    assert_eq!(recorded.len(), 1, "只应有一次命令调用");
    assert_eq!(recorded[0].ddl_ms, 500);
    assert_eq!(recorded[0].args, b"IO4".to_vec());
    drop(recorded);

    // policy.set：显式关写权限、清 Worker 过滤器，返回更新后的完整策略。
    let messages = request(
        &mut state,
        r#"{"id":5,"method":"policy.set","params":{"allow_write":false,"worker_filter":null}}"#,
    );
    let result = messages[0].get("result").unwrap();
    assert_eq!(
        result.get("allow_write").and_then(Json::as_bool),
        Some(false)
    );
    assert!(result.get("worker_filter").unwrap().is_null());

    // 写工具被策略拒绝：信封 ok（响应送达），result 为 failure_result_json
    // 形状且 error.code=policy_denied；命令未发到网关。
    let messages = request(
        &mut state,
        r#"{"id":6,"method":"tool_call","params":{"worker_id":"pc-1","tool":"gpio.write","args":"IO4=1"}}"#,
    );
    assert_eq!(messages[0].get("ok").and_then(Json::as_bool), Some(true));
    let result = messages[0].get("result").unwrap();
    assert_eq!(result.get("ok").and_then(Json::as_bool), Some(false));
    let error = result.get("error").unwrap();
    assert_eq!(
        error.get("code").and_then(Json::as_str),
        Some("policy_denied")
    );
    assert_eq!(result.get("worker_id").and_then(Json::as_str), Some("pc-1"));
    assert_eq!(calls.lock().unwrap().len(), 1, "策略拒绝的命令不应发到网关");

    // task 默认也被拒（allow_tasks=false）。
    let messages = request(
        &mut state,
        r#"{"id":7,"method":"task","params":{"worker_id":"pc-1","task":"echo","args":""}}"#,
    );
    let result = messages[0].get("result").unwrap();
    let error = result.get("error").unwrap();
    assert_eq!(
        error.get("code").and_then(Json::as_str),
        Some("policy_denied")
    );
    assert_eq!(result.get("kind").and_then(Json::as_str), Some("task"));

    // policy.set 放开 tasks 后命令可达网关。
    let messages = request(
        &mut state,
        r#"{"id":8,"method":"policy.set","params":{"allow_tasks":true}}"#,
    );
    assert_eq!(
        messages[0]
            .get("result")
            .unwrap()
            .get("allow_tasks")
            .and_then(Json::as_bool),
        Some(true)
    );
    let messages = request(
        &mut state,
        r#"{"id":9,"method":"task","params":{"worker_id":"pc-1","task":"echo","args":"hi"}}"#,
    );
    let result = messages[0].get("result").unwrap();
    assert_eq!(result.get("ok").and_then(Json::as_bool), Some(true));
    assert_eq!(result.get("kind").and_then(Json::as_str), Some("task"));
    assert_eq!(calls.lock().unwrap().len(), 2);
}

// ---- 未知方法 / 参数校验 / 未连接 ----

#[test]
fn unknown_method_replies_error() {
    let mut state = state();
    let messages = request(&mut state, r#"{"id":9,"method":"bogus","params":{}}"#);
    assert_eq!(messages.len(), 1);
    let message = &messages[0];
    assert_eq!(message.get("id").and_then(Json::as_i64), Some(9));
    assert_eq!(message.get("ok").and_then(Json::as_bool), Some(false));
    let error = message.get("error").unwrap();
    assert_eq!(
        error.get("code").and_then(Json::as_str),
        Some("unknown_method")
    );
    assert!(
        error
            .get("message")
            .and_then(Json::as_str)
            .unwrap()
            .contains("bogus")
    );
}

/// tool_call 参数校验复用 tools.rs 严格解析：未知字段 / 非法名称 /
/// ddl 越界 / 缺必填字段 → invalid_arguments 信封错误。
#[test]
fn tool_call_argument_validation_is_strict() {
    let mut state = state();
    let workers = Arc::new(Mutex::new(vec![worker("pc-1")]));
    let calls = Arc::new(Mutex::new(Vec::new()));
    state.set_gateway(
        fake_handle(workers, &calls),
        "tcp",
        String::from("测试链路（注入）"),
    );
    for (line, needle) in [
        (
            r#"{"id":1,"method":"tool_call","params":{"worker_id":"pc-1","tool":"gpio.read","args":"","bogus":1}}"#,
            "未知参数",
        ),
        (
            r#"{"id":2,"method":"tool_call","params":{"worker_id":"pc-1","tool":"bad name","args":""}}"#,
            "非法",
        ),
        (
            r#"{"id":3,"method":"tool_call","params":{"worker_id":"pc-1","tool":"gpio.read","args":"","ddl_ms":0}}"#,
            "ddl_ms",
        ),
        (
            r#"{"id":4,"method":"tool_call","params":{"tool":"gpio.read","args":""}}"#,
            "缺少必填参数",
        ),
    ] {
        let messages = request(&mut state, line);
        let error = messages[0].get("error").unwrap();
        assert_eq!(
            error.get("code").and_then(Json::as_str),
            Some("invalid_arguments"),
            "{line}"
        );
        assert!(
            error
                .get("message")
                .and_then(Json::as_str)
                .unwrap()
                .contains(needle),
            "{line} 应含「{needle}」"
        );
    }
    assert!(calls.lock().unwrap().is_empty(), "非法参数不应触达网关");
}

/// 未连接时 workers / tool_call 回 link_error；stdio 拉起失败回 link_error
/// 外加 link_state ready=false 通知。
#[test]
fn link_errors_are_reported_with_link_state() {
    let mut state = state();
    for line in [
        r#"{"id":1,"method":"workers","params":{}}"#,
        r#"{"id":2,"method":"tool_call","params":{"worker_id":"pc-1","tool":"gpio.read","args":""}}"#,
    ] {
        let messages = request(&mut state, line);
        assert_eq!(
            messages[0]
                .get("error")
                .unwrap()
                .get("code")
                .and_then(Json::as_str),
            Some("link_error"),
            "{line}"
        );
    }

    // stdio 程序不存在：link_error 响应 + link_state ready=false 通知。
    let messages = request(
        &mut state,
        r#"{"id":3,"method":"link","params":{"mode":"stdio","worker_bin":"sekai-no-such-binary-xyz"}}"#,
    );
    assert_eq!(
        messages[0]
            .get("error")
            .unwrap()
            .get("code")
            .and_then(Json::as_str),
        Some("link_error")
    );
    assert_eq!(
        messages[1].get("event").and_then(Json::as_str),
        Some("link_state")
    );
    assert_eq!(
        messages[1].get("ready").and_then(Json::as_bool),
        Some(false)
    );

    // 未知 mode / 未知字段同样报 invalid_arguments。
    for line in [
        r#"{"id":4,"method":"link","params":{"mode":"vpn"}}"#,
        r#"{"id":5,"method":"link","params":{"mode":"tcp","bogus":1}}"#,
        r#"{"id":6,"method":"link","params":{}}"#,
    ] {
        let messages = request(&mut state, line);
        assert_eq!(
            messages[0]
                .get("error")
                .unwrap()
                .get("code")
                .and_then(Json::as_str),
            Some("invalid_arguments"),
            "{line}"
        );
    }
}

/// status：链路描述 / 模式 / 就绪 / Worker 数 / 策略摘要 / 版本 / pid。
#[test]
fn status_reports_link_policy_version_and_pid() {
    let mut state = state();
    let messages = request(&mut state, r#"{"id":1,"method":"status","params":{}}"#);
    let result = messages[0].get("result").unwrap();
    assert_eq!(result.get("mode").and_then(Json::as_str), Some("off"));
    assert_eq!(result.get("link").and_then(Json::as_str), Some("未连接"));
    assert_eq!(result.get("ready").and_then(Json::as_bool), Some(false));
    assert_eq!(result.get("worker_count").and_then(Json::as_i64), Some(0));
    assert!(
        result
            .get("policy")
            .and_then(Json::as_str)
            .unwrap()
            .contains("拒绝")
    );
    assert!(
        result
            .get("version")
            .and_then(Json::as_str)
            .unwrap()
            .contains("sekai-router")
    );
    assert!(result.get("pid").and_then(Json::as_i64).unwrap() > 0);
}

/// 网关 call 失败（如超时 / 链路丢失）：信封 ok，result 为
/// failure_result_json 形状（错误码与 retryable 透传）。
#[test]
fn gateway_call_failure_maps_to_failure_result() {
    let mut state = state();
    state.set_gateway(
        Arc::new(Mutex::new(
            Box::new(FailingGateway) as Box<dyn WorkerGateway>
        )),
        "tcp",
        String::from("失败链路（注入）"),
    );
    let messages = request(
        &mut state,
        r#"{"id":1,"method":"tool_call","params":{"worker_id":"pc-1","tool":"gpio.read","args":""}}"#,
    );
    assert_eq!(messages[0].get("ok").and_then(Json::as_bool), Some(true));
    let result = messages[0].get("result").unwrap();
    assert_eq!(result.get("ok").and_then(Json::as_bool), Some(false));
    let error = result.get("error").unwrap();
    assert_eq!(
        error.get("code").and_then(Json::as_str),
        Some("worker_offline")
    );
    assert_eq!(error.get("retryable").and_then(Json::as_bool), Some(true));
}

// ---- 畸形行 ----

/// 畸形 JSON 行：跳行、invalid_json 通知（无 id 可取），进程不崩。
#[test]
fn malformed_lines_are_skipped_without_crash() {
    let mut state = state();
    // 非 JSON。
    let messages = request(&mut state, "{oops");
    assert_eq!(messages.len(), 1);
    assert_eq!(
        messages[0].get("event").and_then(Json::as_str),
        Some("invalid_json")
    );
    assert!(
        messages[0]
            .get("message")
            .and_then(Json::as_str)
            .unwrap()
            .contains("JSON")
    );
    // JSON 但非对象 / 缺 id / id 非整数：同样取不到 id，以通知形式回错。
    for line in [
        r#""hello""#,
        r#"{"method":"status"}"#,
        r#"{"id":"x","method":"status"}"#,
        r#"{"id":1.5,"method":"status"}"#,
    ] {
        let messages = request(&mut state, line);
        assert_eq!(
            messages[0].get("event").and_then(Json::as_str),
            Some("invalid_json"),
            "{line}"
        );
    }
    // method 缺失但 id 合法：正常信封错误。
    let messages = request(&mut state, r#"{"id":7}"#);
    assert_eq!(
        messages[0]
            .get("error")
            .unwrap()
            .get("code")
            .and_then(Json::as_str),
        Some("invalid_arguments")
    );
    // 之后照常工作（不崩）。
    let messages = request(&mut state, r#"{"id":1,"method":"status","params":{}}"#);
    assert_eq!(messages[0].get("ok").and_then(Json::as_bool), Some(true));
    // 空白行静默跳过。
    let (messages, _) = state.handle_line("   ");
    assert!(messages.is_empty());
}

// ---- 通知 diff（poll 核心逻辑）----

#[test]
fn worker_notifications_diff_baseline_both_ways() {
    let mut baseline = Vec::new();
    let mut notifications = worker_notifications(&mut baseline, &[worker("pc-1")]);
    assert_eq!(notifications.len(), 1);
    assert_eq!(
        notifications[0].get("event").and_then(Json::as_str),
        Some("worker_online")
    );
    assert_eq!(
        notifications[0].get("worker_id").and_then(Json::as_str),
        Some("pc-1")
    );
    let worker_object = notifications[0].get("worker").unwrap();
    assert_eq!(
        worker_object.get("worker_id").and_then(Json::as_str),
        Some("pc-1")
    );
    assert!(worker_object.get("tools").is_some());

    // 无变化 → 空。
    assert!(worker_notifications(&mut baseline, &[worker("pc-1")]).is_empty());

    // Worker 消失 → 离线通知（只带 worker_id）。
    notifications = worker_notifications(&mut baseline, &[]);
    assert_eq!(notifications.len(), 1);
    assert_eq!(
        notifications[0].get("event").and_then(Json::as_str),
        Some("worker_offline")
    );
    assert_eq!(
        notifications[0].get("worker_id").and_then(Json::as_str),
        Some("pc-1")
    );
}

// ---- serve_loop（线程编排 + EOF/shutdown）----

/// 可共享的内存输出（Vec<u8> 的 Write 适配）。
#[derive(Clone, Default)]
struct SharedOutput(Arc<Mutex<Vec<u8>>>);

impl Write for SharedOutput {
    fn write(&mut self, buf: &[u8]) -> IoResult<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> IoResult<()> {
        Ok(())
    }
}

fn output_text(output: &SharedOutput) -> String {
    String::from_utf8(output.0.lock().unwrap().clone()).unwrap()
}

fn output_json(output: &SharedOutput) -> Vec<Json> {
    output_text(output)
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| Json::parse(line).expect("stdout 每行都应是 JSON"))
        .collect()
}

/// EOF 即优雅退出：退出码 0，stdout 第一行（且仅一行）为 ready 通知。
#[test]
fn serve_loop_eof_exits_zero_with_ready_first() {
    let output = SharedOutput::default();
    let code = serve_loop(
        Cursor::new(String::new()),
        output.clone(),
        ServeConfig::minimal(),
        Policy::default(),
        None,
        || {},
    );
    assert_eq!(code, 0);
    let lines = output_json(&output);
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert_eq!(lines[0].get("event").and_then(Json::as_str), Some("ready"));
}

/// 请求依次处理；畸形行回 invalid_json 通知；shutdown 响应后不再处理
/// 后续行；输入耗尽（EOF）后优雅退出 0。
#[test]
fn serve_loop_processes_requests_until_shutdown() {
    let input = concat!(
        "{\"id\":1,\"method\":\"status\",\"params\":{}}\n",
        "{bad json\n",
        "{\"id\":2,\"method\":\"shutdown\",\"params\":{}}\n",
        "{\"id\":3,\"method\":\"status\",\"params\":{}}\n",
    );
    let output = SharedOutput::default();
    let code = serve_loop(
        Cursor::new(String::from(input)),
        output.clone(),
        ServeConfig::minimal(),
        Policy::default(),
        None,
        || {},
    );
    assert_eq!(code, 0, "EOF 应优雅退出");
    let lines = output_json(&output);
    assert_eq!(
        lines[0].get("event").and_then(Json::as_str),
        Some("ready"),
        "ready 应是第一行"
    );
    assert!(
        lines
            .iter()
            .any(|line| line.get("id").and_then(Json::as_i64) == Some(1)
                && line.get("ok").and_then(Json::as_bool) == Some(true)),
        "status 应有成功响应：{lines:?}"
    );
    assert!(
        lines
            .iter()
            .any(|line| line.get("event").and_then(Json::as_str) == Some("invalid_json")),
        "畸形行应回 invalid_json 通知：{lines:?}"
    );
    let shutdown = lines
        .iter()
        .find(|line| line.get("id").and_then(Json::as_i64) == Some(2))
        .expect("shutdown 应有响应");
    assert_eq!(shutdown.get("ok").and_then(Json::as_bool), Some(true));
    assert!(
        !lines
            .iter()
            .any(|line| line.get("id").and_then(Json::as_i64) == Some(3)),
        "shutdown 之后的请求不应再被处理：{lines:?}"
    );
}

// ---- worker_online 通知（真实 poll 线程 + 可阻塞输入）----

/// 可阻塞的行 reader：无数据且未关闭时等待（模拟真实 stdin）。
struct ChannelReader {
    state: Arc<Mutex<ReaderState>>,
    buf: Vec<u8>,
    pos: usize,
}

struct ReaderState {
    lines: VecDeque<Vec<u8>>,
    closed: bool,
}

impl ChannelReader {
    fn new(state: Arc<Mutex<ReaderState>>) -> Self {
        Self {
            state,
            buf: Vec::new(),
            pos: 0,
        }
    }
}

impl BufRead for ChannelReader {
    fn fill_buf(&mut self) -> IoResult<&[u8]> {
        while self.pos >= self.buf.len() {
            let next = {
                let mut state = self.state.lock().unwrap();
                state.lines.pop_front()
            };
            match next {
                Some(line) => {
                    self.buf = line;
                    self.buf.push(b'\n');
                    self.pos = 0;
                }
                None => {
                    if self.state.lock().unwrap().closed {
                        self.buf.clear();
                        self.pos = 0;
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
        }
        Ok(&self.buf[self.pos..])
    }

    fn consume(&mut self, amt: usize) {
        self.pos += amt;
    }
}

impl Read for ChannelReader {
    fn read(&mut self, buf: &mut [u8]) -> IoResult<usize> {
        let internal = self.fill_buf()?;
        let count = internal.len().min(buf.len());
        buf[..count].copy_from_slice(&internal[..count]);
        self.consume(count);
        Ok(count)
    }
}

/// 轮询共享输出直到出现 event 通知（超时 panic，附完整输出便于诊断）。
fn wait_for_event(output: &SharedOutput, event: &str, timeout_ms: u64) -> Json {
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    loop {
        for line in output_json(output) {
            if line.get("event").and_then(Json::as_str) == Some(event) {
                return line;
            }
        }
        if Instant::now() >= deadline {
            panic!("等待 {event} 通知超时，输出：{}", output_text(output));
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// 注入网关（初始无 Worker）→ 外部把 Worker 加进链路 → poll 线程在
/// ~200ms 内发出 worker_online 通知（带完整 Worker 快照）。
#[test]
fn serve_loop_emits_worker_online_notification() {
    let reader_state = Arc::new(Mutex::new(ReaderState {
        lines: VecDeque::new(),
        closed: false,
    }));
    let output = SharedOutput::default();

    let workers = Arc::new(Mutex::new(Vec::<WorkerInfo>::new()));
    let calls = Arc::new(Mutex::new(Vec::new()));
    let injected = Some(InjectedLink {
        handle: fake_handle(Arc::clone(&workers), &calls),
        mode: "tcp",
        desc: String::from("测试链路（注入）"),
    });

    let thread_reader = Arc::clone(&reader_state);
    let thread_output = output.clone();
    let exit_code = Arc::new(Mutex::new(None));
    let thread_exit = Arc::clone(&exit_code);
    let server = std::thread::spawn(move || {
        let code = serve_loop(
            ChannelReader::new(thread_reader),
            thread_output,
            ServeConfig::minimal(),
            Policy::default(),
            injected,
            || {},
        );
        *thread_exit.lock().unwrap() = Some(code);
    });

    // ready 先到；随后 Worker 上线（首个 poll tick 200ms 内）。
    wait_for_event(&output, "ready", 2_000);
    workers.lock().unwrap().push(worker("pc-1"));
    let online = wait_for_event(&output, "worker_online", 3_000);
    assert_eq!(online.get("worker_id").and_then(Json::as_str), Some("pc-1"));
    let worker_object = online.get("worker").unwrap();
    assert_eq!(
        worker_object.get("worker_id").and_then(Json::as_str),
        Some("pc-1")
    );
    assert!(worker_object.get("tools").is_some());

    // 关闭输入 → EOF → serve_loop 返回 0，线程汇合。
    reader_state.lock().unwrap().closed = true;
    server.join().expect("serve 线程不应 panic");
    assert_eq!(*exit_code.lock().unwrap(), Some(0), "EOF 优雅退出 0");
}
