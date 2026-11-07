//! serve 无头 JSONL 服务模式测试：内存网关注入 + 协议往返断言。
//!
//! 覆盖任务卡所列场景：协议往返（link→workers→tool_call→policy.set→
//! 策略拒绝 policy_denied）、未知方法 unknown_method、畸形 JSON 行跳行回错
//! 不崩、EOF / shutdown 优雅退出 exit 0、worker_online 通知。

use std::collections::VecDeque;
use std::io::{BufRead, Cursor, Read, Result as IoResult, Write};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use sekai::sekai_device_list::DeviceList;
use sekai_router::agent::AgentConfig;
use sekai_router::config::DEFAULT_MAX_STEPS;
use sekai_router::control::{CommandKind, CommandRequest, CommandResult};
use sekai_router::gateway::{CommandReceipt, GatewayError, GatewayHandle, WorkerGateway};
use sekai_router::http::{HttpClient, HttpError, HttpRequest, HttpResponse};
use sekai_router::json::Json;
use sekai_router::llm::{LlmClient, LlmConfig};
use sekai_router::prompt_server::PromptCore;
use sekai_router::serve::{
    InjectedLink, ServeConfig, ServeState, serve_loop, worker_notifications,
};
use sekai_router::tools::Policy;
use sekai_router::trace::TraceWriter;
use sekai_router::worker::WorkerInfo;

// ---- 测试替身 ----

/// 可从外部改 Worker 集合、可记录命令请求/取消的内存网关。
struct FakeGateway {
    workers: Arc<Mutex<Vec<WorkerInfo>>>,
    calls: Arc<Mutex<Vec<CommandRequest>>>,
    /// cancel 收到的 cmd_id（B2）。
    cancelled: Arc<Mutex<Vec<u64>>>,
}

impl WorkerGateway for FakeGateway {
    fn list_workers(&mut self) -> Vec<WorkerInfo> {
        self.workers.lock().unwrap().clone()
    }

    fn submit(&mut self, request: CommandRequest) -> Result<CommandReceipt, GatewayError> {
        self.calls.lock().unwrap().push(request.clone());
        let result = Ok(CommandResult {
            cmd_id: 7,
            worker_id: request.worker_id,
            kind: request.kind,
            name: request.name,
            payload: b"ok".to_vec(),
            phases: Vec::new(),
            elapsed_ms: 3,
            failure: None,
        });
        let (sender, receiver) = std::sync::mpsc::channel();
        let _ = sender.send(result);
        // 回执带 cmd_id=7（与 CommandResult.cmd_id 一致；取消/账本断言依赖它）。
        Ok(CommandReceipt::with_cmd_id(receiver, 60_000, "内存", 7))
    }

    fn cancel(&mut self, cmd_id: u64) -> Result<(), GatewayError> {
        self.cancelled.lock().unwrap().push(cmd_id);
        Ok(())
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

    fn submit(&mut self, _request: CommandRequest) -> Result<CommandReceipt, GatewayError> {
        let (sender, receiver) = std::sync::mpsc::channel();
        let _ = sender.send(Err(GatewayError::new(
            "worker_offline",
            "测试：Worker 已离线",
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
    fake_handle_with_cancel(workers, calls).0
}

/// 同 [`fake_handle`]，额外返回 cancel 记录的 cmd_id（B2 断言用）。
fn fake_handle_with_cancel(
    workers: Arc<Mutex<Vec<WorkerInfo>>>,
    calls: &Arc<Mutex<Vec<CommandRequest>>>,
) -> (GatewayHandle, Arc<Mutex<Vec<u64>>>) {
    let cancelled = Arc::new(Mutex::new(Vec::new()));
    let gateway = FakeGateway {
        workers,
        calls: Arc::clone(calls),
        cancelled: Arc::clone(&cancelled),
    };
    let handle: GatewayHandle = Arc::new(Mutex::new(Box::new(gateway) as Box<dyn WorkerGateway>));
    (handle, cancelled)
}

/// 注入内存网关（`pc-1`）的 state + 请求/取消记录。
fn linked_state() -> (
    ServeState,
    Arc<Mutex<Vec<CommandRequest>>>,
    Arc<Mutex<Vec<u64>>>,
) {
    let mut state = state();
    let workers = Arc::new(Mutex::new(vec![worker("pc-1")]));
    let calls = Arc::new(Mutex::new(Vec::new()));
    let (handle, cancelled) = fake_handle_with_cancel(Arc::clone(&workers), &calls);
    state.set_gateway(handle, "tcp", String::from("测试链路（注入）"));
    (state, calls, cancelled)
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

// ---- prompt 方法（v1.1 additive）----

/// 固定返回一个 chat completions 响应体的脚本化 HTTP 传输（离线）。
struct ScriptedHttp {
    body: String,
}

impl HttpClient for ScriptedHttp {
    fn execute(&mut self, _request: &HttpRequest) -> Result<HttpResponse, HttpError> {
        Ok(HttpResponse {
            status: 200,
            headers: Vec::new(),
            body: self.body.clone().into_bytes(),
        })
    }
}

/// prompt 成功路径用的 hub（mock HTTP 支撑的 LlmClient）。
fn prompt_hub_with_chat_body(body: &str) -> Arc<PromptCore<LlmClient>> {
    let llm = LlmClient::new(
        Box::new(ScriptedHttp {
            body: String::from(body),
        }),
        LlmConfig::new("http://127.0.0.1:9", "deepseek-flash", "sk-test"),
    );
    Arc::new(PromptCore::new(
        llm,
        Policy::default(),
        AgentConfig {
            model: String::from("deepseek-flash"),
            max_steps: Some(4),
            ddl_ms: 1_000,
            timeout_ms: None,
            temperature: 0.0,
            max_tokens: 4_096,
        },
        TraceWriter::disabled(),
    ))
}

/// prompt 成功：注入 hub + 网关 → 完整 result JSON（answer/status/steps/usage）。
#[test]
fn prompt_method_runs_agent_and_returns_result() {
    let chat_body = r#"{"id":"chatcmpl-1","model":"deepseek-flash","choices":[{"index":0,"message":{"role":"assistant","content":"prompt 回答"},"finish_reason":"stop"}],"usage":{"prompt_tokens":9,"completion_tokens":4,"total_tokens":13}}"#;
    let mut state = state();
    state.set_prompt_hub(prompt_hub_with_chat_body(chat_body));
    let workers = Arc::new(Mutex::new(vec![worker("pc-1")]));
    let calls = Arc::new(Mutex::new(Vec::new()));
    state.set_gateway(
        fake_handle(workers, &calls),
        "tcp",
        String::from("测试链路（注入）"),
    );

    let messages = request(
        &mut state,
        r#"{"id":1,"method":"prompt","params":{"text":"你好","max_steps":4}}"#,
    );
    assert_eq!(messages.len(), 1, "{messages:?}");
    assert_eq!(messages[0].get("id").and_then(Json::as_i64), Some(1));
    assert_eq!(messages[0].get("ok").and_then(Json::as_bool), Some(true));
    let result = messages[0].get("result").unwrap();
    assert_eq!(
        result.get("answer").and_then(Json::as_str),
        Some("prompt 回答")
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
        Some(9)
    );
    assert_eq!(
        result
            .get_path(&["usage", "total_tokens"])
            .and_then(Json::as_u64),
        Some(13)
    );
    // prompt 不产生 Worker 命令。
    assert!(calls.lock().unwrap().is_empty());
}

/// 无 API key：惰性构造 hub → `llm_unavailable`；未建链先报 `not_linked`。
#[test]
fn prompt_method_without_key_or_link_reports_errors() {
    let mut state = state(); // minimal：api_key None。
    // 未建链：not_linked。
    let messages = request(
        &mut state,
        r#"{"id":1,"method":"prompt","params":{"text":"hi"}}"#,
    );
    let error = messages[0].get("error").unwrap();
    assert_eq!(error.get("code").and_then(Json::as_str), Some("not_linked"));

    // 建链 + Worker 就绪：llm_unavailable（无 API key）。
    let workers = Arc::new(Mutex::new(vec![worker("pc-1")]));
    let calls = Arc::new(Mutex::new(Vec::new()));
    state.set_gateway(
        fake_handle(workers, &calls),
        "tcp",
        String::from("测试链路（注入）"),
    );
    let messages = request(
        &mut state,
        r#"{"id":2,"method":"prompt","params":{"text":"hi"}}"#,
    );
    let error = messages[0].get("error").unwrap();
    assert_eq!(
        error.get("code").and_then(Json::as_str),
        Some("llm_unavailable")
    );
    assert!(
        error
            .get("message")
            .and_then(Json::as_str)
            .unwrap()
            .contains("API key")
    );
}

/// 参数校验：text 空/缺失/超限、max_steps 越界/类型错、未知字段 →
/// `invalid_arguments`（先于链路与 LLM 检查）。
#[test]
fn prompt_method_validates_arguments_strictly() {
    let mut state = state();
    for (line, needle) in [
        (
            r#"{"id":1,"method":"prompt","params":{}}"#,
            "缺少必填参数 text",
        ),
        (
            r#"{"id":2,"method":"prompt","params":{"text":""}}"#,
            "不能为空",
        ),
        (
            r#"{"id":3,"method":"prompt","params":{"text":7}}"#,
            "必须是字符串",
        ),
        (
            r#"{"id":4,"method":"prompt","params":{"text":"hi","max_steps":0}}"#,
            "max_steps",
        ),
        (
            r#"{"id":5,"method":"prompt","params":{"text":"hi","max_steps":33}}"#,
            "max_steps",
        ),
        (
            r#"{"id":6,"method":"prompt","params":{"text":"hi","extra":1}}"#,
            "未知参数",
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
    // text 超 8192 字节。
    let over = format!(
        r#"{{"id":7,"method":"prompt","params":{{"text":"{}"}}}}"#,
        "a".repeat(8_193)
    );
    let messages = request(&mut state, &over);
    let error = messages[0].get("error").unwrap();
    assert_eq!(
        error.get("code").and_then(Json::as_str),
        Some("invalid_arguments")
    );
}

/// hub 跟随 serve 状态：建链后再注入 hub，prompt 也能看到网关
/// （sync_prompt_hub 在注入时同步）。
#[test]
fn prompt_hub_sees_gateway_injected_after_hub() {
    let chat_body = r#"{"id":"chatcmpl-2","model":"deepseek-flash","choices":[{"index":0,"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}"#;
    let mut state = state();
    let workers = Arc::new(Mutex::new(vec![worker("pc-1")]));
    let calls = Arc::new(Mutex::new(Vec::new()));
    state.set_gateway(
        fake_handle(workers, &calls),
        "tcp",
        String::from("测试链路（注入）"),
    );
    // 先建链、后注入 hub：注入时同步网关。
    state.set_prompt_hub(prompt_hub_with_chat_body(chat_body));
    let messages = request(
        &mut state,
        r#"{"id":1,"method":"prompt","params":{"text":"hi"}}"#,
    );
    assert_eq!(messages[0].get("ok").and_then(Json::as_bool), Some(true));
    assert_eq!(
        messages[0]
            .get_path(&["result", "answer"])
            .and_then(Json::as_str),
        Some("ok")
    );
}

// ---- v1.2：异步 / 取消 / commands / jump / admin / 诊断（B2/B6/B7/B8）----

/// 参数：`async:true` 时立即回 `{accepted,cmd_id}`，完成后发无 id 的
/// `command_result` 通知；在途期间 `commands` 能看到该命令。
#[test]
fn async_tool_call_replies_accepted_then_notifies() {
    let (mut state, calls, _cancelled) = linked_state();

    let (messages, exit, deferred) = state.step(
        r#"{"id":1,"method":"tool_call","params":{"worker_id":"pc-1","tool":"gpio.info","args":"IO4","async":true}}"#,
    );
    assert!(!exit);
    assert_eq!(messages.len(), 1, "{messages:?}");
    let result = messages[0].get("result").unwrap();
    assert_eq!(result.get("accepted").and_then(Json::as_bool), Some(true));
    assert_eq!(result.get("cmd_id").and_then(Json::as_u64), Some(7));
    assert_eq!(result.get("async").and_then(Json::as_bool), Some(true));
    assert_eq!(result.get("kind").and_then(Json::as_str), Some("tool"));
    assert_eq!(calls.lock().unwrap().len(), 1, "dispatcher 内应已完成提交");

    // 完成前：commands 以标准信封列出在途命令。
    let messages = request(&mut state, r#"{"id":2,"method":"commands","params":{}}"#);
    assert_eq!(messages[0].get("id").and_then(Json::as_i64), Some(2));
    assert_eq!(messages[0].get("ok").and_then(Json::as_bool), Some(true));
    let listed = messages[0].get("result").unwrap();
    assert_eq!(listed.get("count").and_then(Json::as_u64), Some(1));
    let Json::Array(commands) = listed.get("commands").unwrap() else {
        panic!("commands 应是数组");
    };
    assert_eq!(commands[0].get("cmd_id").and_then(Json::as_u64), Some(7));
    assert_eq!(commands[0].get("async").and_then(Json::as_bool), Some(true));

    // 后台完成后：无 id 的 command_result 通知，账本清空。
    let notifications = deferred.expect("async 应拆出后台任务").run();
    assert_eq!(notifications.len(), 1);
    assert!(notifications[0].get("id").is_none(), "通知行不得带 id");
    assert_eq!(
        notifications[0].get("method").and_then(Json::as_str),
        Some("command_result")
    );
    let params = notifications[0].get("params").unwrap();
    assert_eq!(params.get("cmd_id").and_then(Json::as_u64), Some(7));
    assert_eq!(params.get("ok").and_then(Json::as_bool), Some(true));
    let messages = request(&mut state, r#"{"id":3,"method":"commands","params":{}}"#);
    assert_eq!(
        messages[0]
            .get("result")
            .unwrap()
            .get("count")
            .and_then(Json::as_u64),
        Some(0)
    );
}

/// `cancel {cmd_id}`：转发到网关并标记账本（known=true）；非法参数回错。
#[test]
fn cancel_routes_to_gateway_and_marks_known() {
    let (mut state, _calls, cancelled) = linked_state();
    let (_messages, _exit, deferred) = state.step(
        r#"{"id":1,"method":"tool_call","params":{"worker_id":"pc-1","tool":"gpio.info","args":"IO4","async":true}}"#,
    );

    let messages = request(
        &mut state,
        r#"{"id":2,"method":"cancel","params":{"cmd_id":7}}"#,
    );
    let result = messages[0].get("result").unwrap();
    assert_eq!(result.get("cmd_id").and_then(Json::as_u64), Some(7));
    assert_eq!(result.get("cancelled").and_then(Json::as_bool), Some(true));
    assert_eq!(result.get("known").and_then(Json::as_bool), Some(true));
    assert_eq!(*cancelled.lock().unwrap(), vec![7]);

    let messages = request(
        &mut state,
        r#"{"id":3,"method":"cancel","params":{"cmd_id":0}}"#,
    );
    assert_eq!(
        messages[0]
            .get("error")
            .unwrap()
            .get("code")
            .and_then(Json::as_str),
        Some("invalid_arguments")
    );

    // 收尾后台任务：通知里带 cancelled 标记。
    let notifications = deferred.expect("应有后台任务").run();
    assert_eq!(
        notifications[0]
            .get("params")
            .unwrap()
            .get("cancelled")
            .and_then(Json::as_bool),
        Some(true)
    );
}

/// jump：受 `--allow-admin`（policy.allow_admin）约束，按 `CommandKind::Jump`
/// 下发；未授权时 policy_denied 且不发命令。
#[test]
fn jump_requires_admin_and_dispatches_jump_kind() {
    let (mut state, calls, _cancelled) = linked_state();

    let messages = request(
        &mut state,
        r#"{"id":1,"method":"jump","params":{"worker_id":"pc-1","target":"peer-1"}}"#,
    );
    let error = messages[0].get("error").unwrap();
    assert_eq!(
        error.get("code").and_then(Json::as_str),
        Some("policy_denied")
    );
    assert!(calls.lock().unwrap().is_empty(), "未授权不应发命令");

    let _ = request(
        &mut state,
        r#"{"id":2,"method":"policy.set","params":{"allow_admin":true}}"#,
    );
    let messages = request(
        &mut state,
        r#"{"id":3,"method":"jump","params":{"worker_id":"pc-1","target":"peer-1"}}"#,
    );
    let result = messages[0].get("result").unwrap();
    assert_eq!(result.get("ok").and_then(Json::as_bool), Some(true));
    assert_eq!(result.get("kind").and_then(Json::as_str), Some("jump"));
    assert_eq!(result.get("name").and_then(Json::as_str), Some("peer-1"));
    let sent = calls.lock().unwrap().clone();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].kind, CommandKind::Jump);
    assert_eq!(sent[0].name, "peer-1");

    // 缺 target：invalid_arguments。
    let messages = request(
        &mut state,
        r#"{"id":4,"method":"jump","params":{"worker_id":"pc-1"}}"#,
    );
    assert_eq!(
        messages[0]
            .get("error")
            .unwrap()
            .get("code")
            .and_then(Json::as_str),
        Some("invalid_arguments")
    );
}

/// admin：set_time 编码 8 字节大端、workers.set 编码 DeviceList、cancel
/// 走取消路径；未授权时全部 policy_denied。
#[test]
fn admin_methods_encode_payloads() {
    let (mut state, calls, cancelled) = linked_state();

    let messages = request(
        &mut state,
        r#"{"id":1,"method":"admin","params":{"op":"set_time","worker_id":"pc-1","unix_ms":123}}"#,
    );
    assert_eq!(
        messages[0]
            .get("error")
            .unwrap()
            .get("code")
            .and_then(Json::as_str),
        Some("policy_denied")
    );

    let _ = request(
        &mut state,
        r#"{"id":2,"method":"policy.set","params":{"allow_admin":true}}"#,
    );
    let messages = request(
        &mut state,
        r#"{"id":3,"method":"admin","params":{"op":"set_time","worker_id":"pc-1","unix_ms":123}}"#,
    );
    let result = messages[0].get("result").unwrap();
    assert_eq!(result.get("op").and_then(Json::as_str), Some("set_time"));
    assert_eq!(result.get("cmd_id").and_then(Json::as_u64), Some(7));
    let sent = calls.lock().unwrap().clone();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].name, "sekai.set_time");
    assert_eq!(sent[0].args, 123u64.to_be_bytes().to_vec());

    let messages = request(
        &mut state,
        r#"{"id":4,"method":"admin","params":{"op":"workers.set","worker_id":"pc-1","workers":[{"name":"peer-1","ip":"127.0.0.1","port":7422}]}}"#,
    );
    let result = messages[0].get("result").unwrap();
    assert_eq!(result.get("op").and_then(Json::as_str), Some("workers.set"));
    let sent = calls.lock().unwrap().clone();
    let encoded = sent.last().expect("应有 workers.set 请求").args.clone();
    let list = DeviceList::decode(&encoded).expect("DeviceList 编码可解码");
    let device = list.get("peer-1").expect("应含 peer-1");
    assert_eq!(device.ip, "127.0.0.1");
    assert_eq!(device.port, 7422);

    // admin op=cancel 复用取消路径（账本无该 id → known=false）。
    let messages = request(
        &mut state,
        r#"{"id":5,"method":"admin","params":{"op":"cancel","cmd_id":42}}"#,
    );
    let result = messages[0].get("result").unwrap();
    assert_eq!(result.get("cancelled").and_then(Json::as_bool), Some(true));
    assert_eq!(result.get("known").and_then(Json::as_bool), Some(false));
    assert_eq!(*cancelled.lock().unwrap(), vec![42]);
}

/// status：诊断字段（link_info / inflight / counters / clock_synced /
/// late_results）齐全且来自控制面诊断。
#[test]
fn status_exposes_diagnostics_fields() {
    let (mut state, _calls, _cancelled) = linked_state();
    let messages = request(&mut state, r#"{"id":1,"method":"status","params":{}}"#);
    assert_eq!(messages[0].get("ok").and_then(Json::as_bool), Some(true));
    let result = messages[0].get("result").unwrap();
    assert_eq!(
        result.get("link").and_then(Json::as_str),
        Some("测试链路（注入）")
    );
    let link_info = result.get("link_info").unwrap();
    assert_eq!(link_info.get("state").and_then(Json::as_str), Some("ready"));
    assert_eq!(link_info.get("workers").and_then(Json::as_u64), Some(1));
    let Json::Array(inflight) = result.get("inflight").unwrap() else {
        panic!("inflight 应是数组");
    };
    assert!(inflight.is_empty());
    let counters = result.get("counters").unwrap();
    assert_eq!(counters.get("late_results").and_then(Json::as_u64), Some(0));
    assert_eq!(
        counters.get("dropped_events").and_then(Json::as_u64),
        Some(0)
    );
    assert_eq!(
        result.get("clock_synced").and_then(Json::as_bool),
        Some(false)
    );
    let Json::Array(late) = result.get("late_results").unwrap() else {
        panic!("late_results 应是数组");
    };
    assert!(late.is_empty());
}

/// D5 自动路由：名字在 Worker 通告的 tasks 列表里时，`tool_call` 按 Task 下发
/// （策略仍按名字分类，需要 allow_tasks）。
#[test]
fn tool_call_routes_advertised_task_names_as_task() {
    let (mut state, calls, _cancelled) = linked_state();
    // 默认拒绝 Task：echo 已在 Worker tasks 列表 → 按 Task 分类被拒。
    let messages = request(
        &mut state,
        r#"{"id":1,"method":"tool_call","params":{"worker_id":"pc-1","tool":"echo","args":"hi"}}"#,
    );
    let denied = messages[0].get("result").unwrap();
    assert_eq!(
        denied
            .get("error")
            .unwrap()
            .get("code")
            .and_then(Json::as_str),
        Some("policy_denied")
    );
    assert_eq!(denied.get("kind").and_then(Json::as_str), Some("tool"));
    assert!(calls.lock().unwrap().is_empty(), "策略拒绝不应发命令");

    let _ = request(
        &mut state,
        r#"{"id":2,"method":"policy.set","params":{"allow_tasks":true}}"#,
    );
    let messages = request(
        &mut state,
        r#"{"id":3,"method":"tool_call","params":{"worker_id":"pc-1","tool":"echo","args":"hi"}}"#,
    );
    let result = messages[0].get("result").unwrap();
    assert_eq!(result.get("ok").and_then(Json::as_bool), Some(true));
    assert_eq!(result.get("kind").and_then(Json::as_str), Some("task"));
    let sent = calls.lock().unwrap().clone();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].kind, CommandKind::Task);

    // 未通告的普通工具仍按 Tool 下发。
    let messages = request(
        &mut state,
        r#"{"id":4,"method":"tool_call","params":{"worker_id":"pc-1","tool":"gpio.read","args":"IO4"}}"#,
    );
    assert_eq!(
        messages[0]
            .get("result")
            .unwrap()
            .get("kind")
            .and_then(Json::as_str),
        Some("tool")
    );
}

/// 步数上限随运行期链路取值：未显式 `--max-steps` 时，直连 PC Worker（tcp）→
/// 不设上限，MQTT（嵌入式）→ 默认 12（`ServeConfig::max_steps_auto`）。
#[test]
fn max_steps_follows_runtime_link_when_not_explicit() {
    let mut config = ServeConfig::minimal();
    config.max_steps = Some(DEFAULT_MAX_STEPS);
    config.max_steps_auto = true;
    let mut state = ServeState::new(config, Policy::default());
    let hub = prompt_hub_with_chat_body("{}");
    hub.set_max_steps(Some(DEFAULT_MAX_STEPS));
    state.set_prompt_hub(hub.clone());
    assert_eq!(
        hub.max_steps(),
        Some(DEFAULT_MAX_STEPS),
        "未建链时保持配置值"
    );

    // 直连 PC Worker：不设上限。
    let workers = Arc::new(Mutex::new(vec![worker("pc-1")]));
    let calls = Arc::new(Mutex::new(Vec::new()));
    state.set_gateway(
        fake_handle(workers, &calls),
        "tcp",
        String::from("测试链路（注入）"),
    );
    assert_eq!(hub.max_steps(), None, "tcp 直连（PC Worker）应不设上限");

    // 切到 MQTT（非 PC Worker）：回到默认 12。
    let workers = Arc::new(Mutex::new(vec![worker("pc-1")]));
    let calls = Arc::new(Mutex::new(Vec::new()));
    state.set_gateway(
        fake_handle(workers, &calls),
        "mqtt",
        String::from("测试链路（注入）"),
    );
    assert_eq!(
        hub.max_steps(),
        Some(DEFAULT_MAX_STEPS),
        "MQTT（非 PC Worker）保留原上限"
    );
}

/// 显式指定步数上限时，运行期链路变化不再改写（`max_steps_auto=false`）。
#[test]
fn explicit_max_steps_survives_runtime_link_change() {
    let mut config = ServeConfig::minimal();
    config.max_steps = Some(3);
    config.max_steps_auto = false;
    let mut state = ServeState::new(config, Policy::default());
    let hub = prompt_hub_with_chat_body("{}");
    hub.set_max_steps(Some(3));
    state.set_prompt_hub(hub.clone());

    let workers = Arc::new(Mutex::new(vec![worker("pc-1")]));
    let calls = Arc::new(Mutex::new(Vec::new()));
    state.set_gateway(
        fake_handle(workers, &calls),
        "tcp",
        String::from("测试链路（注入）"),
    );
    assert_eq!(hub.max_steps(), Some(3), "显式上限不随链路变化");
}
