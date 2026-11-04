//! 真实 DeepSeek API 验收测试（计划 §2.3 / §8.3，共 7 项）。
//!
//! 默认不运行：全部测试带 #[ignore]；进入测试体后还会检查
//! SEKAI_REAL_API=1 与 DEEPSEEK_API_KEY，缺少条件时打印 skip 并返回。
//!
//! 本机（DSH 沙箱，cargo 不可用）运行方式（仓库根）：
//!
//! ~~~bash
//! # 构建：先得到 .build/real_api.exe；本次运行 0 个用例（默认全部 ignore）。
//! bash .build/build-test.sh crate/sekai-router/tests/real_api.rs real_api
//!
//! # 真实运行（在 .build 下；key 只经环境变量传递，不写入文件/日志）
//! cd .build
//! DEEPSEEK_API_KEY=$(sed -n 's/^  DEEPSEEK_API_KEY: //p' ~/.dsh/.credentials.yaml | tr -d '\r\n') \
//!   SEKAI_REAL_API=1 ./real_api.exe --ignored --nocapture --test-threads=1
//! ~~~
//!
//! 可选环境变量：DEEPSEEK_BASE_URL（默认 https://api.deepseek.com）、
//! SEKAI_ROUTER_MODEL（默认 deepseek-flash）。
//!
//! 每项测试都打印 model、usage.* 与累计 API 调用次数 / token 总量。
//! GPIO/Task 结果一律通过直接查询Worker验证：同一个内存 harness 里的
//! ControlPlane + 真实 Runtime + VirtualGpio Worker节点，不依赖模型文本。

mod support;

use std::cell::RefCell;
use std::env;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use sekai_gpio::pin::PinId;
use sekai_gpio::{ChipFamily, Level, Port};
use sekai_router::agent::{Agent, AgentConfig, AgentOutcome, AgentStatus};
use sekai_router::control::{CommandKind, CommandRequest, CommandResult};
use sekai_router::gateway::{CommandReceipt, GatewayError, WorkerGateway};
use sekai_router::http::StdHttpClient;
use sekai_router::llm::{
    ChatClient, ChatRequest, ChatResponse, LlmClient, LlmConfig, LlmError, Message, ToolChoice,
    Usage,
};
use sekai_router::tools::{self, ParsedCall, Policy};
use sekai_router::trace::TraceWriter;
use sekai_router::worker::WorkerInfo;

use support::{DEFAULT_BASE_UNIX_MS, DEFAULT_TOPIC_ROOT, Harness};

// ---------------------------------------------------------------------------
// 真实 API 调用统计（--test-threads=1 时在单进程内累计）
// ---------------------------------------------------------------------------

static API_CALLS: AtomicU64 = AtomicU64::new(0);
static PROMPT_TOKENS: AtomicU64 = AtomicU64::new(0);
static COMPLETION_TOKENS: AtomicU64 = AtomicU64::new(0);
static TOTAL_TOKENS: AtomicU64 = AtomicU64::new(0);

fn count_api_call() {
    API_CALLS.fetch_add(1, Ordering::Relaxed);
}

fn record_usage(usage: &Usage) {
    PROMPT_TOKENS.fetch_add(usage.prompt_tokens, Ordering::Relaxed);
    COMPLETION_TOKENS.fetch_add(usage.completion_tokens, Ordering::Relaxed);
    TOTAL_TOKENS.fetch_add(usage.total_tokens, Ordering::Relaxed);
}

/// 打印单项测试的 model / usage / 累计统计（真实 API 证据行）。
fn print_usage(label: &str, model: &str, usage: &Usage) {
    println!(
        "real_api[{label}] model={model} prompt_tokens={} completion_tokens={} total_tokens={} cumulative_api_calls={} cumulative_total_tokens={}",
        usage.prompt_tokens,
        usage.completion_tokens,
        usage.total_tokens,
        API_CALLS.load(Ordering::Relaxed),
        TOTAL_TOKENS.load(Ordering::Relaxed),
    );
}

/// 读取真实 API 配置；不满足运行条件时打印 skip 并返回 None。
fn real_config(test: &str) -> Option<LlmConfig> {
    if env::var("SEKAI_REAL_API").ok().as_deref() != Some("1") {
        eprintln!("[{test}] skip：真实 API 测试默认不运行（设置 SEKAI_REAL_API=1 才启用）");
        return None;
    }
    let api_key = match env::var("DEEPSEEK_API_KEY") {
        Ok(value) if !value.trim().is_empty() => value,
        _ => {
            eprintln!("[{test}] skip：缺少 DEEPSEEK_API_KEY");
            return None;
        }
    };
    let base_url = env::var("DEEPSEEK_BASE_URL")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| String::from("https://api.deepseek.com"));
    let model = env::var("SEKAI_ROUTER_MODEL")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| String::from("deepseek-flash"));
    let http = StdHttpClient::new();
    let helper = http.https_helper_name();
    if helper.is_none() && base_url.starts_with("https://") {
        eprintln!("[{test}] skip：当前环境没有 HTTPS helper（node/curl），无法访问 {base_url}");
        return None;
    }
    eprintln!("[{test}] base_url={base_url} model={model} https_helper={helper:?}");
    Some(LlmConfig {
        base_url,
        model,
        api_key,
        max_tokens: 4096,
        temperature: 0.0,
        timeout: Duration::from_secs(120),
        retry_base_ms: 1_000,
        max_retries: 3,
    })
}

// ---------------------------------------------------------------------------
// 进程内环回网关：Agent 用它下发命令，测试用同一 harness 直接查Worker状态
// ---------------------------------------------------------------------------

struct LoopbackGateway {
    harness: Rc<RefCell<Harness>>,
    calls: Rc<RefCell<Vec<CommandRequest>>>,
    results: Rc<RefCell<Vec<CommandResult>>>,
}

// 测试（含 --ignored 真实 API 用例）以 --test-threads=1 运行，gateway 与
// harness 全程停留在同一线程，Rc 不会跨线程移动；此处声明 Send 仅为满足
// `WorkerGateway: Send` 的 trait 约束。
unsafe impl Send for LoopbackGateway {}

impl LoopbackGateway {
    fn new(harness: Rc<RefCell<Harness>>) -> Self {
        Self {
            harness,
            calls: Rc::new(RefCell::new(Vec::new())),
            results: Rc::new(RefCell::new(Vec::new())),
        }
    }
}

impl WorkerGateway for LoopbackGateway {
    fn list_workers(&mut self) -> Vec<WorkerInfo> {
        self.harness.borrow().server().workers()
    }

    fn submit(&mut self, request: CommandRequest) -> Result<CommandReceipt, GatewayError> {
        self.calls.borrow_mut().push(request.clone());
        let result = (|| {
            let mut harness = self.harness.borrow_mut();
            let cmd_id = harness
                .try_submit(request.clone())
                .map_err(|error| GatewayError::from_control(&error))?;
            // 内存 harness 用 ManualClock 推进命令超时：给足预算，正常路径会提前返回。
            let timeout_ms = request.timeout_ms.unwrap_or(60_000);
            let result = harness
                .run_until_command(cmd_id, timeout_ms)
                .ok_or_else(|| {
                    GatewayError::new(
                        "timeout",
                        format!("命令 {cmd_id} 在 {timeout_ms}ms 内没有最终事件"),
                    )
                })?;
            self.results.borrow_mut().push(result.clone());
            Ok(result)
        })();
        let (sender, receiver) = std::sync::mpsc::channel();
        let _ = sender.send(result);
        Ok(CommandReceipt::new(receiver, 60_000, "内存"))
    }

    fn wait_ready(&mut self, _timeout_ms: u64) -> Result<(), GatewayError> {
        if self.harness.borrow().server().is_ready() {
            Ok(())
        } else {
            Err(GatewayError::new(
                "link_lost",
                "内存 harness 控制平面未就绪",
            ))
        }
    }

    fn is_ready(&self) -> bool {
        self.harness.borrow().server().is_ready()
    }
}

/// 统计真实 API 调用次数与 token 的 ChatClient（不改变真实调用语义）。
struct CountingChat {
    inner: LlmClient,
    last_model: Rc<RefCell<String>>,
}

impl ChatClient for CountingChat {
    fn chat(&mut self, request: &ChatRequest) -> Result<ChatResponse, LlmError> {
        count_api_call();
        let response = self.inner.chat(request)?;
        record_usage(&response.usage);
        if !response.model.is_empty() {
            *self.last_model.borrow_mut() = response.model.clone();
        }
        Ok(response)
    }
}

// ---------------------------------------------------------------------------
// harness / agent 辅助
// ---------------------------------------------------------------------------

fn single_esp32(inputs: &[(PinId, Level)]) -> Rc<RefCell<Harness>> {
    let mut harness = Harness::new(DEFAULT_TOPIC_ROOT, DEFAULT_BASE_UNIX_MS);
    harness.add_worker("esp32-1", ChipFamily::Esp32, inputs);
    harness.connect();
    Rc::new(RefCell::new(harness))
}

fn two_workers() -> Rc<RefCell<Harness>> {
    let mut harness = Harness::new(DEFAULT_TOPIC_ROOT, DEFAULT_BASE_UNIX_MS);
    harness.add_worker("esp32-1", ChipFamily::Esp32, &[]);
    harness.add_worker(
        "host-f1",
        ChipFamily::Stm32F1,
        &[(PinId::port(Port::A, 5), Level::High)],
    );
    harness.connect();
    Rc::new(RefCell::new(harness))
}

fn test_policy() -> Policy {
    Policy {
        allow_write: true,
        allow_tasks: true,
        allow_admin: false,
        worker_filter: None,
        deny_llm_relay: false,
    }
}

struct RealRun {
    outcome: AgentOutcome,
    /// 最近一次响应返回的 model（诊断用；由打印逻辑读取，clippy 分析可能认为未读）。
    #[allow(dead_code)]
    model: String,
    calls: Vec<CommandRequest>,
    results: Vec<CommandResult>,
}

fn run_agent(
    label: &str,
    harness: &Rc<RefCell<Harness>>,
    cfg: &LlmConfig,
    goal: &str,
    max_steps: u32,
) -> RealRun {
    let gateway = LoopbackGateway::new(harness.clone());
    let calls_handle = gateway.calls.clone();
    let results_handle = gateway.results.clone();
    let last_model = Rc::new(RefCell::new(String::new()));
    let chat = CountingChat {
        inner: LlmClient::new(Box::new(StdHttpClient::new()), cfg.clone()),
        last_model: last_model.clone(),
    };
    let agent_config = AgentConfig {
        model: cfg.model.clone(),
        max_steps,
        ddl_ms: 5_000,
        timeout_ms: Some(30_000),
        temperature: 0.0,
        max_tokens: 4_096,
    };
    let mut agent = Agent::new(
        chat,
        gateway,
        test_policy(),
        agent_config,
        TraceWriter::disabled(),
    );
    let outcome = agent
        .run(goal)
        .unwrap_or_else(|error| panic!("[{label}] agent 运行失败：{error}"));
    drop(agent);

    let model = last_model.borrow().clone();
    print_usage(label, &model, &outcome.usage);
    println!(
        "real_api[{label}] steps={} status={:?} final_text={:?}",
        outcome.steps, outcome.status, outcome.final_text
    );
    for record in &outcome.tool_calls {
        println!(
            "real_api[{label}] tool_call function={} worker={:?} name={:?} ok={} error_code={:?}",
            record.function, record.worker_id, record.name, record.ok, record.error_code
        );
    }
    RealRun {
        outcome,
        model,
        calls: calls_handle.borrow().clone(),
        results: results_handle.borrow().clone(),
    }
}

/// 直接向Worker发命令（绕过 Agent/LLM），用于验证真实Worker状态。
fn direct_command(
    harness: &Rc<RefCell<Harness>>,
    worker_id: &str,
    kind: CommandKind,
    name: &str,
    args: &str,
) -> CommandResult {
    let request = match kind {
        CommandKind::Tool => CommandRequest::tool(worker_id, name, args.as_bytes().to_vec()),
        CommandKind::Task => CommandRequest::task(worker_id, name, args.as_bytes().to_vec()),
        // Jump 的载荷是目标端点（JumpBody 编码），不走 args。
        CommandKind::Jump => CommandRequest::jump(worker_id, name),
    };
    let mut harness = harness.borrow_mut();
    harness.run_command(request, 5_000)
}

fn direct_tool(harness: &Rc<RefCell<Harness>>, worker_id: &str, tool: &str, args: &str) -> String {
    let result = direct_command(harness, worker_id, CommandKind::Tool, tool, args);
    assert!(
        result.ok(),
        "直接调用 {tool} {args} 失败：{:?}",
        result.failure
    );
    String::from_utf8_lossy(&result.payload).to_string()
}

fn args_text(request: &CommandRequest) -> String {
    String::from_utf8_lossy(&request.args).to_string()
}

fn require_final_text(outcome: &AgentOutcome) -> String {
    outcome
        .final_text
        .clone()
        .filter(|text| !text.trim().is_empty())
        .expect("Agent 完成时必须有非空最终回答")
}

// ---------------------------------------------------------------------------
// 7 项真实 API 验收
// ---------------------------------------------------------------------------

/// A：模型可用 + usage 非 0（GET /models + 一次最小 chat）。
#[test]
#[ignore = "真实 DeepSeek API；设置 SEKAI_REAL_API=1 与 DEEPSEEK_API_KEY 后运行"]
fn a_model_and_usage() {
    let Some(cfg) = real_config("A") else {
        return;
    };
    let mut client = LlmClient::new(Box::new(StdHttpClient::new()), cfg.clone());
    count_api_call();
    let models = client
        .list_models()
        .unwrap_or_else(|error| panic!("[A] GET /models 失败：{error}"));
    assert!(!models.is_empty(), "GET /models 返回空列表");
    println!("real_api[A] models={models:?}");

    count_api_call();
    let request = ChatRequest {
        model: cfg.model.clone(),
        messages: vec![
            Message::system("你是一个测试助手。"),
            Message::user("只回答两个字符：收到"),
        ],
        tools: Vec::new(),
        tool_choice: ToolChoice::None,
        temperature: 0.0,
        max_tokens: 32,
        disable_thinking: true,
    };
    let response = client
        .chat(&request)
        .unwrap_or_else(|error| panic!("[A] chat 失败：{error}"));
    record_usage(&response.usage);
    print_usage("A", &response.model, &response.usage);

    assert!(
        response.usage.total_tokens > 0,
        "usage.total_tokens 必须 > 0：{:?}",
        response.usage
    );
    assert!(!response.model.is_empty(), "响应必须带 model");
    assert!(
        response
            .content
            .as_deref()
            .map(|text| !text.trim().is_empty())
            .unwrap_or(false),
        "普通 chat 必须返回非空 content：{:?}",
        response
    );
    assert!(
        models.iter().any(|model| model == &cfg.model) || response.model == cfg.model,
        "配置的模型 {} 既不在 /models 列表也不等于响应 model {}（/models={models:?}）",
        cfg.model,
        response.model
    );
}

/// B：tool_choice 指定 worker_tool 时返回合法 tool_calls（参数 JSON 可解析）。
#[test]
#[ignore = "真实 DeepSeek API；设置 SEKAI_REAL_API=1 与 DEEPSEEK_API_KEY 后运行"]
fn b_tool_choice_function_protocol() {
    let Some(cfg) = real_config("B") else {
        return;
    };
    let mut client = LlmClient::new(Box::new(StdHttpClient::new()), cfg.clone());
    let request = ChatRequest {
        model: cfg.model.clone(),
        messages: vec![
            Message::system("你是 Sekai Worker控制助手。只返回工具调用，不要用自然语言回答。"),
            Message::user(
                "请调用 worker_tool，为Worker esp32-1 执行 gpio.info 查看 IO4 状态。只选择一个工具调用。",
            ),
        ],
        tools: tools::tool_defs(),
        tool_choice: ToolChoice::Function(String::from(tools::FN_WORKER_TOOL)),
        temperature: 0.0,
        max_tokens: 512,
        disable_thinking: true,
    };
    count_api_call();
    let response = client
        .chat(&request)
        .unwrap_or_else(|error| panic!("[B] chat（tool_choice=function）失败：{error}"));
    record_usage(&response.usage);
    print_usage("B", &response.model, &response.usage);

    assert!(
        !response.tool_calls.is_empty(),
        "tool_choice 指定 worker_tool 时必须返回 tool_calls：{:?}",
        response
    );
    for call in &response.tool_calls {
        assert_eq!(
            call.name,
            tools::FN_WORKER_TOOL,
            "tool_choice 指定 worker_tool，实际函数名为 {}",
            call.name
        );
        let parsed = tools::parse_call(&call.name, &call.arguments).unwrap_or_else(|error| {
            panic!(
                "[B] 工具参数 JSON 非法：{error}；arguments={}",
                call.arguments
            )
        });
        match &parsed {
            ParsedCall::WorkerTool {
                worker_id,
                name,
                args,
                ..
            } => println!(
                "real_api[B] tool_call id={} worker_id={worker_id} tool={name} args={args:?}",
                call.id
            ),
            other => panic!("[B] 解析结果不是 worker_tool：{other:?}"),
        }
    }
}

/// C：GPIO 输出 —— 目标配置 IO4 输出并输出高电平；直接 gpio.info 验证。
#[test]
#[ignore = "真实 DeepSeek API；设置 SEKAI_REAL_API=1 与 DEEPSEEK_API_KEY 后运行"]
fn c_gpio_output_is_verified_directly_on_worker() {
    let Some(cfg) = real_config("C") else {
        return;
    };
    let harness = single_esp32(&[]);
    let goal = "请对Worker esp32-1 执行以下操作，必须用工具真实执行，不要只描述：\
                1) 用 worker_tool 调用 gpio.config，args 为 'IO4 out'；\
                2) 用 worker_tool 调用 gpio.write，args 为 'IO4 1'；\
                3) 用 worker_tool 调用 gpio.info，args 为 'IO4'。\
                完成后简短汇报 IO4 的模式与电平。";
    let run = run_agent("C", &harness, &cfg, goal, 8);

    assert_eq!(run.outcome.status, AgentStatus::Completed, "agent 未完成");
    assert!(!run.outcome.had_worker_failure, "本场景不应出现Worker失败");
    require_final_text(&run.outcome);
    assert!(
        run.calls
            .iter()
            .any(|call| call.name == "gpio.config" && args_text(call).contains("out")),
        "轨迹缺少 gpio.config ... out：{:?}",
        run.calls
            .iter()
            .map(|call| (call.name.as_str(), args_text(call)))
            .collect::<Vec<_>>()
    );
    assert!(
        run.calls
            .iter()
            .any(|call| call.name == "gpio.write" && args_text(call).contains('1')),
        "轨迹缺少 gpio.write ... 1：{:?}",
        run.calls
            .iter()
            .map(|call| (call.name.as_str(), args_text(call)))
            .collect::<Vec<_>>()
    );

    // 直接查Worker状态（不信任模型文本）。
    let info = direct_tool(&harness, "esp32-1", "gpio.info", "IO4");
    println!("real_api[C] direct gpio.info IO4 -> {info}");
    assert!(
        info.contains("mode=output"),
        "gpio.info 缺少 mode=output：{info}"
    );
    assert!(info.contains("level=1"), "gpio.info 缺少 level=1：{info}");
    assert!(
        info.contains("configured=1"),
        "gpio.info 缺少 configured=1：{info}"
    );
    harness.borrow().assert_no_errors();
}

/// D：GPIO 输入 —— 虚拟输入 IO4=1；最终回答含 1，轨迹含 config in + read。
#[test]
#[ignore = "真实 DeepSeek API；设置 SEKAI_REAL_API=1 与 DEEPSEEK_API_KEY 后运行"]
fn d_gpio_input_is_read_and_reported() {
    let Some(cfg) = real_config("D") else {
        return;
    };
    let harness = single_esp32(&[(PinId::io(4), Level::High)]);
    let goal = "请对Worker esp32-1 执行以下操作，必须用工具真实执行：\
                1) 用 worker_tool 调用 gpio.config，args 为 'IO4 in'，把 IO4 配成输入；\
                2) 用 worker_tool 调用 gpio.read，args 为 'IO4'；\
                3) 在最终回答中写出读到的电平数字（0 或 1）。";
    let run = run_agent("D", &harness, &cfg, goal, 8);

    assert_eq!(run.outcome.status, AgentStatus::Completed, "agent 未完成");
    let final_text = require_final_text(&run.outcome);
    assert!(
        final_text.contains('1'),
        "最终回答应包含读到的电平 1：{final_text}"
    );
    assert!(
        run.calls
            .iter()
            .any(|call| call.name == "gpio.config" && args_text(call).contains("in")),
        "轨迹缺少 gpio.config ... in：{:?}",
        run.calls
            .iter()
            .map(|call| (call.name.as_str(), args_text(call)))
            .collect::<Vec<_>>()
    );
    assert!(
        run.calls
            .iter()
            .any(|call| call.name == "gpio.read" && args_text(call).contains("IO4")),
        "轨迹缺少 gpio.read IO4：{:?}",
        run.calls
            .iter()
            .map(|call| (call.name.as_str(), args_text(call)))
            .collect::<Vec<_>>()
    );
    assert!(
        run.outcome.tool_calls.iter().any(|record| {
            record.function == tools::FN_WORKER_TOOL
                && record.name.as_deref() == Some("gpio.read")
                && record.ok
        }),
        "agent 记录里没有成功的 gpio.read：{:?}",
        run.outcome.tool_calls
    );

    // 直接查Worker状态：IO4 是输入且外部电平为 1。
    let info = direct_tool(&harness, "esp32-1", "gpio.info", "IO4");
    println!("real_api[D] direct gpio.info IO4 -> {info}");
    assert!(
        info.contains("mode=input"),
        "gpio.info 缺少 mode=input：{info}"
    );
    assert!(info.contains("level=1"), "gpio.info 缺少 level=1：{info}");
    let read = direct_tool(&harness, "esp32-1", "gpio.read", "IO4");
    println!("real_api[D] direct gpio.read IO4 -> {read}");
    assert_eq!(read.trim(), "pin=IO4 level=1");
    harness.borrow().assert_no_errors();
}

/// E：Task echo —— 真实下发 echo hello，结果载荷为 hello，轨迹含 worker_task。
#[test]
#[ignore = "真实 DeepSeek API；设置 SEKAI_REAL_API=1 与 DEEPSEEK_API_KEY 后运行"]
fn e_task_echo_payload_verified_directly() {
    let Some(cfg) = real_config("E") else {
        return;
    };
    let harness = single_esp32(&[]);
    let goal = "请对Worker esp32-1 下发 Task：任务名 echo，args 为 'hello'（用 worker_task）。\
                然后把任务结果原样告诉我。";
    let run = run_agent("E", &harness, &cfg, goal, 8);

    assert_eq!(run.outcome.status, AgentStatus::Completed, "agent 未完成");
    let final_text = require_final_text(&run.outcome);
    assert!(
        final_text.to_lowercase().contains("hello"),
        "最终回答应包含任务结果 hello：{final_text}"
    );
    assert!(
        run.outcome.tool_calls.iter().any(|record| {
            record.function == tools::FN_WORKER_TASK
                && record.name.as_deref() == Some("echo")
                && record.ok
        }),
        "agent 记录里没有成功的 worker_task/echo：{:?}",
        run.outcome.tool_calls
    );

    let echo = run
        .results
        .iter()
        .find(|result| result.kind == CommandKind::Task && result.name == "echo")
        .unwrap_or_else(|| panic!("没有实际下发 echo Task：{:?}", run.calls));
    println!(
        "real_api[E] direct task result payload={:?} phases={:?}",
        String::from_utf8_lossy(&echo.payload),
        echo.phase_names()
    );
    assert!(echo.ok(), "echo Task 失败：{:?}", echo.failure);
    assert_eq!(
        echo.payload.as_slice(),
        b"hello",
        "echo 结果载荷必须是 hello"
    );
    assert!(
        echo.phase_names().contains(&"Started"),
        "Task 轨迹应包含 Started：{:?}",
        echo.phase_names()
    );
    assert!(
        echo.phase_names().contains(&"Completed"),
        "Task 轨迹应包含 Completed：{:?}",
        echo.phase_names()
    );
    harness.borrow().assert_no_errors();
}

/// F：保留引脚 IO6 —— Worker返回 InvalidArgument，回答解释失败，IO6 状态不变。
#[test]
#[ignore = "真实 DeepSeek API；设置 SEKAI_REAL_API=1 与 DEEPSEEK_API_KEY 后运行"]
fn f_reserved_pin_failure_is_explained() {
    let Some(cfg) = real_config("F") else {
        return;
    };
    let harness = single_esp32(&[]);
    let goal = "请对Worker esp32-1 执行：用 worker_tool 调用 gpio.config，args 为 'IO6 out'。\
                如果Worker返回错误，请在最终回答中如实说明失败原因与错误码；不要换其他引脚。";
    let run = run_agent("F", &harness, &cfg, goal, 8);

    assert_eq!(run.outcome.status, AgentStatus::Completed, "agent 未完成");
    assert!(
        run.outcome.had_worker_failure,
        "保留引脚场景应出现Worker失败：{:?}",
        run.outcome.tool_calls
    );
    let failed = run
        .outcome
        .tool_calls
        .iter()
        .find(|record| record.name.as_deref() == Some("gpio.config") && !record.ok)
        .expect("应有失败的 gpio.config 调用");
    assert_eq!(
        failed.error_code.as_deref(),
        Some("worker_failed"),
        "服务端失败码应为 worker_failed：{failed:?}"
    );

    let final_text = require_final_text(&run.outcome);
    assert!(
        final_text.contains("IO6"),
        "最终回答应提到 IO6：{final_text}"
    );
    let lower = final_text.to_lowercase();
    assert!(
        [
            "失败",
            "错误",
            "保留",
            "无法",
            "拒绝",
            "不支持",
            "invalidargument",
            "invalid"
        ]
        .iter()
        .any(|needle| lower.contains(needle)),
        "最终回答应解释失败原因：{final_text}"
    );

    // 直接查Worker状态：IO6 仍未配置（未变成输出）。
    let info = direct_tool(&harness, "esp32-1", "gpio.info", "IO6");
    println!("real_api[F] direct gpio.info IO6 -> {info}");
    assert!(
        info.contains("mode=unconfigured"),
        "保留引脚状态必须保持 unconfigured：{info}"
    );
    assert!(
        !info.contains("mode=output"),
        "IO6 不应被配置为输出：{info}"
    );
    harness.borrow().assert_no_errors();
}

/// G：多Worker —— 目标指定 host-f1 的 PA5；只改该Worker，另一台状态不变。
#[test]
#[ignore = "真实 DeepSeek API；设置 SEKAI_REAL_API=1 与 DEEPSEEK_API_KEY 后运行"]
fn g_multi_worker_targeting_keeps_other_worker_untouched() {
    let Some(cfg) = real_config("G") else {
        return;
    };
    let harness = two_workers();
    let goal = "已连接两台 Sekai Worker。请只操作 host-f1：\
                用 worker_tool 调用 gpio.config，args 为 'PA5 in'；\
                再用 worker_tool 调用 gpio.read，args 为 'PA5'；\
                然后告诉我读到的电平（0 或 1）。不要对 esp32-1 发任何命令。";
    let run = run_agent("G", &harness, &cfg, goal, 8);

    assert_eq!(run.outcome.status, AgentStatus::Completed, "agent 未完成");
    let final_text = require_final_text(&run.outcome);
    assert!(
        final_text.contains('1'),
        "最终回答应包含读到的电平 1：{final_text}"
    );
    assert!(!run.calls.is_empty(), "agent 没有向任何Worker下发命令");
    let targets: Vec<(&str, &str)> = run
        .calls
        .iter()
        .map(|call| (call.worker_id.as_str(), call.name.as_str()))
        .collect();
    assert!(
        run.calls.iter().all(|call| call.worker_id == "host-f1"),
        "只应对 host-f1 发命令，实际：{targets:?}"
    );
    assert!(
        run.calls
            .iter()
            .any(|call| call.name == "gpio.config" && args_text(call).contains("PA5")),
        "轨迹缺少 gpio.config PA5：{targets:?}"
    );
    assert!(
        run.calls
            .iter()
            .any(|call| call.name == "gpio.read" && args_text(call).contains("PA5")),
        "轨迹缺少 gpio.read PA5：{targets:?}"
    );
    assert!(
        run.outcome
            .tool_calls
            .iter()
            .all(|record| record.worker_id.as_deref() != Some("esp32-1")),
        "agent 记录里不应出现 esp32-1：{:?}",
        run.outcome.tool_calls
    );

    // broker 记录：不得有发往 esp32-1 的 in（先查记录，再做直接查询）。
    let esp32_in = {
        let harness = harness.borrow();
        harness
            .recorded_publishes()
            .iter()
            .filter(|record| record.topic.ends_with("/in"))
            .filter(|record| record.topic.contains("/esp32-1/"))
            .count()
    };
    assert_eq!(esp32_in, 0, "不应向 esp32-1 发布任何命令");

    // 直接查两台Worker状态。
    let host_info = direct_tool(&harness, "host-f1", "gpio.info", "PA5");
    println!("real_api[G] direct gpio.info host-f1 PA5 -> {host_info}");
    assert!(
        host_info.contains("mode=input"),
        "host-f1 PA5 应为输入：{host_info}"
    );
    assert!(
        host_info.contains("level=1"),
        "host-f1 PA5 外部电平应为 1：{host_info}"
    );
    let host_read = direct_tool(&harness, "host-f1", "gpio.read", "PA5");
    println!("real_api[G] direct gpio.read host-f1 PA5 -> {host_read}");
    assert_eq!(host_read.trim(), "pin=PA5 level=1");

    let esp_info = direct_tool(&harness, "esp32-1", "gpio.info", "IO4");
    println!("real_api[G] direct gpio.info esp32-1 IO4 -> {esp_info}");
    assert!(
        esp_info.contains("mode=unconfigured"),
        "esp32-1 状态不应被改变：{esp_info}"
    );
    harness.borrow().assert_no_errors();
}
