//! Offline integration of authenticated Feishu tools, Agent and PromptCore.
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use sekai_router::agent::{AgentConfig, CancelToken, ContextPolicy, ConversationContext};
use sekai_router::control::CommandRequest;
use sekai_router::feishu_api::{FeishuApi, FeishuTools};
use sekai_router::gateway::{CommandReceipt, GatewayError, GatewayHandle, WorkerGateway};
use sekai_router::json::Json;
use sekai_router::llm::{ChatClient, ChatRequest, ChatResponse, LlmError, Role, ToolCall, Usage};
use sekai_router::prompt_server::{CODE_NO_WORKERS, PromptCore};
use sekai_router::tools::Policy;
use sekai_router::trace::TraceWriter;
use sekai_router::worker::WorkerInfo;

#[derive(Clone)]
struct Chat {
    replies: Arc<Mutex<VecDeque<ChatResponse>>>,
    requests: Arc<Mutex<Vec<ChatRequest>>>,
}
impl ChatClient for Chat {
    fn chat(&mut self, request: &ChatRequest) -> Result<ChatResponse, LlmError> {
        self.requests.lock().unwrap().push(request.clone());
        Ok(self
            .replies
            .lock()
            .unwrap()
            .pop_front()
            .expect("unexpected LLM call"))
    }
}
struct EmptyGateway;
impl WorkerGateway for EmptyGateway {
    fn list_workers(&mut self) -> Vec<WorkerInfo> {
        Vec::new()
    }
    fn submit(&mut self, _: CommandRequest) -> Result<CommandReceipt, GatewayError> {
        panic!("Feishu API must not submit a Worker command")
    }
    fn wait_ready(&mut self, _: u64) -> Result<(), GatewayError> {
        Ok(())
    }
    fn is_ready(&self) -> bool {
        true
    }
}
#[derive(Default)]
struct Api {
    calls: Mutex<Vec<(String, String, Json)>>,
}
impl FeishuApi for Api {
    fn request(&self, method: &str, path: &str, body: &Json) -> Result<Json, String> {
        self.calls
            .lock()
            .unwrap()
            .push((method.into(), path.into(), body.clone()));
        if method == "GET" && !path.contains("/events") {
            return Ok(Json::parse(r#"{"code":0,"data":{"calendar_list":[]}}"#).unwrap());
        }
        Ok(Json::parse(r#"{"code":0,"data":{"event":{"event_id":"event-created","app_link":"https://applink.feishu.cn/calendar/view?id=event-created"}}}"#).unwrap())
    }
}
fn response(calls: Vec<ToolCall>) -> ChatResponse {
    ChatResponse {
        model: "test".into(),
        content: Some("完成".into()),
        reasoning_content: None,
        tool_calls: calls,
        finish_reason: None,
        usage: Usage::default(),
    }
}
fn create_call() -> ToolCall {
    ToolCall {
        id: "create-1".into(), name: "feishu_event_create".into(),
        arguments: r#"{"calendar_id":"cal-1","summary":"项目讨论","start_time":{"timestamp":"1800000000","timezone":"Asia/Shanghai"},"end_time":{"timestamp":"1800003600","timezone":"Asia/Shanghai"},"idempotency_key":"sekai-create-event-000000000000001"}"#.into(),
    }
}
fn core(allow_write: bool, replies: Vec<ChatResponse>) -> (PromptCore<Chat>, Chat) {
    let chat = Chat {
        replies: Arc::new(Mutex::new(replies.into())),
        requests: Arc::new(Mutex::new(Vec::new())),
    };
    let core = PromptCore::new(
        chat.clone(),
        Policy {
            allow_write,
            ..Policy::default()
        },
        AgentConfig {
            model: "test".into(),
            max_steps: Some(4),
            ddl_ms: 1000,
            timeout_ms: Some(1000),
            temperature: 0.0,
            max_tokens: 1024,
            context: ContextPolicy::default(),
        },
        TraceWriter::disabled(),
    );
    let gateway: GatewayHandle = Arc::new(Mutex::new(Box::new(EmptyGateway)));
    core.set_gateway(Some(gateway));
    (core, chat)
}
fn result_json(chat: &Chat) -> Json {
    let requests = chat.requests.lock().unwrap();
    let message = requests[1]
        .messages
        .iter()
        .find(|m| m.role == Role::Tool)
        .unwrap();
    Json::parse(message.content.as_ref().unwrap()).unwrap()
}
#[test]
fn authenticated_feishu_creates_event_without_worker_and_returns_evidence() {
    let (core, chat) = core(
        true,
        vec![response(vec![create_call()]), response(Vec::new())],
    );
    let api = Arc::new(Api::default());
    let tools = Arc::new(FeishuTools::new(api.clone()));
    let result = core
        .run_feishu_prompt(
            "创建项目讨论日程",
            None,
            CancelToken::new(),
            ConversationContext {
                channel: "feishu".into(),
                chat_id: "oc-test".into(),
                ..ConversationContext::default()
            },
            tools,
        )
        .unwrap();
    assert_eq!(result.status, "completed");
    assert_eq!(api.calls.lock().unwrap().len(), 1);
    let json = result_json(&chat);
    assert_eq!(json.get("ok").and_then(Json::as_bool), Some(true));
    assert!(json.to_compact_string().contains("event-created"));
    let requests = chat.requests.lock().unwrap();
    assert!(
        requests[0]
            .tools
            .iter()
            .any(|t| t.name == "feishu_event_create")
    );
    let system = requests[0].messages[0].content.as_ref().unwrap();
    assert!(system.contains("Asia/Shanghai"));
    assert!(system.contains("Unix 秒"));
    assert!(system.contains("oc-test"));
    drop(requests);
    // The same shared core must not leak Feishu capability to generic ingress.
    let error = core.run_prompt("创建日程", None).unwrap_err();
    assert_eq!(error.code, CODE_NO_WORKERS);
}
#[test]
fn readonly_policy_denies_feishu_write_before_http() {
    let (core, chat) = core(
        false,
        vec![response(vec![create_call()]), response(Vec::new())],
    );
    let api = Arc::new(Api::default());
    core.run_feishu_prompt(
        "创建日程",
        None,
        CancelToken::new(),
        ConversationContext::default(),
        Arc::new(FeishuTools::new(api.clone())),
    )
    .unwrap();
    assert!(api.calls.lock().unwrap().is_empty());
    assert_eq!(
        result_json(&chat)
            .get_path(&["error", "code"])
            .and_then(Json::as_str),
        Some("policy_denied")
    );
}
#[test]
fn read_tools_work_without_write_permission_or_worker() {
    let call = ToolCall {
        id: "list-1".into(),
        name: "feishu_calendar_list".into(),
        arguments: "{}".into(),
    };
    let (core, chat) = core(false, vec![response(vec![call]), response(Vec::new())]);
    let api = Arc::new(Api::default());
    core.run_feishu_prompt(
        "列出可访问日历",
        None,
        CancelToken::new(),
        ConversationContext::default(),
        Arc::new(FeishuTools::new(api.clone())),
    )
    .unwrap();
    let calls = api.calls.lock().unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].0, "GET");
    assert!(calls[0].1.starts_with("/open-apis/calendar/v4/calendars"));
    assert_eq!(
        result_json(&chat).get("ok").and_then(Json::as_bool),
        Some(true)
    );
}
