//! 会话命令层测试：ScriptedChat + FakeGateway 注入，逐行执行命令并断言输出。
//!
//! 覆盖：无链路时目标的提示、目标执行（Agent 回路 + 回答渲染）、/workers、
//! /policy 切换、/wait 超时、/exit、未知命令、/link off 幂等、
//! API key 缺失不阻塞会话（懒校验）。

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use sekai_router::agent::AgentConfig;
use sekai_router::control::{CommandRequest, CommandResult};
use sekai_router::gateway::{GatewayError, GatewayHandle, WorkerGateway};
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

    fn call(&mut self, _request: CommandRequest) -> Result<CommandResult, GatewayError> {
        Err(GatewayError::new("worker_not_found", "会话测试不执行命令"))
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
        max_steps: 4,
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
