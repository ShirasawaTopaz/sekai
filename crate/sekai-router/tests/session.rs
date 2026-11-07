//! 会话命令层测试：ScriptedChat + FakeGateway 注入，逐行执行命令并断言输出。
//!
//! 覆盖：无链路时目标的提示、目标执行（Agent 回路 + 回答渲染）、/workers、
//! /policy 切换、/wait 超时、/exit、未知命令、/link off 幂等、
//! API key 缺失不阻塞会话（懒校验）。

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use sekai_router::agent::AgentConfig;
use sekai_router::control::CommandRequest;
use sekai_router::gateway::{CommandReceipt, GatewayError, GatewayHandle, WorkerGateway};
use sekai_router::llm::{ChatClient, ChatRequest, ChatResponse, LlmError, Usage};
use sekai_router::session::{Session, SessionConfig, SessionMessage};
use sekai_router::tools::Policy;
use sekai_router::trace::TraceWriter;
use sekai_router::worker::WorkerInfo;

// ---- 测试替身 ----

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

struct FakeGateway {
    workers: Vec<WorkerInfo>,
}

impl WorkerGateway for FakeGateway {
    fn list_workers(&mut self) -> Vec<WorkerInfo> {
        self.workers.clone()
    }

    fn submit(&mut self, _request: CommandRequest) -> Result<CommandReceipt, GatewayError> {
        let (sender, receiver) = std::sync::mpsc::channel();
        let _ = sender.send(Err(GatewayError::new(
            "worker_not_found",
            "会话测试不执行命令",
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

fn worker(worker_id: &str, online: bool) -> WorkerInfo {
    WorkerInfo {
        worker_id: String::from(worker_id),
        online,
        boot_id: 11,
        firmware: String::from("0.1.0"),
        uptime_ms: 100,
        unix_ms: 1_700_000_000_000,
        tools: vec![String::from("gpio.info")],
        tasks: vec![String::from("echo")],
        workers: Vec::new(),
        active_tasks: Vec::new(),
        last_seen_mono_ms: 5,
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
            prompt_tokens: 10,
            completion_tokens: 5,
            total_tokens: 15,
        },
    }
}

fn fake_handle(workers: Vec<WorkerInfo>) -> GatewayHandle {
    Arc::new(Mutex::new(
        Box::new(FakeGateway { workers }) as Box<dyn WorkerGateway>
    ))
}

fn session(chat: ScriptedChat) -> Session<ScriptedChat> {
    let policy = Policy::default();
    let agent_config = AgentConfig {
        model: String::from("deepseek-flash"),
        max_steps: Some(4),
        ddl_ms: 1_000,
        timeout_ms: Some(30_000),
        temperature: 0.0,
        max_tokens: 4_096,
    };
    Session::new(
        chat,
        policy,
        agent_config,
        TraceWriter::disabled(),
        SessionConfig::minimal(),
    )
}

fn lines(messages: &[SessionMessage]) -> Vec<String> {
    messages
        .iter()
        .map(|message| match message {
            SessionMessage::Line(text) => text.clone(),
            SessionMessage::Exit => String::from("<exit>"),
        })
        .collect()
}

// ---- 测试 ----

/// 无链路时：目标输入提示先建立链路；/link 显示未连接。
#[test]
fn goal_without_link_hints_to_connect() {
    let mut session = session(ScriptedChat::one(text_response("unused")));
    let out = lines(&session.execute("把 IO4 拉高"));
    assert_eq!(out.len(), 1);
    assert!(out[0].contains("未连接"));

    let out = lines(&session.execute("/link"));
    assert!(out.iter().any(|line| line.contains("未连接")));
}

/// 有链路与在线 Worker：目标走完整 Agent 回路，回答行 + 统计行。
#[test]
fn goal_with_gateway_runs_agent_loop() {
    let chat = ScriptedChat::one(text_response("IO4 已是高电平。"));
    let requests = chat.requests.clone();
    let mut session = session(chat);
    session.set_gateway(fake_handle(vec![worker("pc-1", true)]), "测试链路");

    let out = lines(&session.execute("读取 IO4"));
    assert!(out.iter().any(|line| line == "IO4 已是高电平。"));
    assert!(out.iter().any(|line| line.contains("步数=1")));
    // system prompt 应包含 Worker 快照。
    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    let system = format!("{:?}", requests[0].messages[0]);
    assert!(system.contains("pc-1"));
}

/// 已连接但没有在线 Worker：目标被拦下并提示 /wait。
#[test]
fn goal_without_online_workers_is_blocked() {
    let mut session = session(ScriptedChat::one(text_response("unused")));
    session.set_gateway(fake_handle(vec![]), "测试链路");
    let out = lines(&session.execute("做什么"));
    assert_eq!(out.len(), 1);
    assert!(out[0].contains("/wait"));
}

/// /workers 列出 Worker 快照。
#[test]
fn workers_command_lists_snapshot() {
    let mut session = session(ScriptedChat::one(text_response("unused")));
    session.set_gateway(fake_handle(vec![worker("pc-1", true)]), "测试链路");
    let out = lines(&session.execute("/workers"));
    assert!(
        out.iter()
            .any(|line| line.contains("pc-1") && line.contains("gpio.info"))
    );
}

/// /wait：无 Worker 时按窗口超时（用 60ms 小窗口）。
#[test]
fn wait_times_out_without_workers() {
    let mut session = session(ScriptedChat::one(text_response("unused")));
    session.set_gateway(fake_handle(vec![]), "测试链路");
    let out = lines(&session.execute("/wait 60"));
    assert!(out.iter().any(|line| line.contains("超时")), "out={out:?}");
}

/// /wait：有在线 Worker 立即返回其能力。
#[test]
fn wait_returns_online_worker() {
    let mut session = session(ScriptedChat::one(text_response("unused")));
    session.set_gateway(fake_handle(vec![worker("pc-9", true)]), "测试链路");
    let out = lines(&session.execute("/wait 5000"));
    assert!(
        out.iter()
            .any(|line| line.contains("pc-9") && line.contains("echo"))
    );
}

/// /policy 查看 + 切换。
#[test]
fn policy_show_and_toggle() {
    let mut session = session(ScriptedChat::one(text_response("unused")));
    let out = lines(&session.execute("/policy"));
    assert!(out[0].contains("allow_write=false"));

    session.execute("/policy allow-write on");
    session.execute("/policy allow-tasks on");
    let out = lines(&session.execute("/policy"));
    assert!(out[0].contains("allow_write=true"));
    assert!(out[0].contains("allow_tasks=true"));
    assert!(session.policy().allow_write);

    let out = lines(&session.execute("/policy nope on"));
    assert!(out[0].contains("未知策略"));
    let out = lines(&session.execute("/policy allow-write maybe"));
    assert!(out[0].contains("用法"));
}

/// /exit 与未知命令。
#[test]
fn exit_and_unknown_commands() {
    let mut session = session(ScriptedChat::one(text_response("unused")));
    assert_eq!(session.execute("/exit"), vec![SessionMessage::Exit]);
    assert_eq!(session.execute("/quit"), vec![SessionMessage::Exit]);
    let out = lines(&session.execute("/nope"));
    assert!(out[0].contains("未知命令"));
}

/// /link off：无链路时提示；有链路时关闭后回到未连接。
#[test]
fn link_off_is_idempotent() {
    let mut session = session(ScriptedChat::one(text_response("unused")));
    let out = lines(&session.execute("/link off"));
    assert!(out[0].contains("没有链路"));

    session.set_gateway(fake_handle(vec![worker("pc-1", true)]), "测试链路");
    let out = lines(&session.execute("/link off"));
    assert!(out[0].contains("已关闭"));
    assert_eq!(session.link_desc(), "未连接");
}

/// /help 输出包含全部命令。
#[test]
fn help_lists_commands() {
    let mut session = session(ScriptedChat::one(text_response("unused")));
    let out = lines(&session.execute("/help"));
    let text = out.join("\n");
    for command in [
        "/link tcp",
        "/link stdio",
        "/link mqtt",
        "/workers",
        "/wait",
        "/policy",
        "/exit",
    ] {
        assert!(text.contains(command), "help 缺少 {command}");
    }
}

/// 空输入与纯空白输入不产生输出。
#[test]
fn blank_input_is_ignored() {
    let mut session = session(ScriptedChat::one(text_response("unused")));
    assert!(session.execute("").is_empty());
    assert!(session.execute("   ").is_empty());
}

// ---- prompt 通道 hub 同步 ----

/// 会话把网关与策略同步进 prompt hub：/policy 变更即时可见；
/// /link tcp 后 hub 网关就位（run_prompt 从 not_linked 变为 no_workers）。
#[test]
fn session_syncs_policy_and_gateway_into_prompt_hub() {
    use sekai_router::prompt_server::{self, PromptCore};

    let mut session = session(ScriptedChat::one(text_response("unused")));
    let hub = Arc::new(PromptCore::new(
        ScriptedChat::one(text_response("unused")),
        Policy::default(),
        AgentConfig {
            model: String::from("deepseek-flash"),
            max_steps: Some(1),
            ddl_ms: 1_000,
            timeout_ms: None,
            temperature: 0.0,
            max_tokens: 4_096,
        },
        TraceWriter::disabled(),
    ));
    session.set_prompt_hub(Arc::clone(&hub));

    // 策略同步：/policy 切换后 hub 策略快照随之更新。
    assert!(!hub.policy().allow_write);
    session.execute("/policy allow-write on");
    assert!(hub.policy().allow_write);

    // 网关同步：未建链 → not_linked；/link tcp 后 → no_workers
    // （tcp 监听建立但无 Worker 连入，hub 已能看到网关）。
    let error = hub.run_prompt("hi", None).expect_err("未建链应报错");
    assert_eq!(error.code, prompt_server::CODE_NOT_LINKED);
    session.execute("/link tcp 127.0.0.1:0");
    let error = hub.run_prompt("hi", None).expect_err("无 Worker 应报错");
    assert_eq!(error.code, prompt_server::CODE_NO_WORKERS);

    // /link off 后回到 not_linked。
    session.execute("/link off");
    let error = hub.run_prompt("hi", None).expect_err("关链后应报错");
    assert_eq!(error.code, prompt_server::CODE_NOT_LINKED);

    // set_gateway 注入同样同步：注入带 Worker 的网关后可走到 LLM 并返回回答。
    session.set_gateway(fake_handle(vec![worker("pc-1", true)]), "测试链路");
    let result = hub.run_prompt("hi", None).expect("注入网关后应成功");
    assert_eq!(result.answer, "unused");
    assert_eq!(result.status, "completed");
}

// ---- B3：后台目标 + 事件通道；B2：/cancel ----

/// 前端 submit_line：'/' 命令同步执行，自然语言目标在后台线程跑，
/// poll 消费完成事件（回答 + 统计行），不阻塞提交线程。
#[test]
fn background_goal_runs_through_event_channel() {
    let chat = ScriptedChat::one(text_response("IO4 已拉高。"));
    let mut session = session(chat);
    session.set_gateway(fake_handle(vec![worker("pc-1", true)]), "测试链路");

    let start = lines(&session.submit_line("把 IO4 拉高"));
    assert!(
        start.iter().any(|line| line.contains("已开始执行")),
        "{start:?}"
    );
    assert!(session.goal_running(), "提交后应有后台目标");
    assert!(session.goal_elapsed_ms() <= 5_000);

    let deadline = Instant::now() + Duration::from_secs(5);
    let mut out = Vec::new();
    while session.goal_running() && Instant::now() < deadline {
        out.extend(lines(&session.poll()));
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(!session.goal_running(), "目标应已完成");
    assert!(
        out.iter().any(|line| line.contains("IO4 已拉高")),
        "poll 应交付回答行：{out:?}"
    );
    assert!(
        out.iter().any(|line| line.contains("步数=1")),
        "poll 应交付统计行：{out:?}"
    );
    // 再次 poll 为空（事件只交付一次）。
    assert!(session.poll().is_empty(), "完成事件不应重复交付");
}

/// '/' 命令仍同步执行：submit_line 对 /status 立即返回输出。
#[test]
fn submit_line_routes_slash_commands_synchronously() {
    let mut session = session(ScriptedChat::one(text_response("unused")));
    let out = lines(&session.submit_line("/status"));
    assert!(out.iter().any(|line| line.contains("链路：")), "{out:?}");
    assert!(!session.goal_running(), "斜杠命令不应启动后台目标");
}

/// /cancel：无目标时提示；非法参数报错；带 cmd_id 时经网关取消。
#[test]
fn cancel_command_handles_idle_and_worker_ids() {
    let mut session = session(ScriptedChat::one(text_response("unused")));
    let idle = lines(&session.execute("/cancel"));
    assert!(
        idle.iter()
            .any(|line| line.contains("没有在后台执行的目标")),
        "{idle:?}"
    );
    assert!(!session.cancel_goal(), "空闲时 cancel_goal 返回 false");

    let bad = lines(&session.execute("/cancel abc"));
    assert!(bad.iter().any(|line| line.contains("cmd_id")), "{bad:?}");

    session.set_gateway(fake_handle(vec![worker("pc-1", true)]), "测试链路");
    let out = lines(&session.execute("/cancel 42"));
    assert!(
        out.iter()
            .any(|line| line.contains("取消命令 42 失败") || line.contains("已请求取消命令 42")),
        "带 cmd_id 的取消应给出结果：{out:?}"
    );
}

/// /status 暴露 B8 诊断字段（最近错误 / 在途 / 时钟校准 / 晚到终态）。
#[test]
fn status_reports_diagnostics_snapshot() {
    let mut session = session(ScriptedChat::one(text_response("unused")));
    session.set_gateway(fake_handle(vec![worker("pc-1", true)]), "测试链路");
    let out = lines(&session.execute("/status")).join("\n");
    assert!(out.contains("时钟校准："), "{out}");
    assert!(out.contains("晚到终态：0 条"), "{out}");
}
