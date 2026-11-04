//! agent 循环离线测试：ScriptedChat 脚本化 LLM + FakeGateway 假Worker网关。
//!
//! 覆盖：单工具 -> 最终回答、多工具顺序执行、max steps、策略拒绝（不调用
//! gateway）、Worker过滤、未知Worker、Worker Failed、非法参数、未知工具、tool 消息
//! 回填、usage 汇总、trace JSONL。

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use sekai::sekai_stdout::Phase;
use sekai::sekai_wire::ErrorCode;

use sekai_router::agent::{Agent, AgentConfig, AgentError, AgentStatus};
use sekai_router::control::{CommandFailure, CommandKind, CommandRequest, CommandResult};
use sekai_router::gateway::{CommandReceipt, GatewayError, WorkerGateway};
use sekai_router::json::Json;
use sekai_router::llm::{
    ChatClient, ChatRequest, ChatResponse, LlmError, Message, Role, ToolCall, Usage,
};
use sekai_router::tools::Policy;
use sekai_router::trace::TraceWriter;
use sekai_router::worker::WorkerInfo;

// ---------------------------------------------------------------------------
// 测试替身
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct ScriptedChat {
    responses: VecDeque<Result<ChatResponse, LlmError>>,
    requests: Arc<Mutex<Vec<ChatRequest>>>,
}

impl ScriptedChat {
    fn new(
        responses: Vec<Result<ChatResponse, LlmError>>,
        requests: Arc<Mutex<Vec<ChatRequest>>>,
    ) -> Self {
        Self {
            responses: responses.into(),
            requests,
        }
    }
}

impl ChatClient for ScriptedChat {
    fn chat(&mut self, request: &ChatRequest) -> Result<ChatResponse, LlmError> {
        self.requests.lock().unwrap().push(request.clone());
        self.responses
            .pop_front()
            .unwrap_or_else(|| Err(LlmError::Protocol(String::from("scripted chat 已耗尽"))))
    }
}

struct FakeGateway {
    workers: Vec<WorkerInfo>,
    responses: VecDeque<Result<CommandResult, GatewayError>>,
    calls: Arc<Mutex<Vec<CommandRequest>>>,
}

impl FakeGateway {
    fn new(
        workers: Vec<WorkerInfo>,
        responses: Vec<Result<CommandResult, GatewayError>>,
        calls: Arc<Mutex<Vec<CommandRequest>>>,
    ) -> Self {
        Self {
            workers,
            responses: responses.into(),
            calls,
        }
    }
}

impl WorkerGateway for FakeGateway {
    fn list_workers(&mut self) -> Vec<WorkerInfo> {
        self.workers.clone()
    }

    fn submit(&mut self, request: CommandRequest) -> Result<CommandReceipt, GatewayError> {
        self.calls.lock().unwrap().push(request);
        let result = self
            .responses
            .pop_front()
            .unwrap_or_else(|| Err(GatewayError::new("worker_not_found", "没有脚本化响应")));
        let (sender, receiver) = std::sync::mpsc::channel();
        let _ = sender.send(result);
        Ok(CommandReceipt::new(receiver, 60_000, "内存"))
    }

    fn wait_ready(&mut self, _timeout_ms: u64) -> Result<(), GatewayError> {
        Ok(())
    }

    fn is_ready(&self) -> bool {
        true
    }
}

// ---------------------------------------------------------------------------
// 构造辅助
// ---------------------------------------------------------------------------

fn worker(worker_id: &str, online: bool) -> WorkerInfo {
    WorkerInfo {
        worker_id: String::from(worker_id),
        online,
        boot_id: 11,
        firmware: String::from("0.1.0"),
        uptime_ms: 100,
        unix_ms: 1_700_000_000_000,
        tools: vec![
            String::from("gpio.info"),
            String::from("gpio.read"),
            String::from("gpio.config"),
            String::from("gpio.write"),
            String::from("sekai.info"),
        ],
        tasks: vec![String::from("echo")],
        workers: Vec::new(),
        active_tasks: Vec::new(),
        last_seen_mono_ms: 5,
        hello_seq: 1,
    }
}

fn agent_config(max_steps: u32) -> AgentConfig {
    AgentConfig {
        model: String::from("deepseek-flash"),
        max_steps,
        ddl_ms: 1_000,
        timeout_ms: Some(30_000),
        temperature: 0.0,
        max_tokens: 4_096,
    }
}

fn usage(prompt: u64, completion: u64, total: u64) -> Usage {
    Usage {
        prompt_tokens: prompt,
        completion_tokens: completion,
        total_tokens: total,
    }
}

fn tool_call(id: &str, name: &str, arguments: &str) -> ToolCall {
    ToolCall {
        id: String::from(id),
        name: String::from(name),
        arguments: String::from(arguments),
    }
}

fn tool_response(calls: Vec<ToolCall>, usage: Usage) -> ChatResponse {
    ChatResponse {
        model: String::from("deepseek-flash"),
        content: None,
        tool_calls: calls,
        finish_reason: Some(String::from("tool_calls")),
        usage,
    }
}

fn text_response(text: &str, usage: Usage) -> ChatResponse {
    ChatResponse {
        model: String::from("deepseek-flash"),
        content: Some(String::from(text)),
        tool_calls: Vec::new(),
        finish_reason: Some(String::from("stop")),
        usage,
    }
}

fn ok_result(worker_id: &str, kind: CommandKind, name: &str, payload: &[u8]) -> CommandResult {
    CommandResult {
        cmd_id: 1_790_000_000_123,
        worker_id: String::from(worker_id),
        kind,
        name: String::from(name),
        payload: payload.to_vec(),
        phases: vec![Phase::Accepted, Phase::Completed],
        elapsed_ms: 8,
        failure: None,
    }
}

fn failed_result(worker_id: &str, kind: CommandKind, name: &str) -> CommandResult {
    CommandResult {
        cmd_id: 7,
        worker_id: String::from(worker_id),
        kind,
        name: String::from(name),
        payload: Vec::new(),
        phases: vec![Phase::Accepted, Phase::Failed],
        elapsed_ms: 3,
        failure: Some(CommandFailure::worker(
            ErrorCode::InvalidArgument,
            "引脚 IO6 已保留（SPI flash）",
        )),
    }
}

fn new_requests() -> Arc<Mutex<Vec<ChatRequest>>> {
    Arc::new(Mutex::new(Vec::new()))
}

fn new_calls() -> Arc<Mutex<Vec<CommandRequest>>> {
    Arc::new(Mutex::new(Vec::new()))
}

fn message_json(message: &Message) -> Json {
    Json::parse(message.content.as_deref().expect("tool 消息必须有 content"))
        .expect("tool 消息 content 必须是合法 JSON")
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[test]
fn single_tool_then_final_answer() {
    let requests = new_requests();
    let calls = new_calls();
    let chat = ScriptedChat::new(
        vec![
            Ok(tool_response(
                vec![tool_call("call_1", "list_workers", "{}")],
                usage(10, 5, 15),
            )),
            Ok(text_response("host-gpio 在线，已就绪。", usage(20, 7, 27))),
        ],
        requests.clone(),
    );
    let gateway = FakeGateway::new(vec![worker("host-gpio", true)], vec![], calls.clone());
    let mut agent = Agent::new(
        chat,
        gateway,
        Policy::default(),
        agent_config(4),
        TraceWriter::disabled(),
    );
    let outcome = agent.run("看看有哪些Worker").unwrap();

    assert_eq!(outcome.status, AgentStatus::Completed);
    assert_eq!(
        outcome.final_text.as_deref(),
        Some("host-gpio 在线，已就绪。")
    );
    assert_eq!(outcome.steps, 2);
    assert_eq!(outcome.usage, usage(30, 12, 42));
    assert_eq!(outcome.tool_calls.len(), 1);
    let record = &outcome.tool_calls[0];
    assert_eq!(record.function, "list_workers");
    assert!(record.ok);
    assert!(record.worker_id.is_none());
    assert!(record.error_code.is_none());
    assert!(!outcome.had_worker_failure);
    assert!(!outcome.last_tool_failed);
    assert!(
        calls.lock().unwrap().is_empty(),
        "list_workers 不应走 gateway.call"
    );

    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    let first = &requests[0];
    assert_eq!(first.tool_choice, sekai_router::llm::ToolChoice::Auto);
    assert_eq!(first.messages.len(), 2);
    assert_eq!(first.messages[0].role, Role::System);
    assert!(
        first.messages[0]
            .content
            .as_deref()
            .unwrap()
            .contains("Sekai Worker控制助手")
    );
    assert_eq!(first.messages[1].role, Role::User);
    let second = &requests[1];
    assert_eq!(second.messages.len(), 4);
    assert_eq!(second.messages[2].role, Role::Assistant);
    assert_eq!(second.messages[2].tool_calls.len(), 1);
    assert_eq!(second.messages[3].role, Role::Tool);
    assert_eq!(second.messages[3].tool_call_id.as_deref(), Some("call_1"));
    let result = message_json(&second.messages[3]);
    assert_eq!(result.get("count").and_then(Json::as_u64), Some(1));
    assert_eq!(
        result
            .get_path(&["workers", "0", "worker_id"])
            .and_then(Json::as_str),
        Some("host-gpio")
    );
    assert_eq!(
        result
            .get_path(&["workers", "0", "online"])
            .and_then(Json::as_bool),
        Some(true)
    );
}

#[test]
fn multiple_tool_calls_execute_in_order() {
    let requests = new_requests();
    let calls = new_calls();
    let chat = ScriptedChat::new(
        vec![
            Ok(tool_response(
                vec![
                    tool_call(
                        "call_a",
                        "worker_tool",
                        r#"{"worker_id":"host-gpio","tool":"gpio.info","args":"help"}"#,
                    ),
                    tool_call(
                        "call_b",
                        "worker_tool",
                        r#"{"worker_id":"host-gpio","tool":"gpio.read","args":"IO4"}"#,
                    ),
                ],
                usage(1, 1, 2),
            )),
            Ok(text_response("已读取 IO4。", usage(2, 2, 4))),
        ],
        requests.clone(),
    );
    let gateway = FakeGateway::new(
        vec![worker("host-gpio", true)],
        vec![
            Ok(ok_result(
                "host-gpio",
                CommandKind::Tool,
                "gpio.info",
                b"gpio tools v1",
            )),
            Ok(ok_result(
                "host-gpio",
                CommandKind::Tool,
                "gpio.read",
                b"pin=IO4 level=1",
            )),
        ],
        calls.clone(),
    );
    let mut agent = Agent::new(
        chat,
        gateway,
        Policy::default(),
        agent_config(4),
        TraceWriter::disabled(),
    );
    let outcome = agent.run("读一下 IO4").unwrap();
    assert_eq!(outcome.status, AgentStatus::Completed);
    assert_eq!(outcome.steps, 2);

    let calls = calls.lock().unwrap();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].name, "gpio.info");
    assert_eq!(calls[0].args, b"help");
    assert_eq!(calls[1].name, "gpio.read");
    assert_eq!(calls[1].args, b"IO4");
    assert_eq!(calls[0].kind, CommandKind::Tool);
    assert_eq!(calls[0].ddl_ms, 1_000);
    assert_eq!(calls[0].timeout_ms, Some(30_000));

    let requests = requests.lock().unwrap();
    let second = &requests[1];
    assert_eq!(second.messages[2].role, Role::Assistant);
    assert_eq!(second.messages[2].tool_calls.len(), 2);
    assert_eq!(second.messages[3].role, Role::Tool);
    assert_eq!(second.messages[3].tool_call_id.as_deref(), Some("call_a"));
    assert_eq!(second.messages[4].role, Role::Tool);
    assert_eq!(second.messages[4].tool_call_id.as_deref(), Some("call_b"));
    let first_result = message_json(&second.messages[3]);
    assert_eq!(
        first_result.get("payload").and_then(Json::as_str),
        Some("gpio tools v1")
    );
    let second_result = message_json(&second.messages[4]);
    assert_eq!(
        second_result.get("payload").and_then(Json::as_str),
        Some("pin=IO4 level=1")
    );
    assert_eq!(
        second_result
            .get("phases")
            .and_then(Json::as_array)
            .unwrap()
            .len(),
        2
    );
    assert_eq!(
        outcome
            .tool_calls
            .iter()
            .map(|record| record.name.clone().unwrap())
            .collect::<Vec<String>>(),
        vec!["gpio.info", "gpio.read"]
    );
}

#[test]
fn max_steps_stops_the_loop() {
    let requests = new_requests();
    let calls = new_calls();
    let scripted: Vec<Result<ChatResponse, LlmError>> = (0..2)
        .map(|index| {
            Ok(tool_response(
                vec![tool_call(&format!("call_{index}"), "list_workers", "{}")],
                usage(1, 1, 2),
            ))
        })
        .collect();
    let chat = ScriptedChat::new(scripted, requests.clone());
    let gateway = FakeGateway::new(vec![worker("host-gpio", true)], vec![], calls);
    let mut agent = Agent::new(
        chat,
        gateway,
        Policy::default(),
        agent_config(2),
        TraceWriter::disabled(),
    );
    let outcome = agent.run("一直列Worker").unwrap();
    assert_eq!(outcome.status, AgentStatus::MaxSteps);
    assert_eq!(outcome.steps, 2);
    assert!(outcome.final_text.is_none());
    assert_eq!(outcome.tool_calls.len(), 2);
    assert_eq!(requests.lock().unwrap().len(), 2);
}

#[test]
fn policy_denied_tool_does_not_call_gateway() {
    let requests = new_requests();
    let calls = new_calls();
    let chat = ScriptedChat::new(
        vec![
            Ok(tool_response(
                vec![tool_call(
                    "call_w",
                    "worker_tool",
                    r#"{"worker_id":"host-gpio","tool":"gpio.write","args":"IO4 1"}"#,
                )],
                usage(1, 1, 2),
            )),
            Ok(text_response(
                "写操作被拒绝：需要 --allow-write。",
                usage(1, 1, 2),
            )),
        ],
        requests.clone(),
    );
    let gateway = FakeGateway::new(vec![worker("host-gpio", true)], vec![], calls.clone());
    let mut agent = Agent::new(
        chat,
        gateway,
        Policy::default(),
        agent_config(4),
        TraceWriter::disabled(),
    );
    let outcome = agent.run("把 IO4 拉高").unwrap();
    assert_eq!(outcome.status, AgentStatus::Completed);
    assert!(
        calls.lock().unwrap().is_empty(),
        "策略拒绝时不得调用 gateway.call"
    );
    assert_eq!(outcome.tool_calls.len(), 1);
    assert_eq!(
        outcome.tool_calls[0].error_code.as_deref(),
        Some("policy_denied")
    );
    assert!(!outcome.had_worker_failure);
    assert!(outcome.last_tool_failed);
    let requests = requests.lock().unwrap();
    let result = message_json(&requests[1].messages[3]);
    assert_eq!(result.get("ok").and_then(Json::as_bool), Some(false));
    assert_eq!(
        result.get_path(&["error", "code"]).and_then(Json::as_str),
        Some("policy_denied")
    );
    assert_eq!(
        result
            .get_path(&["error", "retryable"])
            .and_then(Json::as_bool),
        Some(false)
    );
}

#[test]
fn worker_filter_blocks_other_workers_without_gateway_call() {
    let requests = new_requests();
    let calls = new_calls();
    let chat = ScriptedChat::new(
        vec![
            Ok(tool_response(
                vec![tool_call(
                    "call_f",
                    "worker_tool",
                    r#"{"worker_id":"host-f1","tool":"gpio.info","args":"PA5"}"#,
                )],
                usage(1, 1, 2),
            )),
            Ok(text_response("只能操作 host-gpio。", usage(1, 1, 2))),
        ],
        requests.clone(),
    );
    let gateway = FakeGateway::new(
        vec![worker("host-gpio", true), worker("host-f1", true)],
        vec![],
        calls.clone(),
    );
    let policy = Policy {
        worker_filter: Some(String::from("host-gpio")),
        ..Policy::default()
    };
    let mut agent = Agent::new(
        chat,
        gateway,
        policy,
        agent_config(4),
        TraceWriter::disabled(),
    );
    let outcome = agent.run("查 host-f1 的 PA5").unwrap();
    assert!(calls.lock().unwrap().is_empty());
    assert_eq!(
        outcome.tool_calls[0].error_code.as_deref(),
        Some("policy_denied")
    );
    let requests = requests.lock().unwrap();
    let result = message_json(&requests[1].messages[3]);
    let message = result
        .get_path(&["error", "message"])
        .and_then(Json::as_str)
        .unwrap();
    assert!(message.contains("host-f1"));
}

#[test]
fn unknown_worker_becomes_tool_error() {
    let requests = new_requests();
    let calls = new_calls();
    let chat = ScriptedChat::new(
        vec![
            Ok(tool_response(
                vec![tool_call(
                    "call_u",
                    "worker_tool",
                    r#"{"worker_id":"nope","tool":"gpio.info","args":""}"#,
                )],
                usage(1, 1, 2),
            )),
            Ok(text_response("Worker nope 不存在。", usage(1, 1, 2))),
        ],
        requests.clone(),
    );
    let gateway = FakeGateway::new(
        vec![worker("host-gpio", true)],
        vec![Err(GatewayError::new(
            "worker_not_found",
            "Worker nope 不存在",
        ))],
        calls.clone(),
    );
    let mut agent = Agent::new(
        chat,
        gateway,
        Policy::default(),
        agent_config(4),
        TraceWriter::disabled(),
    );
    let outcome = agent.run("查 nope").unwrap();
    assert_eq!(
        outcome.tool_calls[0].error_code.as_deref(),
        Some("worker_not_found")
    );
    assert!(outcome.had_worker_failure);
    assert_eq!(calls.lock().unwrap().len(), 1);
    let requests = requests.lock().unwrap();
    let result = message_json(&requests[1].messages[3]);
    assert_eq!(
        result.get_path(&["error", "code"]).and_then(Json::as_str),
        Some("worker_not_found")
    );
    assert_eq!(
        result
            .get_path(&["error", "worker_code"])
            .map(Json::is_null),
        Some(true)
    );
}

#[test]
fn worker_failed_result_is_reported_with_error_code() {
    let requests = new_requests();
    let calls = new_calls();
    let chat = ScriptedChat::new(
        vec![
            Ok(tool_response(
                vec![tool_call(
                    "call_g",
                    "worker_tool",
                    r#"{"worker_id":"host-gpio","tool":"gpio.config","args":"IO6 out"}"#,
                )],
                usage(1, 1, 2),
            )),
            Ok(text_response(
                "gpio.config 失败：InvalidArgument，IO6 已保留。",
                usage(1, 1, 2),
            )),
        ],
        requests.clone(),
    );
    let gateway = FakeGateway::new(
        vec![worker("host-gpio", true)],
        vec![Ok(failed_result(
            "host-gpio",
            CommandKind::Tool,
            "gpio.config",
        ))],
        calls.clone(),
    );
    let policy = Policy {
        allow_write: true,
        ..Policy::default()
    };
    let mut agent = Agent::new(
        chat,
        gateway,
        policy,
        agent_config(4),
        TraceWriter::disabled(),
    );
    let outcome = agent.run("配置 IO6").unwrap();
    assert!(outcome.had_worker_failure);
    assert!(outcome.last_tool_failed);
    assert_eq!(
        outcome.tool_calls[0].error_code.as_deref(),
        Some("worker_failed")
    );
    let requests = requests.lock().unwrap();
    let result = message_json(&requests[1].messages[3]);
    assert_eq!(result.get("ok").and_then(Json::as_bool), Some(false));
    assert_eq!(
        result.get_path(&["error", "code"]).and_then(Json::as_str),
        Some("worker_failed")
    );
    assert_eq!(
        result
            .get_path(&["error", "worker_code"])
            .and_then(Json::as_u64),
        Some(3)
    );
    assert_eq!(
        result
            .get_path(&["error", "worker_code_name"])
            .and_then(Json::as_str),
        Some("InvalidArgument")
    );
    assert!(
        result
            .get_path(&["error", "message"])
            .and_then(Json::as_str)
            .unwrap()
            .contains("IO6")
    );
}

#[test]
fn invalid_arguments_become_tool_error_without_gateway_call() {
    let requests = new_requests();
    let calls = new_calls();
    let chat = ScriptedChat::new(
        vec![
            Ok(tool_response(
                vec![tool_call("call_bad", "worker_tool", "this is not json")],
                usage(1, 1, 2),
            )),
            Ok(text_response("参数非法，已停止。", usage(1, 1, 2))),
        ],
        requests.clone(),
    );
    let gateway = FakeGateway::new(vec![worker("host-gpio", true)], vec![], calls.clone());
    let mut agent = Agent::new(
        chat,
        gateway,
        Policy::default(),
        agent_config(4),
        TraceWriter::disabled(),
    );
    let outcome = agent.run("坏参数").unwrap();
    assert!(calls.lock().unwrap().is_empty());
    assert_eq!(
        outcome.tool_calls[0].error_code.as_deref(),
        Some("invalid_arguments")
    );
    assert!(!outcome.had_worker_failure);
    let requests = requests.lock().unwrap();
    let result = message_json(&requests[1].messages[3]);
    assert_eq!(
        result.get_path(&["error", "code"]).and_then(Json::as_str),
        Some("invalid_arguments")
    );
}

#[test]
fn unknown_function_becomes_tool_error() {
    let requests = new_requests();
    let calls = new_calls();
    let chat = ScriptedChat::new(
        vec![
            Ok(tool_response(
                vec![tool_call("call_x", "gpio.write", r#"{"pin":"IO4"}"#)],
                usage(1, 1, 2),
            )),
            Ok(text_response("未知工具，无法执行。", usage(1, 1, 2))),
        ],
        requests.clone(),
    );
    let gateway = FakeGateway::new(vec![worker("host-gpio", true)], vec![], calls.clone());
    let mut agent = Agent::new(
        chat,
        gateway,
        Policy::default(),
        agent_config(4),
        TraceWriter::disabled(),
    );
    let outcome = agent.run("直接调 gpio.write").unwrap();
    assert!(calls.lock().unwrap().is_empty());
    assert_eq!(
        outcome.tool_calls[0].error_code.as_deref(),
        Some("unknown_tool")
    );
    let requests = requests.lock().unwrap();
    let result = message_json(&requests[1].messages[3]);
    assert_eq!(
        result.get_path(&["error", "code"]).and_then(Json::as_str),
        Some("unknown_tool")
    );
    assert!(
        result
            .get_path(&["error", "message"])
            .and_then(Json::as_str)
            .unwrap()
            .contains("gpio.write")
    );
}

#[test]
fn allowed_task_and_ddl_override_are_forwarded() {
    let requests = new_requests();
    let calls = new_calls();
    let chat = ScriptedChat::new(
        vec![
            Ok(tool_response(
                vec![tool_call(
                    "call_t",
                    "worker_task",
                    r#"{"worker_id":"host-gpio","task":"echo","args":"hello","ddl_ms":5000}"#,
                )],
                usage(1, 1, 2),
            )),
            Ok(text_response("任务返回 hello。", usage(1, 1, 2))),
        ],
        requests.clone(),
    );
    let gateway = FakeGateway::new(
        vec![worker("host-gpio", true)],
        vec![Ok(ok_result(
            "host-gpio",
            CommandKind::Task,
            "echo",
            b"hello",
        ))],
        calls.clone(),
    );
    let policy = Policy {
        allow_tasks: true,
        ..Policy::default()
    };
    let mut agent = Agent::new(
        chat,
        gateway,
        policy,
        agent_config(4),
        TraceWriter::disabled(),
    );
    let outcome = agent.run("跑 echo hello").unwrap();
    assert_eq!(outcome.status, AgentStatus::Completed);
    let calls = calls.lock().unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].kind, CommandKind::Task);
    assert_eq!(calls[0].name, "echo");
    assert_eq!(calls[0].args, b"hello");
    assert_eq!(calls[0].ddl_ms, 5_000);
}

#[test]
fn task_denied_without_allow_tasks() {
    let requests = new_requests();
    let calls = new_calls();
    let chat = ScriptedChat::new(
        vec![
            Ok(tool_response(
                vec![tool_call(
                    "call_t2",
                    "worker_task",
                    r#"{"worker_id":"host-gpio","task":"echo","args":"hi"}"#,
                )],
                usage(1, 1, 2),
            )),
            Ok(text_response("Task 未授权。", usage(1, 1, 2))),
        ],
        requests.clone(),
    );
    let gateway = FakeGateway::new(vec![worker("host-gpio", true)], vec![], calls.clone());
    let mut agent = Agent::new(
        chat,
        gateway,
        Policy::default(),
        agent_config(4),
        TraceWriter::disabled(),
    );
    let outcome = agent.run("跑 echo").unwrap();
    assert!(calls.lock().unwrap().is_empty());
    assert_eq!(
        outcome.tool_calls[0].error_code.as_deref(),
        Some("policy_denied")
    );
}

#[test]
fn empty_response_is_an_agent_error() {
    let requests = new_requests();
    let calls = new_calls();
    let chat = ScriptedChat::new(
        vec![Ok(ChatResponse {
            model: String::from("deepseek-flash"),
            content: None,
            tool_calls: Vec::new(),
            finish_reason: None,
            usage: Usage::default(),
        })],
        requests,
    );
    let gateway = FakeGateway::new(vec![], vec![], calls);
    let mut agent = Agent::new(
        chat,
        gateway,
        Policy::default(),
        agent_config(4),
        TraceWriter::disabled(),
    );
    let error = agent.run("空响应").unwrap_err();
    assert_eq!(error, AgentError::EmptyResponse);
}

#[test]
fn llm_error_aborts_the_loop() {
    let requests = new_requests();
    let calls = new_calls();
    let chat = ScriptedChat::new(
        vec![Err(LlmError::Api {
            status: 401,
            message: String::from("invalid api key"),
        })],
        requests.clone(),
    );
    let gateway = FakeGateway::new(vec![], vec![], calls);
    let mut agent = Agent::new(
        chat,
        gateway,
        Policy::default(),
        agent_config(4),
        TraceWriter::disabled(),
    );
    let error = agent.run("hi").unwrap_err();
    match error {
        AgentError::Llm(message) => assert!(message.contains("401")),
        other => panic!("expected Llm error, got {other:?}"),
    }
    assert_eq!(requests.lock().unwrap().len(), 1);
}

#[test]
fn usage_is_aggregated_across_steps() {
    let requests = new_requests();
    let calls = new_calls();
    let chat = ScriptedChat::new(
        vec![
            Ok(tool_response(
                vec![tool_call("c1", "list_workers", "{}")],
                usage(10, 5, 15),
            )),
            Ok(tool_response(
                vec![tool_call("c2", "list_workers", "{}")],
                usage(20, 7, 27),
            )),
            Ok(text_response("完成。", usage(30, 11, 41))),
        ],
        requests,
    );
    let gateway = FakeGateway::new(vec![worker("host-gpio", true)], vec![], calls);
    let mut agent = Agent::new(
        chat,
        gateway,
        Policy::default(),
        agent_config(5),
        TraceWriter::disabled(),
    );
    let outcome = agent.run("hi").unwrap();
    assert_eq!(outcome.usage, usage(60, 23, 83));
    assert_eq!(outcome.steps, 3);
}

#[test]
fn trace_records_redacted_events() {
    let mut path = std::env::temp_dir();
    path.push(format!(
        "sekai-router-agent-trace-{}-{}.jsonl",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_millis())
            .unwrap_or(0)
    ));
    let requests = new_requests();
    let calls = new_calls();
    let chat = ScriptedChat::new(
        vec![
            Ok(tool_response(
                vec![tool_call(
                    "call_trace",
                    "worker_tool",
                    r#"{"worker_id":"host-gpio","tool":"gpio.info","args":"help"}"#,
                )],
                usage(3, 4, 7),
            )),
            Ok(text_response("trace 完成。", usage(5, 6, 11))),
        ],
        requests,
    );
    let gateway = FakeGateway::new(
        vec![worker("host-gpio", true)],
        vec![Ok(ok_result(
            "host-gpio",
            CommandKind::Tool,
            "gpio.info",
            b"gpio tools v1",
        ))],
        calls,
    );
    let trace = TraceWriter::new(Some(path.as_path())).unwrap();
    let mut agent = Agent::new(chat, gateway, Policy::default(), agent_config(4), trace);
    let outcome = agent.run("trace 一下").unwrap();
    assert_eq!(outcome.status, AgentStatus::Completed);
    let text = std::fs::read_to_string(&path).unwrap();
    let _ = std::fs::remove_file(&path);
    assert!(!text.contains("Bearer"));
    assert!(!text.contains("Authorization"));
    let events: Vec<Json> = text
        .lines()
        .map(|line| Json::parse(line).expect("trace 行必须是合法 JSON"))
        .collect();
    assert!(!events.is_empty());
    let names: Vec<&str> = events
        .iter()
        .map(|event| event.get("event").and_then(Json::as_str).unwrap())
        .collect();
    for required in [
        "agent_start",
        "llm_request",
        "llm_response",
        "usage",
        "tool_call",
        "tool_result",
        "completed",
    ] {
        assert!(
            names.contains(&required),
            "trace 缺少事件 {required}: {names:?}"
        );
    }
    let tool_result = events
        .iter()
        .find(|event| event.get("event").and_then(Json::as_str) == Some("tool_result"))
        .unwrap();
    assert_eq!(tool_result.get("ok").and_then(Json::as_bool), Some(true));
    assert!(
        tool_result
            .get("result")
            .and_then(Json::as_str)
            .unwrap()
            .contains("gpio tools v1")
    );
    let usage_event = events
        .iter()
        .find(|event| event.get("event").and_then(Json::as_str) == Some("usage"))
        .unwrap();
    assert_eq!(
        usage_event.get("total_tokens").and_then(Json::as_u64),
        Some(7)
    );
    assert_eq!(
        usage_event
            .get("cumulative_total_tokens")
            .and_then(Json::as_u64),
        Some(7)
    );
}
