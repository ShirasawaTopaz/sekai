//! llm.rs 离线 HTTP 测试：本地 TcpListener mock server，不访问真实网络。
//!
//! 覆盖：请求方法/路径/Authorization/Content-Type/thinking/tools/tool_choice/
//! stream=false；普通回答与 tool_calls 解析；429 重试成功、5xx 重试耗尽、
//! 401 不重试、非法 JSON、错误信息不含 key、list_models。

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use sekai_router::http::{HttpsHelper, StdHttpClient};
use sekai_router::json::Json;
use sekai_router::llm::{
    ChatClient, ChatRequest, LlmClient, LlmConfig, LlmError, Message, ToolChoice, ToolDef,
};

const ANSWER_BODY: &str = r#"{"id":"chatcmpl-1","model":"deepseek-flash","choices":[{"index":0,"message":{"role":"assistant","content":"hello from mock"},"finish_reason":"stop"}],"usage":{"prompt_tokens":11,"completion_tokens":7,"total_tokens":18}}"#;

const TOOL_CALLS_BODY: &str = r#"{"id":"chatcmpl-2","model":"deepseek-flash","choices":[{"index":0,"message":{"role":"assistant","content":null,"tool_calls":[{"id":"call_a","type":"function","function":{"name":"list_workers","arguments":"{}"}},{"id":"call_b","type":"function","function":{"name":"worker_tool","arguments":"{\"worker_id\":\"host-gpio\",\"tool\":\"gpio.info\",\"args\":\"help\"}"}}]},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":20,"completion_tokens":30,"total_tokens":50}}"#;

const MALFORMED_ARGS_BODY: &str = r#"{"model":"deepseek-flash","choices":[{"message":{"content":null,"tool_calls":[{"id":"call_x","type":"function","function":{"name":"worker_tool","arguments":"{not valid json"}}]},"finish_reason":"tool_calls"}]}"#;

/// 顺序返回预设响应的本地 HTTP server。
struct MockServer {
    url: String,
    requests: mpsc::Receiver<String>,
    stop: mpsc::Sender<()>,
    handle: thread::JoinHandle<()>,
}

impl MockServer {
    fn start(responses: Vec<String>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock server");
        let port = listener.local_addr().unwrap().port();
        listener
            .set_nonblocking(true)
            .expect("nonblocking listener");
        let (request_tx, request_rx) = mpsc::channel();
        let (stop_tx, stop_rx) = mpsc::channel::<()>();
        let handle = thread::spawn(move || {
            let mut served = 0usize;
            let deadline = Instant::now() + Duration::from_secs(10);
            while served < responses.len() && Instant::now() < deadline {
                if stop_rx.try_recv().is_ok() {
                    return;
                }
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        stream.set_nonblocking(false).ok();
                        stream.set_read_timeout(Some(Duration::from_secs(5))).ok();
                        stream.set_write_timeout(Some(Duration::from_secs(5))).ok();
                        let request = read_request(&mut stream);
                        let _ = request_tx.send(request);
                        let response = &responses[served];
                        let _ = stream.write_all(response.as_bytes());
                        let _ = stream.flush();
                        served += 1;
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => return,
                }
            }
        });
        Self {
            url: format!("http://127.0.0.1:{port}"),
            requests: request_rx,
            stop: stop_tx,
            handle,
        }
    }

    fn take_request(&self) -> String {
        self.requests
            .recv_timeout(Duration::from_secs(10))
            .expect("mock server 未收到请求")
    }

    fn assert_no_more_requests(&self) {
        thread::sleep(Duration::from_millis(150));
        assert!(
            self.requests.try_recv().is_err(),
            "不应有额外请求（可能发生了不该有的重试）"
        );
    }

    fn stop(self) {
        let _ = self.stop.send(());
        let _ = self.handle.join();
    }
}

fn read_request(stream: &mut std::net::TcpStream) -> String {
    let mut raw: Vec<u8> = Vec::new();
    let mut buffer = [0u8; 4096];
    loop {
        match stream.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => raw.extend_from_slice(&buffer[..read]),
            Err(_) => break,
        }
        if let Some(index) = find_header_end(&raw) {
            let head = String::from_utf8_lossy(&raw[..index]).to_string();
            let content_length = head
                .lines()
                .find_map(|line| {
                    line.to_ascii_lowercase()
                        .strip_prefix("content-length:")
                        .map(|value| value.trim().parse::<usize>().unwrap_or(0))
                })
                .unwrap_or(0);
            if raw.len() >= index + 4 + content_length {
                break;
            }
        }
    }
    String::from_utf8_lossy(&raw).to_string()
}

fn find_header_end(bytes: &[u8]) -> Option<usize> {
    bytes.windows(4).position(|window| window == b"\r\n\r\n")
}

fn json_response(status: u16, body: &str) -> String {
    format!(
        "HTTP/1.1 {status} OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

fn error_response(status: u16, message: &str) -> String {
    let body = format!("{{\"error\":{{\"message\":\"{message}\"}}}}");
    json_response(status, &body)
}

fn client(base_url: &str, key: &str, retry_base_ms: u64, max_retries: u32) -> LlmClient {
    let mut config = LlmConfig::new(format!("{base_url}/v1"), "deepseek-flash", key);
    config.retry_base_ms = retry_base_ms;
    config.max_retries = max_retries;
    LlmClient::new(
        Box::new(StdHttpClient::with_helper(HttpsHelper::Disabled)),
        config,
    )
}

fn tool_def(name: &str) -> ToolDef {
    ToolDef {
        name: String::from(name),
        description: String::from("desc"),
        parameters: Json::object(vec![("type", Json::string("object"))]),
    }
}

fn sample_request(choice: ToolChoice) -> ChatRequest {
    ChatRequest {
        model: String::from("deepseek-flash"),
        messages: vec![Message::system("sys prompt"), Message::user("user goal")],
        tools: vec![
            tool_def("list_workers"),
            tool_def("worker_tool"),
            tool_def("worker_task"),
        ],
        tool_choice: choice,
        temperature: 0.0,
        max_tokens: 4096,
        disable_thinking: true,
    }
}

fn split_request(request: &str) -> (String, Json) {
    let index = request.find("\r\n\r\n").expect("请求缺少头部结束标记");
    let head = String::from(request[..index].to_string().as_str());
    let body = &request[index + 4..];
    let value = Json::parse(body).expect("请求体不是合法 JSON");
    (head, value)
}

#[test]
fn chat_request_has_expected_method_path_and_headers() {
    let server = MockServer::start(vec![json_response(200, ANSWER_BODY)]);
    let mut client = client(&server.url, "sk-llm-http-secret-42", 0, 0);
    let response = client
        .chat(&sample_request(ToolChoice::Function(String::from(
            "worker_tool",
        ))))
        .expect("chat should succeed");
    assert_eq!(response.content.as_deref(), Some("hello from mock"));
    assert_eq!(response.usage.total_tokens, 18);

    let request = server.take_request();
    server.stop();
    let (head, body) = split_request(&request);
    assert!(
        head.starts_with("POST /v1/chat/completions HTTP/1.1"),
        "unexpected request line: {head}"
    );
    assert!(
        head.to_ascii_lowercase()
            .contains("authorization: bearer sk-llm-http-secret-42"),
        "missing Authorization header: {head}"
    );
    assert!(
        head.to_ascii_lowercase()
            .contains("content-type: application/json"),
        "missing Content-Type header: {head}"
    );
    assert_eq!(
        body.get("model").and_then(Json::as_str),
        Some("deepseek-flash")
    );
    assert_eq!(body.get("stream").and_then(Json::as_bool), Some(false));
    assert_eq!(body.get("temperature").and_then(Json::as_f64), Some(0.0));
    assert_eq!(body.get("max_tokens").and_then(Json::as_u64), Some(4096));
    assert_eq!(
        body.get_path(&["thinking", "type"]).and_then(Json::as_str),
        Some("disabled")
    );
    let messages = body.get("messages").and_then(Json::as_array).unwrap();
    assert_eq!(
        messages[0].get("role").and_then(Json::as_str),
        Some("system")
    );
    assert_eq!(
        messages[0].get("content").and_then(Json::as_str),
        Some("sys prompt")
    );
    assert_eq!(messages[1].get("role").and_then(Json::as_str), Some("user"));
    let tools = body.get("tools").and_then(Json::as_array).unwrap();
    let names: Vec<&str> = tools
        .iter()
        .map(|tool| {
            tool.get_path(&["function", "name"])
                .and_then(Json::as_str)
                .unwrap()
        })
        .collect();
    assert_eq!(names, vec!["list_workers", "worker_tool", "worker_task"]);
    assert_eq!(
        tools[0].get("type").and_then(Json::as_str),
        Some("function")
    );
    assert_eq!(
        tools[0]
            .get_path(&["function", "description"])
            .and_then(Json::as_str),
        Some("desc")
    );
    assert_eq!(
        body.get_path(&["tool_choice", "function", "name"])
            .and_then(Json::as_str),
        Some("worker_tool")
    );
}

#[test]
fn tool_choice_auto_and_none_serialize_as_strings() {
    let server = MockServer::start(vec![
        json_response(200, ANSWER_BODY),
        json_response(200, ANSWER_BODY),
    ]);
    let mut client = client(&server.url, "sk-test", 0, 0);
    client.chat(&sample_request(ToolChoice::Auto)).unwrap();
    client.chat(&sample_request(ToolChoice::None)).unwrap();
    let first = split_request(&server.take_request()).1;
    let second = split_request(&server.take_request()).1;
    server.stop();
    assert_eq!(
        first.get("tool_choice").and_then(Json::as_str),
        Some("auto")
    );
    assert_eq!(
        second.get("tool_choice").and_then(Json::as_str),
        Some("none")
    );
}

#[test]
fn chat_without_tools_omits_tools_and_tool_choice() {
    let server = MockServer::start(vec![json_response(200, ANSWER_BODY)]);
    let mut client = client(&server.url, "sk-test", 0, 0);
    let mut request = sample_request(ToolChoice::Auto);
    request.tools.clear();
    client.chat(&request).unwrap();
    let body = split_request(&server.take_request()).1;
    server.stop();
    assert!(body.get("tools").is_none());
    assert!(body.get("tool_choice").is_none());
}

#[test]
fn parses_plain_answer_with_finish_reason_and_usage() {
    let server = MockServer::start(vec![json_response(200, ANSWER_BODY)]);
    let mut client = client(&server.url, "sk-test", 0, 0);
    let response = client.chat(&sample_request(ToolChoice::Auto)).unwrap();
    server.stop();
    assert_eq!(response.model, "deepseek-flash");
    assert_eq!(response.content.as_deref(), Some("hello from mock"));
    assert!(response.tool_calls.is_empty());
    assert_eq!(response.finish_reason.as_deref(), Some("stop"));
    assert_eq!(response.usage.prompt_tokens, 11);
    assert_eq!(response.usage.completion_tokens, 7);
    assert_eq!(response.usage.total_tokens, 18);
}

#[test]
fn parses_multiple_tool_calls_in_order() {
    let server = MockServer::start(vec![json_response(200, TOOL_CALLS_BODY)]);
    let mut client = client(&server.url, "sk-test", 0, 0);
    let response = client.chat(&sample_request(ToolChoice::Auto)).unwrap();
    server.stop();
    assert!(response.content.is_none());
    assert_eq!(response.finish_reason.as_deref(), Some("tool_calls"));
    assert_eq!(response.usage.total_tokens, 50);
    assert_eq!(response.tool_calls.len(), 2);
    assert_eq!(response.tool_calls[0].id, "call_a");
    assert_eq!(response.tool_calls[0].name, "list_workers");
    assert_eq!(response.tool_calls[0].arguments, "{}");
    assert_eq!(response.tool_calls[1].id, "call_b");
    assert_eq!(response.tool_calls[1].name, "worker_tool");
    let arguments = Json::parse(&response.tool_calls[1].arguments).unwrap();
    assert_eq!(
        arguments.get("worker_id").and_then(Json::as_str),
        Some("host-gpio")
    );
    assert_eq!(arguments.get("args").and_then(Json::as_str), Some("help"));
}

#[test]
fn malformed_arguments_are_preserved_verbatim() {
    let server = MockServer::start(vec![json_response(200, MALFORMED_ARGS_BODY)]);
    let mut client = client(&server.url, "sk-test", 0, 0);
    let response = client.chat(&sample_request(ToolChoice::Auto)).unwrap();
    server.stop();
    assert_eq!(response.tool_calls.len(), 1);
    assert_eq!(response.tool_calls[0].arguments, "{not valid json");
}

#[test]
fn empty_content_and_no_tool_calls_is_protocol_error() {
    let server = MockServer::start(vec![json_response(
        200,
        r#"{"choices":[{"message":{"content":null},"finish_reason":"stop"}]}"#,
    )]);
    let mut client = client(&server.url, "sk-test", 0, 0);
    let error = client.chat(&sample_request(ToolChoice::Auto)).unwrap_err();
    server.stop();
    assert!(matches!(error, LlmError::Protocol(_)), "got {error:?}");
}

#[test]
fn invalid_json_response_is_protocol_error() {
    let server = MockServer::start(vec![json_response(200, "not json at all")]);
    let mut client = client(&server.url, "sk-test", 0, 0);
    let error = client.chat(&sample_request(ToolChoice::Auto)).unwrap_err();
    server.stop();
    assert!(matches!(error, LlmError::Protocol(_)), "got {error:?}");
}

#[test]
fn retries_on_429_then_succeeds() {
    let server = MockServer::start(vec![
        error_response(429, "rate limited"),
        json_response(200, ANSWER_BODY),
    ]);
    let mut client = client(&server.url, "sk-test", 0, 3);
    let response = client.chat(&sample_request(ToolChoice::Auto)).unwrap();
    assert_eq!(response.content.as_deref(), Some("hello from mock"));
    server.take_request();
    server.take_request();
    server.assert_no_more_requests();
    server.stop();
}

#[test]
fn retries_on_5xx_until_exhausted() {
    let server = MockServer::start(vec![
        error_response(503, "overloaded"),
        error_response(503, "overloaded"),
        error_response(503, "overloaded"),
    ]);
    let mut client = client(&server.url, "sk-test", 0, 2);
    let error = client.chat(&sample_request(ToolChoice::Auto)).unwrap_err();
    match &error {
        LlmError::Api { status, message } => {
            assert_eq!(*status, 503);
            assert_eq!(message, "overloaded");
        }
        other => panic!("expected Api error, got {other:?}"),
    }
    server.take_request();
    server.take_request();
    server.take_request();
    server.assert_no_more_requests();
    server.stop();
}

#[test]
fn does_not_retry_on_401_and_does_not_leak_key() {
    let key = "sk-very-secret-key-123";
    let server = MockServer::start(vec![error_response(401, "invalid api key")]);
    let mut client = client(&server.url, key, 0, 3);
    let error = client.chat(&sample_request(ToolChoice::Auto)).unwrap_err();
    match &error {
        LlmError::Api { status, message } => {
            assert_eq!(*status, 401);
            assert_eq!(message, "invalid api key");
        }
        other => panic!("expected Api error, got {other:?}"),
    }
    let message = error.to_string();
    assert!(!message.contains(key), "错误消息泄露 key：{message}");
    let config_debug = format!("{:?}", client.config());
    assert!(
        !config_debug.contains(key),
        "Debug 泄露 key：{config_debug}"
    );
    assert!(config_debug.contains("redacted"));
    server.take_request();
    server.assert_no_more_requests();
    server.stop();
}

#[test]
fn transport_error_is_reported_without_key() {
    let key = "sk-transport-secret-9";
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let mut client = client(&format!("http://127.0.0.1:{port}"), key, 0, 1);
    let error = client.chat(&sample_request(ToolChoice::Auto)).unwrap_err();
    assert!(matches!(error, LlmError::Transport(_)), "got {error:?}");
    assert!(!error.to_string().contains(key));
}

#[test]
fn list_models_parses_data_ids() {
    let server = MockServer::start(vec![json_response(
        200,
        r#"{"object":"list","data":[{"id":"deepseek-flash","object":"model"},{"id":"deepseek-v4-pro","object":"model"}]}"#,
    )]);
    let mut client = client(&server.url, "sk-models-key", 0, 0);
    let models = client.list_models().unwrap();
    assert_eq!(models, vec!["deepseek-flash", "deepseek-v4-pro"]);
    let request = server.take_request();
    server.stop();
    let head = request
        .split("\r\n\r\n")
        .next()
        .unwrap_or_default()
        .to_string();
    assert!(head.starts_with("GET /v1/models HTTP/1.1"), "head: {head}");
    assert!(
        head.to_ascii_lowercase()
            .contains("authorization: bearer sk-models-key")
    );
}

#[test]
fn list_models_rejects_missing_data() {
    let server = MockServer::start(vec![json_response(200, r#"{"object":"list"}"#)]);
    let mut client = client(&server.url, "sk-test", 0, 0);
    let error = client.list_models().unwrap_err();
    server.stop();
    assert!(matches!(error, LlmError::Protocol(_)), "got {error:?}");
}
