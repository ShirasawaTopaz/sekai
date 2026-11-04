//! sekai-router CLI。
//!
//! 三种运行形态：
//!
//! - **批处理**（`--goal` / `--goal-file` / `--dry-run`）：建链路 → 等 Worker
//!   →（可选 dry-run 输出 Worker）→ 构造 LLM 客户端与 agent → 执行目标
//!   → 输出结果/退出码。链路由 `--link mqtt|stdio|tcp` 选择。不起 prompt
//!   ingress（进程短命）。
//! - **交互会话**（无 `--goal` 或 `-i`）：行式 REPL（`/link` 建链路、
//!   `/workers` 观察、`/wait` 等上线，非 `/` 输入即目标，多轮执行）。
//!   默认另起 prompt ingress（`--prompt-addr`，默认 127.0.0.1:7422）。
//! - **无头服务**（`--serve`）：stdin 读 JSONL 请求、stdout 只写 JSONL
//!   （协议 v1），供 DSH 插件子进程驱动；`prompt` 方法（v1.1 additive）
//!   与 prompt ingress 由 serve 内部驱动。

use std::sync::Arc;
use std::time::Duration;

use sekai_router::agent::{Agent, AgentConfig, AgentError, AgentOutcome, AgentStatus};
use sekai_router::config::{self, Config, LinkMode, Parsed};
use sekai_router::gateway::{GatewayError, WorkerGateway};
use sekai_router::http::StdHttpClient;
use sekai_router::json::Json;
use sekai_router::link_builder::{self, LinkOptions};
use sekai_router::llm::{LlmClient, LlmConfig};
use sekai_router::prompt::worker_summary;
use sekai_router::prompt_server::{self, PromptCore};
use sekai_router::serve::{self, ServeConfig};
use sekai_router::session::{Session, SessionConfig};
use sekai_router::tools::{Policy, workers_json};
use sekai_router::trace::TraceWriter;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let code = run(&args);
    std::process::exit(code);
}

fn run(args: &[String]) -> i32 {
    match config::parse(args) {
        Ok(Parsed::Help) => {
            println!("{}", config::usage());
            0
        }
        Ok(Parsed::Version) => {
            println!("{}", config::version_text());
            0
        }
        Ok(Parsed::Run(config)) => run_config(*config),
        Err(error) => {
            eprintln!("sekai-router: {error}");
            if matches!(
                error,
                config::ConfigError::InvalidArg(_) | config::ConfigError::Conflict(_)
            ) {
                eprintln!();
                eprintln!("{}", config::usage());
            }
            error.exit_code()
        }
    }
}

fn run_config(config: Config) -> i32 {
    // --serve：无头 JSONL 服务（不预建链路、不用 LLM/trace；
    // 链路建立/切换由 JSONL link 请求驱动）。
    if config.serve {
        return serve::run(ServeConfig::from_config(&config), policy_from(&config));
    }
    let trace = match TraceWriter::new(config.trace_path.as_deref()) {
        Ok(trace) => trace,
        Err(error) => {
            eprintln!(
                "sekai-router: 无法打开 trace 文件 {}：{error}",
                config
                    .trace_path
                    .as_deref()
                    .map(|path| path.display().to_string())
                    .unwrap_or_default()
            );
            return 2;
        }
    };
    let mut trace = trace;

    if config.interactive {
        run_interactive(config, trace)
    } else {
        run_batch(config, &mut trace)
    }
}

// ---- 网关构造（按链路类型） ----

/// CLI 旗标 → 服务端策略（批处理 / 交互 / serve 共用）。
///
/// `deny_llm_relay` 是 prompt 通道运行时内部位，CLI 不暴露、恒为 false。
fn policy_from(config: &Config) -> Policy {
    Policy {
        allow_write: config.allow_write,
        allow_tasks: config.allow_tasks,
        allow_admin: config.allow_admin,
        worker_filter: config.worker_filter.clone(),
        deny_llm_relay: false,
    }
}

/// CLI 配置 → 链路构造参数（装配统一走 link_builder 共享层）。
fn link_options(config: &Config) -> LinkOptions {
    LinkOptions {
        topic_root: config.topic_root.clone(),
        default_ddl_ms: config.ddl_ms,
        tool_timeout_ms: config.timeout_ms,
        task_timeout_ms: config.timeout_ms,
        worker_id_hint: config
            .worker_filter
            .clone()
            .unwrap_or_else(|| String::from("pc-worker")),
        mqtt_username: config.mqtt_username.clone(),
        mqtt_password: config.mqtt_password.clone(),
        mqtt_client_id: Some(format!(
            "sekai-router-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_millis())
                .unwrap_or(0)
        )),
    }
}

/// 按 `config.link` 构造网关（mqtt/stdio/tcp）。
fn build_gateway(config: &Config) -> Result<Box<dyn WorkerGateway>, GatewayError> {
    let options = link_options(config);
    match config.link {
        LinkMode::Mqtt => {
            let endpoint = config
                .mqtt
                .clone()
                .expect("config 已校验：mqtt 链路必有端点");
            let (gateway, _) = link_builder::connect_mqtt(&options, &endpoint)?;
            Ok(Box::new(gateway))
        }
        LinkMode::Stdio => {
            let (gateway, _) =
                link_builder::spawn_stdio(&options, &config.worker_bin, &config.worker_args)?;
            Ok(Box::new(gateway))
        }
        LinkMode::Tcp => {
            let (gateway, _) = link_builder::listen_tcp(&options, &config.listen_addr)?;
            Ok(Box::new(gateway))
        }
    }
}

// ---- 批处理 ----

fn run_batch(config: Config, trace: &mut TraceWriter) -> i32 {
    eprintln!(
        "sekai-router: 建立链路 {}（{})",
        config.link.as_str(),
        match config.link {
            LinkMode::Mqtt => config
                .mqtt
                .as_ref()
                .map(|endpoint| format!("{}:{}", endpoint.host, endpoint.port))
                .unwrap_or_default(),
            LinkMode::Stdio => config.worker_bin.clone(),
            LinkMode::Tcp => config.listen_addr.clone(),
        }
    );
    let mut gateway = match build_gateway(&config) {
        Ok(gateway) => gateway,
        Err(error) => {
            eprintln!("sekai-router: 链路建立失败：{error}");
            return 3;
        }
    };
    if let Err(error) = gateway.wait_ready(config.wait_worker_ms) {
        eprintln!("sekai-router: 链路未就绪：{error}");
        return 3;
    }

    let workers = wait_for_workers(&mut *gateway, &config);
    if workers.is_empty() {
        eprintln!(
            "sekai-router: {}ms 内没有发现Worker（检查 Worker 是否已连接并发布 Hello）",
            config.wait_worker_ms
        );
        return 3;
    }
    if let Some(filter) = &config.worker_filter
        && !workers.iter().any(|worker| worker.worker_id == *filter)
    {
        eprintln!("sekai-router: --worker {filter} 未出现在已发现Worker中");
        return 3;
    }
    trace.event(
        "workers_discovered",
        vec![(
            "workers",
            Json::Array(
                workers
                    .iter()
                    .map(|worker| Json::from(worker.worker_id.as_str()))
                    .collect(),
            ),
        )],
    );

    if config.dry_run {
        if config.json {
            let payload = ok_json(sekai_router::json!({
                "ok": true,
                "dry_run": true,
                "workers": workers_json(&workers),
            }));
            println!("{payload}");
        } else {
            println!("{}", worker_summary(&workers));
        }
        return 0;
    }

    let http = StdHttpClient::new();
    if http.https_helper_name().is_none() && config.base_url.starts_with("https://") {
        eprintln!(
            "sekai-router: 当前环境没有 HTTPS helper（node/curl）；无法访问 {}",
            config.base_url
        );
        return 4;
    }
    let llm_config = build_llm_config(&config);
    let llm = LlmClient::new(Box::new(http), llm_config);
    let policy = policy_from(&config);
    eprintln!(
        "sekai-router: 策略 allow_write={} allow_tasks={} allow_admin={} worker={:?}",
        policy.allow_write, policy.allow_tasks, policy.allow_admin, policy.worker_filter
    );
    let agent_config = build_agent_config(&config);
    let mut agent = Agent::new(llm, gateway, policy, agent_config, trace.clone());
    let goal = config.goal_text().to_string();
    eprintln!("sekai-router: 目标：{goal}");
    match agent.run(&goal) {
        Ok(outcome) => report_outcome(&config, &outcome),
        Err(error) => report_agent_error(&error),
    }
}

fn wait_for_workers(
    gateway: &mut dyn WorkerGateway,
    config: &Config,
) -> Vec<sekai_router::worker::WorkerInfo> {
    let started = std::time::Instant::now();
    let timeout = Duration::from_millis(config.wait_worker_ms.max(1));
    loop {
        let workers = gateway.list_workers();
        let matched = match &config.worker_filter {
            Some(filter) => workers.iter().any(|worker| worker.worker_id == *filter),
            None => !workers.is_empty(),
        };
        if matched || started.elapsed() >= timeout {
            return workers;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

fn report_outcome(config: &Config, outcome: &AgentOutcome) -> i32 {
    if config.json {
        let tool_calls: Vec<Json> = outcome
            .tool_calls
            .iter()
            .map(|record| {
                sekai_router::json!({
                    "function": record.function.as_str(),
                    "worker_id": optional_string(record.worker_id.clone()),
                    "name": optional_string(record.name.clone()),
                    "ok": record.ok,
                    "error_code": optional_string(record.error_code.clone()),
                })
            })
            .collect();
        let usage = sekai_router::json!({
            "prompt_tokens": outcome.usage.prompt_tokens,
            "completion_tokens": outcome.usage.completion_tokens,
            "total_tokens": outcome.usage.total_tokens,
        });
        let payload = ok_json(sekai_router::json!({
            "ok": outcome.status == AgentStatus::Completed,
            "status": if outcome.status == AgentStatus::Completed { "completed" } else { "max_steps" },
            "answer": optional_string(outcome.final_text.clone()),
            "steps": outcome.steps as u64,
            "usage": usage,
            "tool_calls": tool_calls,
        }));
        println!("{payload}");
    } else if let Some(text) = &outcome.final_text {
        println!("{text}");
    }
    eprintln!(
        "sekai-router: status={:?} steps={} prompt_tokens={} completion_tokens={} total_tokens={} tool_calls={} had_failure={}",
        outcome.status,
        outcome.steps,
        outcome.usage.prompt_tokens,
        outcome.usage.completion_tokens,
        outcome.usage.total_tokens,
        outcome.tool_calls.len(),
        outcome.had_worker_failure
    );
    match outcome.status {
        AgentStatus::Completed => {
            // 计划的退出码 6：最终Worker命令失败（Failed / 超时）。
            // 若模型在失败后继续查询并给出解释，最后一次调用通常是成功的，
            // 此时按成功退出；只有循环在失败调用后直接结束时才返回 6。
            if outcome.had_worker_failure && outcome.last_tool_failed {
                6
            } else {
                0
            }
        }
        AgentStatus::MaxSteps => 5,
    }
}

fn report_agent_error(error: &AgentError) -> i32 {
    eprintln!("sekai-router: {error}");
    match error {
        AgentError::Llm(_) => 4,
        AgentError::Gateway(_) => 6,
        AgentError::EmptyResponse => 4,
    }
}

// ---- 交互会话 ----

/// CLI 配置 → LlmConfig（批处理 / 交互 / prompt 通道共用形状）。
fn build_llm_config(config: &Config) -> LlmConfig {
    LlmConfig {
        base_url: config.base_url.clone(),
        model: config.model.clone(),
        api_key: config.api_key.clone().unwrap_or_default(),
        max_tokens: 4_096,
        temperature: 0.0,
        timeout: Duration::from_millis(config.http_timeout_ms),
        retry_base_ms: 1_000,
        max_retries: 3,
    }
}

/// CLI 配置 → Agent 配置。
fn build_agent_config(config: &Config) -> AgentConfig {
    AgentConfig {
        model: config.model.clone(),
        max_steps: config.max_steps,
        ddl_ms: config.ddl_ms,
        timeout_ms: config.timeout_ms,
        temperature: 0.0,
        max_tokens: 4_096,
    }
}

/// 构造交互会话（REPL 与 TUI 共用；TUI 非 TTY 回退时重建一次）。
///
/// `chat` 由调用方传入（与 prompt 通道共享同一 `LlmClient` 克隆）。
fn build_session(
    config: &Config,
    trace: TraceWriter,
    chat: LlmClient,
    policy: &Policy,
    agent_config: &AgentConfig,
) -> Session<LlmClient> {
    let session_config = SessionConfig {
        listen_addr: config.listen_addr.clone(),
        worker_bin: config.worker_bin.clone(),
        worker_args: config.worker_args.clone(),
        mqtt: config.mqtt.clone(),
        mqtt_username: config.mqtt_username.clone(),
        mqtt_password: config.mqtt_password.clone(),
        topic_root: config.topic_root.clone(),
        wait_worker_ms: config.wait_worker_ms,
        default_ddl_ms: config.ddl_ms,
        tool_timeout_ms: config.timeout_ms,
        task_timeout_ms: config.timeout_ms,
    };
    Session::new(
        chat,
        policy.clone(),
        agent_config.clone(),
        trace,
        session_config,
    )
}

/// 按配置启动 prompt ingress（交互模式）；打印「提示通道已监听 <addr>」。
///
/// 监听失败（端口占用等）**只打 stderr 中文错误并继续运行**：交互 REPL
/// 主流程不受影响（这是「报错继续」与「非 0 退出」之间的明确取舍，
/// 用户不应因一个旁路通道不可用而丢掉整个会话）。
fn start_prompt_ingress(config: &Config, hub: Arc<PromptCore<LlmClient>>) {
    let Some(addr) = config.prompt_addr.as_deref() else {
        return;
    };
    match prompt_server::start_ingress(hub, addr) {
        Ok(actual) => println!("提示通道已监听 {actual}"),
        Err(error) => {
            eprintln!("sekai-router: 提示通道监听失败（{addr}）：{error}；交互会话继续运行");
        }
    }
}

/// `--link` 旗标对应的初始 `/link` 命令（交互内仍可 `/link` 切换）。
fn initial_link_command(config: &Config) -> Option<String> {
    match config.link {
        LinkMode::Mqtt => config.mqtt.as_ref().map(|_| String::from("/link mqtt")),
        LinkMode::Stdio => Some(String::from("/link stdio")),
        LinkMode::Tcp => Some(String::from("/link tcp")),
    }
}

/// 在进入行式/TUI 前端前执行初始建链并打印输出。
///
/// TUI 模式下这些行打到主屏（退出 TUI 后可在滚回区看到），链路状态
/// 本身由 TUI 状态栏即时呈现。
fn run_initial_link(session: &mut Session<LlmClient>, command: Option<&str>) {
    if let Some(command) = command {
        for message in session.execute(command) {
            if let sekai_router::session::SessionMessage::Line(text) = message {
                println!("{text}");
            }
        }
    }
}

fn run_interactive(config: Config, trace: TraceWriter) -> i32 {
    let http = StdHttpClient::new();
    if http.https_helper_name().is_none() && config.base_url.starts_with("https://") {
        // 交互模式不退出：提示后继续（LLM 调用时才会真正失败）。
        eprintln!(
            "sekai-router: 注意：当前环境没有 HTTPS helper（node/curl），访问 {} 会失败",
            config.base_url
        );
    }
    // LLM 客户端只建一次：会话 Agent 与 prompt 通道共用同一克隆
    // （LlmClient 内部共享 HTTP 执行器）。
    let llm = LlmClient::new(Box::new(http), build_llm_config(&config));
    let policy = policy_from(&config);
    let agent_config = build_agent_config(&config);
    // prompt 通道共享核心：ingress 与会话在 set_gateway / /policy 变更时同步。
    let hub = Arc::new(PromptCore::new(
        llm.clone(),
        policy.clone(),
        agent_config.clone(),
        trace.clone(),
    ));
    start_prompt_ingress(&config, Arc::clone(&hub));

    let initial = initial_link_command(&config);
    let mut session = build_session(&config, trace.clone(), llm.clone(), &policy, &agent_config);
    session.set_prompt_hub(Arc::clone(&hub));
    run_initial_link(&mut session, initial.as_deref());
    // --tui：优先全屏 TUI；stdin/stdout 任一非终端时 tui 返回
    // EXIT_NOT_TTY，重建 Session 回退行式 REPL（Session 非 Clone；
    // hub 复用同一实例，ingress 不断线）。
    if config.tui {
        let code = sekai_router::tui::run(session);
        if code != sekai_router::tui::EXIT_NOT_TTY {
            return code;
        }
        let mut session = build_session(&config, trace, llm, &policy, &agent_config);
        session.set_prompt_hub(hub);
        run_initial_link(&mut session, initial.as_deref());
        return sekai_router::repl::run(session);
    }
    sekai_router::repl::run(session)
}

fn ok_json(value: Json) -> String {
    value.to_compact_string()
}

fn optional_string(value: Option<String>) -> Json {
    value.map(Json::from).unwrap_or(Json::Null)
}
