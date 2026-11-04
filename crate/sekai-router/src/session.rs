//! 会话命令层：把 REPL / 未来 TUI 的用户输入解释成动作与输出。
//!
//! 纯逻辑、无直接 IO：[`Session::execute`] 吃一行输入，返回一组
//! [`SessionMessage`]（输出行或退出指令）。行式 REPL（[`crate::repl`]）
//! 与后续全屏 TUI 共用这一层，前端只负责渲染与提示符。
//!
//! ## 命令集
//!
//! ```text
//! /help / /status / /link            状态与帮助
//! /start / /connect                  引导式建连向导（菜单选 stdio/tcp/mqtt）
//! /link tcp [HOST:PORT]              监听等 Worker 连入（默认 127.0.0.1:7421）
//! /link stdio [BIN [-- ARGS...]]     拉起 Worker 子进程（默认 sekai-worker）
//! /link mqtt [ADDR]                  连 MQTT broker（默认取 SEKAI_MQTT_ADDR）
//! /link off                          关闭当前链路
//! /workers                           列出已发现 Worker
//! /wait [MS]                         阻塞等待 Worker 上线
//! /cancel [id|last]                  取消当前后台目标（或指定 cmd_id 的在途命令）
//! /policy [NAME on|off]              查看/切换 allow-write|allow-tasks|allow-admin
//! /exit / /quit                      退出会话
//! <非 / 开头输入>                     自然语言目标 → Agent 回路
//! ```
//!
//! 前端（[`crate::repl`] / [`crate::tui`]）用 [`Session::submit_line`] 提交
//! 普通输入：目标在**后台线程**里跑 Agent，[`Session::poll`] 消费完成事件，
//! 期间界面保持可响应；[`Session::cancel_goal`] / `/cancel` 触发协作式取消
//! （agent 会先向 Worker 下发 `sekai.cancel`）。[`Session::execute`] 保留
//! 同步语义（测试与批处理用）。

use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::{Duration, Instant};

use sekai::sekai_device_list::Endpoint;

use crate::agent::{Agent, AgentConfig, AgentError, AgentOutcome, AgentStatus, CancelToken};
use crate::gateway::{GatewayHandle, WorkerGateway};
use crate::link_builder::{self, LinkOptions};
use crate::llm::ChatClient;
use crate::prompt::worker_summary;
use crate::prompt_server::PromptCore;
use crate::tools::Policy;
use crate::topic::TopicRoot;
use crate::trace::TraceWriter;

/// 会话输出消息（前端负责渲染）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionMessage {
    /// 一行输出（不含换行）。
    Line(String),
    /// 会话应当退出。
    Exit,
}

/// 会话默认值（链路建立参数、等待窗口等）。
#[derive(Debug, Clone)]
pub struct SessionConfig {
    /// `/link tcp` 默认监听地址。
    pub listen_addr: String,
    /// `/link stdio` 默认 Worker 程序。
    pub worker_bin: String,
    /// `/link stdio` 追加参数（空格分隔）。
    pub worker_args: Vec<String>,
    /// `/link mqtt` 默认 broker 端点（来自 SEKAI_MQTT_ADDR）。
    pub mqtt: Option<Endpoint>,
    /// MQTT 用户名。
    pub mqtt_username: Option<String>,
    /// MQTT 密码。
    pub mqtt_password: Option<String>,
    /// 主题根。
    pub topic_root: TopicRoot,
    /// `/wait` 默认窗口（毫秒）。
    pub wait_worker_ms: u64,
    /// 默认 DDL。
    pub default_ddl_ms: u32,
    /// Tool 超时覆盖。
    pub tool_timeout_ms: Option<u64>,
    /// Task 超时覆盖。
    pub task_timeout_ms: Option<u64>,
}

impl SessionConfig {
    /// 最小默认集（REPL 测试 / 无环境变量场景）。
    pub fn minimal() -> Self {
        Self {
            listen_addr: String::from("127.0.0.1:7421"),
            worker_bin: String::from("sekai-worker"),
            worker_args: Vec::new(),
            mqtt: None,
            mqtt_username: None,
            mqtt_password: None,
            topic_root: TopicRoot::new("sekai/v1").expect("默认主题根合法"),
            wait_worker_ms: 10_000,
            default_ddl_ms: crate::config::DEFAULT_DDL_MS,
            tool_timeout_ms: None,
            task_timeout_ms: None,
        }
    }
}

/// 后台目标执行状态（[`Session::start_goal`] 创建；[`Session::poll`] 消费）。
struct GoalRun {
    receiver: Receiver<Vec<SessionMessage>>,
    started: Instant,
}

/// REPL / TUI 会话状态机。
pub struct Session<C: ChatClient + Clone> {
    chat: C,
    gateway: Option<GatewayHandle>,
    /// 当前链路描述（人读，`/link` 展示）。
    link_desc: String,
    policy: Policy,
    agent_config: AgentConfig,
    trace: TraceWriter,
    config: SessionConfig,
    /// `/start` 建连向导挂起态（`None` 表示未挂起）。
    wizard: Option<LinkWizard>,
    /// 上次看到的 Worker id 快照（[`Session::poll`] 据此报「已上线」）。
    last_workers: Vec<String>,
    /// prompt 通道共享核心（可选；ingress 由 bin 构造并注入，
    /// 建链/关链/策略变更时同步，见 [`Session::sync_hub`]）。
    hub: Option<Arc<PromptCore<C>>>,
    /// 后台目标的协作式取消令牌（一次目标一个）。
    cancel: CancelToken,
    /// 正在后台执行的目标（None = 空闲）。
    goal: Option<GoalRun>,
}

impl<C: ChatClient + Clone> Session<C> {
    /// 构造会话。
    pub fn new(
        chat: C,
        policy: Policy,
        agent_config: AgentConfig,
        trace: TraceWriter,
        config: SessionConfig,
    ) -> Self {
        Self {
            chat,
            gateway: None,
            link_desc: String::from("未连接"),
            policy,
            agent_config,
            trace,
            config,
            wizard: None,
            last_workers: Vec::new(),
            hub: None,
            cancel: CancelToken::new(),
            goal: None,
        }
    }

    /// 注入网关（测试 / 嵌入场景）；`desc` 为链路描述。
    pub fn set_gateway(&mut self, gateway: GatewayHandle, desc: impl Into<String>) {
        self.link_desc = desc.into();
        self.gateway = Some(gateway);
        // 注入即基线：poll 只报此后新增的 Worker。
        self.refresh_worker_baseline();
        self.sync_hub();
    }

    /// 挂接 prompt 通道共享核心（bin 在启动 ingress 前注入）；
    /// 注入时同步当前网关与策略。
    pub fn set_prompt_hub(&mut self, hub: Arc<PromptCore<C>>) {
        self.hub = Some(hub);
        self.sync_hub();
    }

    /// 当前策略（测试用）。
    pub fn policy(&self) -> &Policy {
        &self.policy
    }

    /// 当前链路描述（测试用）。
    pub fn link_desc(&self) -> &str {
        &self.link_desc
    }

    /// 排空异步更新（REPL 在每条命令后、TUI 在空闲时调用）。
    ///
    /// 先消费后台目标完成事件（若有），再对比上次快照与
    /// [`GatewayHandle::list_workers`] 报新出现的 Worker；无网关或无变化返回空。
    pub fn poll(&mut self) -> Vec<SessionMessage> {
        let mut messages = self.drain_goal_events();
        messages.extend(self.poll_workers());
        messages
    }

    /// 取后台目标的完成输出（非阻塞）；无目标 / 未完成返回空。
    fn drain_goal_events(&mut self) -> Vec<SessionMessage> {
        let Some(goal) = self.goal.as_ref() else {
            return Vec::new();
        };
        match goal.receiver.try_recv() {
            Ok(messages) => {
                self.goal = None;
                messages
            }
            Err(TryRecvError::Empty) => Vec::new(),
            Err(TryRecvError::Disconnected) => {
                self.goal = None;
                vec![SessionMessage::Line(String::from(
                    "后台执行线程已退出（无结果）",
                ))]
            }
        }
    }

    fn poll_workers(&mut self) -> Vec<SessionMessage> {
        let Some(mut gateway) = self.gateway.clone() else {
            return Vec::new();
        };
        let workers = gateway.list_workers();
        let mut messages = Vec::new();
        let mut current = Vec::with_capacity(workers.len());
        for worker in &workers {
            let id = worker.worker_id.clone();
            if !self.last_workers.contains(&id) {
                messages.push(SessionMessage::Line(format!("Worker {id} 已上线")));
            }
            current.push(id);
        }
        self.last_workers = current;
        messages
    }

    /// 把当前 Worker 集合记为 poll 基线（建链 / 注入 / `/wait` 宣告后调用）。
    fn refresh_worker_baseline(&mut self) {
        self.last_workers = match self.gateway.as_mut() {
            Some(gateway) => gateway
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .list_workers()
                .into_iter()
                .map(|worker| worker.worker_id)
                .collect(),
            None => Vec::new(),
        };
    }

    /// 当前链路是否就绪（TUI 状态栏用）；无网关时为 `false`。
    pub fn gateway_ready(&self) -> bool {
        self.gateway
            .as_ref()
            .map(|gateway| {
                gateway
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .is_ready()
            })
            .unwrap_or(false)
    }

    /// 当前已发现的 Worker 数（TUI 状态栏用）；无网关时为 `0`。
    pub fn worker_count(&self) -> usize {
        self.gateway
            .as_ref()
            .map(|gateway| {
                gateway
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .list_workers()
                    .len()
            })
            .unwrap_or(0)
    }

    /// 解释一行输入；返回输出消息（可能为空）。
    ///
    /// `/start` 挂起态下：非 '/' 输入（含空行）由向导消费；'/' 命令取消
    /// 挂起态并照常执行。
    pub fn execute(&mut self, input: &str) -> Vec<SessionMessage> {
        let input = input.trim();
        if self.wizard.is_some() {
            if input.starts_with('/') {
                // 任何 '/' 命令取消向导，然后正常执行该命令。
                self.wizard = None;
                let mut out = vec![SessionMessage::Line(String::from("已取消建连向导"))];
                out.extend(self.execute_command(input));
                return out;
            }
            // 空行在参数步骤视为「回车用默认值」；菜单步骤提示重选。
            return self.wizard_feed(input);
        }
        self.execute_command(input)
    }

    /// 非挂起态的输入解释：空行忽略、'/' 命令分发、其余交给 Agent。
    fn execute_command(&mut self, input: &str) -> Vec<SessionMessage> {
        if input.is_empty() {
            return Vec::new();
        }
        if !input.starts_with('/') {
            return self.run_goal(input);
        }
        let tokens: Vec<&str> = input.split_whitespace().collect();
        match tokens[0] {
            "/help" | "/?" => self.help_lines(),
            "/exit" | "/quit" => vec![SessionMessage::Exit],
            "/start" | "/connect" => self.start_wizard(),
            "/link" => {
                if tokens.len() == 1 {
                    self.status_lines()
                } else {
                    self.link_command(&tokens[1..])
                }
            }
            "/status" => self.status_lines(),
            "/unlink" => self.link_off(),
            "/workers" => self.workers_lines(),
            "/wait" => self.wait_for_worker(tokens.get(1).copied()),
            "/cancel" => self.cancel_command(tokens.get(1).copied()),
            "/policy" => self.policy_command(&tokens[1..]),
            other => vec![SessionMessage::Line(format!(
                "未知命令 {other}（/help 查看命令列表）"
            ))],
        }
    }

    // ---- 命令实现 ----

    fn help_lines(&self) -> Vec<SessionMessage> {
        let text = "\
命令：
  /link                        查看当前链路状态
  /link tcp [HOST:PORT]        监听端口等待 Worker 连入（默认 {listen}）
  /link stdio [BIN [-- ARGS]]  拉起 Worker 子进程（默认 {worker_bin}）
  /link mqtt [HOST:PORT]       连接 MQTT broker（默认取 SEKAI_MQTT_ADDR）
  /link off                    关闭当前链路
  /start                       引导式建连向导（别名 /connect）
  /workers                     列出已发现的 Worker
  /wait [MS]                   等待 Worker 上线（默认 {wait}ms）
  /cancel [id|last]            取消当前后台目标；带 cmd_id 时取消该在途命令
  /policy [NAME on|off]        查看/切换权限（allow-write/allow-tasks/allow-admin）
  /exit                        退出会话
其它任何输入都会作为自然语言目标交给 Agent 执行（后台线程，可 /cancel 取消）。"
            .replace("{listen}", &self.config.listen_addr)
            .replace("{worker_bin}", &self.config.worker_bin)
            .replace("{wait}", &self.config.wait_worker_ms.to_string());
        text.lines()
            .map(|line| SessionMessage::Line(line.to_string()))
            .collect()
    }

    fn status_lines(&self) -> Vec<SessionMessage> {
        let mut lines = vec![SessionMessage::Line(format!("链路：{}", self.link_desc))];
        if let Some(mut gateway) = self.gateway.clone() {
            let workers = gateway.list_workers();
            lines.push(SessionMessage::Line(format!(
                "就绪={} Worker数={}",
                gateway.is_ready(),
                workers.len()
            )));
            // B8：诊断快照（在途账本 / 最近错误 / 晚到终态 / 时钟校准）。
            let diagnostics = gateway.diagnostics();
            if let Some(error) = &diagnostics.last_error {
                lines.push(SessionMessage::Line(format!("最近错误：{error}")));
            }
            if !diagnostics.inflight.is_empty() {
                lines.push(SessionMessage::Line(format!(
                    "在途命令 {} 条：",
                    diagnostics.inflight.len()
                )));
                for command in diagnostics.inflight.iter().take(8) {
                    lines.push(SessionMessage::Line(format!(
                        "  #{} {} {} 已运行 {}ms",
                        command.cmd_id, command.worker_id, command.name, command.elapsed_ms
                    )));
                }
            }
            lines.push(SessionMessage::Line(format!(
                "时钟校准：{} ｜ 晚到终态：{} 条（累计 {}）",
                if diagnostics.clock_synced {
                    "是"
                } else {
                    "否"
                },
                diagnostics.late_results.len(),
                diagnostics.counters.late_results
            )));
        } else {
            lines.push(SessionMessage::Line(String::from(
                "未连接任何链路（/link tcp|stdio|mqtt）",
            )));
        }
        if let Some(goal) = &self.goal {
            lines.push(SessionMessage::Line(format!(
                "目标执行中（{}ms）：/cancel 取消",
                goal.started.elapsed().as_millis()
            )));
        }
        lines
    }

    /// `/cancel [id|last]`：无参/last 取消当前后台目标；数字取消指定在途命令。
    fn cancel_command(&mut self, arg: Option<&str>) -> Vec<SessionMessage> {
        match arg {
            None | Some("last") => {
                if self.goal.is_none() {
                    return vec![SessionMessage::Line(String::from(
                        "当前没有在后台执行的目标（带 cmd_id 可取消指定在途命令）",
                    ))];
                }
                self.cancel.cancel();
                vec![
                    SessionMessage::Line(String::from("已请求取消当前目标")),
                    SessionMessage::Line(String::from(
                        "agent 会在 50ms 内向 Worker 下发 sekai.cancel 并停止后续派工",
                    )),
                ]
            }
            Some(text) => match text.parse::<u64>() {
                Ok(cmd_id) => {
                    let Some(mut gateway) = self.gateway.clone() else {
                        return vec![SessionMessage::Line(String::from(
                            "未连接（先 /link tcp|stdio|mqtt）",
                        ))];
                    };
                    match gateway.cancel(cmd_id) {
                        Ok(()) => vec![SessionMessage::Line(format!(
                            "已请求取消命令 {cmd_id}（sekai.cancel）"
                        ))],
                        Err(error) => vec![SessionMessage::Line(format!(
                            "取消命令 {cmd_id} 失败：{}",
                            error.message
                        ))],
                    }
                }
                Err(_) => vec![SessionMessage::Line(format!(
                    "/cancel 参数必须是 cmd_id 或 last：{text}"
                ))],
            },
        }
    }

    fn link_command(&mut self, tokens: &[&str]) -> Vec<SessionMessage> {
        match tokens[0] {
            "tcp" => self.link_tcp(tokens.get(1).copied()),
            "stdio" => self.link_stdio(&tokens[1..]),
            "mqtt" => self.link_mqtt(tokens.get(1).copied()),
            "off" | "none" => self.link_off(),
            other => vec![SessionMessage::Line(format!(
                "未知链路类型 {other}（可选 tcp/stdio/mqtt/off）"
            ))],
        }
    }

    fn link_tcp(&mut self, addr: Option<&str>) -> Vec<SessionMessage> {
        self.link_tcp_inner(addr).0
    }

    /// 建立 tcp 监听链路；返回（输出，是否成功）。
    ///
    /// 网关构造走 [`crate::link_builder`] 共享层（serve `link` / CLI `--link`
    /// 与此共用）。
    fn link_tcp_inner(&mut self, addr: Option<&str>) -> (Vec<SessionMessage>, bool) {
        let addr = addr.unwrap_or(&self.config.listen_addr).to_string();
        // 先关旧链路。
        self.link_off_quiet();
        match link_builder::listen_tcp(&self.link_options(), &addr) {
            Ok((gateway, actual)) => {
                // 实际地址（端口 0 自动分配、默认端口补全后）取自监听器，
                // 提示用户可直接复制。
                self.link_desc = format!("tcp 监听（{actual}）");
                self.gateway = Some(link_builder::handle_of(gateway));
                self.refresh_worker_baseline();
                self.sync_hub();
                (
                    vec![
                        SessionMessage::Line(format!("已监听 {actual}，等待 Worker 连入…")),
                        SessionMessage::Line(format!(
                            "在另一终端运行：sekai-worker connect {actual}"
                        )),
                    ],
                    true,
                )
            }
            Err(error) => (
                vec![SessionMessage::Line(format!("监听失败：{error}"))],
                false,
            ),
        }
    }

    fn link_stdio(&mut self, tokens: &[&str]) -> Vec<SessionMessage> {
        self.link_stdio_inner(tokens).0
    }

    /// 拉起 stdio Worker 子进程；返回（输出，是否成功）。
    fn link_stdio_inner(&mut self, tokens: &[&str]) -> (Vec<SessionMessage>, bool) {
        // 解析 [BIN] [-- ARGS...]
        let mut program: Option<&str> = None;
        let mut extra_args: Vec<String> = Vec::new();
        let mut after_dashdash = false;
        for token in tokens {
            if after_dashdash {
                extra_args.push((*token).to_string());
            } else if *token == "--" {
                after_dashdash = true;
            } else if program.is_none() {
                program = Some(token);
            } else {
                return (
                    vec![SessionMessage::Line(String::from(
                        "用法：/link stdio [BIN [-- ARGS...]]（多余参数请放到 -- 之后）",
                    ))],
                    false,
                );
            }
        }
        let program = program.unwrap_or(&self.config.worker_bin).to_string();
        // 默认参数（--worker-arg）在前，命令行附加参数在后。
        let mut spawn_args: Vec<String> = self.config.worker_args.clone();
        spawn_args.extend(extra_args);
        self.link_off_quiet();
        match link_builder::spawn_stdio(&self.link_options(), &program, &spawn_args) {
            Ok((gateway, _)) => {
                self.link_desc = format!("stdio 子进程（{program}）");
                // 子进程 Hello 通常在数百毫秒内到达；这里不阻塞，交给 /wait。
                self.gateway = Some(link_builder::handle_of(gateway));
                self.refresh_worker_baseline();
                self.sync_hub();
                (
                    vec![SessionMessage::Line(format!(
                        "已拉起 {program}（等待 Hello，用 /workers 查看）"
                    ))],
                    true,
                )
            }
            Err(error) => (
                vec![SessionMessage::Line(format!("拉起失败：{error}"))],
                false,
            ),
        }
    }

    fn link_mqtt(&mut self, addr: Option<&str>) -> Vec<SessionMessage> {
        self.link_mqtt_inner(addr).0
    }

    /// 连接 MQTT broker；返回（输出，是否成功）。
    fn link_mqtt_inner(&mut self, addr: Option<&str>) -> (Vec<SessionMessage>, bool) {
        let endpoint = match addr {
            Some(text) => match Endpoint::parse(text) {
                Ok(endpoint) => Some(endpoint),
                Err(error) => {
                    return (
                        vec![SessionMessage::Line(format!("地址非法：{error}"))],
                        false,
                    );
                }
            },
            None => self.config.mqtt.clone(),
        };
        let Some(endpoint) = endpoint else {
            return (
                vec![SessionMessage::Line(String::from(
                    "没有默认 broker 地址：/link mqtt HOST:PORT，或设置 SEKAI_MQTT_ADDR",
                ))],
                false,
            );
        };
        let display = format!("{}:{}", endpoint.host, endpoint.port);
        self.link_off_quiet();
        match link_builder::connect_mqtt(&self.link_options(), &endpoint) {
            Ok((gateway, _)) => {
                self.link_desc = format!("mqtt broker（{display}）");
                self.gateway = Some(link_builder::handle_of(gateway));
                self.refresh_worker_baseline();
                self.sync_hub();
                (
                    vec![
                        SessionMessage::Line(format!(
                            "正在连接 broker {display}（/wait 等待 Worker 的 retained Hello）"
                        )),
                        SessionMessage::Line(format!("Worker 侧请连接同一 broker：{display}")),
                    ],
                    true,
                )
            }
            Err(error) => (
                vec![SessionMessage::Line(format!("连接失败：{error}"))],
                false,
            ),
        }
    }

    fn link_off(&mut self) -> Vec<SessionMessage> {
        let had = self.link_off_quiet();
        if had {
            vec![SessionMessage::Line(String::from("已关闭链路"))]
        } else {
            vec![SessionMessage::Line(String::from("当前没有链路"))]
        }
    }

    /// 关闭当前链路（若有）；返回是否关闭了链路。
    fn link_off_quiet(&mut self) -> bool {
        if self.gateway.take().is_some() {
            // 旧句柄 Drop：MqttGateway/DirectGateway 在 Drop 中停 IO 线程。
            self.link_desc = String::from("未连接");
            self.last_workers.clear();
            self.sync_hub();
            true
        } else {
            false
        }
    }

    fn workers_lines(&self) -> Vec<SessionMessage> {
        let Some(mut gateway) = self.gateway.clone() else {
            return vec![SessionMessage::Line(String::from(
                "未连接（先 /link tcp|stdio|mqtt）",
            ))];
        };
        let summary = worker_summary(&gateway.list_workers());
        summary
            .lines()
            .map(|line| SessionMessage::Line(line.to_string()))
            .collect()
    }

    fn wait_for_worker(&mut self, timeout_arg: Option<&str>) -> Vec<SessionMessage> {
        let Some(mut gateway) = self.gateway.clone() else {
            return vec![SessionMessage::Line(String::from(
                "未连接（先 /link tcp|stdio|mqtt）",
            ))];
        };
        let timeout_ms = match timeout_arg {
            Some(text) => match text.parse::<u64>() {
                Ok(value) => value,
                Err(_) => {
                    return vec![SessionMessage::Line(format!(
                        "/wait 参数必须是毫秒数：{text}"
                    ))];
                }
            },
            None => self.config.wait_worker_ms,
        };
        let deadline = Instant::now() + Duration::from_millis(timeout_ms.max(1));
        loop {
            let workers = gateway.list_workers();
            if let Some(worker) = workers.iter().find(|worker| worker.online) {
                // 已向用户宣告上线；同步 poll 基线避免 poll 重复报。
                self.last_workers = workers.iter().map(|w| w.worker_id.clone()).collect();
                return vec![SessionMessage::Line(format!(
                    "Worker 已上线：{}（tools={} tasks={}）",
                    worker.worker_id,
                    worker.tools.join(","),
                    worker.tasks.join(",")
                ))];
            }
            if Instant::now() >= deadline {
                return vec![SessionMessage::Line(format!(
                    "等待超时（{timeout_ms}ms 内没有在线 Worker）"
                ))];
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn policy_command(&mut self, tokens: &[&str]) -> Vec<SessionMessage> {
        if tokens.is_empty() {
            let text = format!(
                "策略：allow_write={} allow_tasks={} allow_admin={} worker={:?}",
                self.policy.allow_write,
                self.policy.allow_tasks,
                self.policy.allow_admin,
                self.policy.worker_filter.clone().unwrap_or_default()
            );
            return vec![SessionMessage::Line(text)];
        }
        let name = tokens[0];
        let field = match name {
            "allow-write" | "allow_write" => &mut self.policy.allow_write,
            "allow-tasks" | "allow_tasks" => &mut self.policy.allow_tasks,
            "allow-admin" | "allow_admin" => &mut self.policy.allow_admin,
            other => {
                return vec![SessionMessage::Line(format!(
                    "未知策略 {other}（可选 allow-write/allow-tasks/allow-admin）"
                ))];
            }
        };
        match tokens.get(1).copied() {
            Some("on") | Some("true") => *field = true,
            Some("off") | Some("false") => *field = false,
            _ => {
                return vec![SessionMessage::Line(String::from(
                    "用法：/policy NAME on|off",
                ))];
            }
        }
        self.sync_hub();
        vec![SessionMessage::Line(format!("{name} 已更新"))]
    }

    // ---- 建连向导（/start）----

    /// 进入 `/start` 向导：打印当前链路状态 + 菜单，进入挂起态。
    fn start_wizard(&mut self) -> Vec<SessionMessage> {
        let defaults = WizardDefaults {
            listen_addr: self.config.listen_addr.clone(),
            worker_bin: self.config.worker_bin.clone(),
            mqtt_addr: self
                .config
                .mqtt
                .as_ref()
                .map(|endpoint| endpoint.to_string()),
        };
        let wizard = LinkWizard::new(defaults);
        let mut lines = wizard.menu_lines(&self.link_desc);
        lines.extend(wizard.prompt_lines());
        self.wizard = Some(wizard);
        lines.into_iter().map(SessionMessage::Line).collect()
    }

    /// 向导挂起态消费一行非 '/' 输入（调用方已保证挂起态）。
    fn wizard_feed(&mut self, input: &str) -> Vec<SessionMessage> {
        let Some(wizard) = self.wizard.as_mut() else {
            return Vec::new();
        };
        let outcome = wizard.feed(input);
        match outcome {
            // 停留当前步骤（非法输入 / 缺默认值）：输出错误后重新打印该步
            // 提示，便于直接重输。
            WizardOutcome::Stay(lines) => {
                let mut out = lines;
                out.extend(
                    self.wizard
                        .as_ref()
                        .map_or(Vec::new(), |w| w.prompt_lines()),
                );
                out.into_iter().map(SessionMessage::Line).collect()
            }
            WizardOutcome::Advance(lines) => lines.into_iter().map(SessionMessage::Line).collect(),
            WizardOutcome::Cancel(lines) => {
                self.wizard = None;
                lines.into_iter().map(SessionMessage::Line).collect()
            }
            WizardOutcome::Link(kind) => self.wizard_link(kind),
        }
    }

    /// 向导参数收齐后执行建链；成功则自动 `/wait` 并展示 `/workers`。
    fn wizard_link(&mut self, kind: WizardLink) -> Vec<SessionMessage> {
        let (messages, ok) = match &kind {
            WizardLink::Tcp(addr) => self.link_tcp_inner(Some(addr.as_str())),
            WizardLink::Mqtt(addr) => self.link_mqtt_inner(Some(addr.as_str())),
            WizardLink::Stdio(bin) => self.link_stdio_inner(&[bin.as_str()]),
        };
        let mut out = messages;
        if !ok {
            // 建链失败：向导停留在当前参数步骤，重新提示输入。
            out.extend(
                self.wizard
                    .as_ref()
                    .map_or(Vec::new(), |w| w.prompt_lines())
                    .into_iter()
                    .map(SessionMessage::Line),
            );
            return out;
        }
        self.wizard = None;
        out.push(SessionMessage::Line(String::from("正在等待 Worker 上线…")));
        out.extend(self.wait_for_worker(None));
        out.extend(self.workers_lines());
        out
    }

    // ---- 目标执行 ----

    fn run_goal(&mut self, goal: &str) -> Vec<SessionMessage> {
        if self.goal.is_some() {
            return vec![SessionMessage::Line(String::from(
                "已有目标在后台执行：/cancel 取消后再提交新目标",
            ))];
        }
        let Some(mut gateway) = self.gateway.clone() else {
            return vec![SessionMessage::Line(String::from(
                "未连接：先 /link tcp|stdio|mqtt 建立链路，再 /wait 等 Worker 上线",
            ))];
        };
        if gateway.list_workers().is_empty() {
            return vec![SessionMessage::Line(String::from(
                "当前没有已发现的 Worker：先 /wait 等待上线（或检查 Worker 是否已连接）",
            ))];
        }
        let mut agent = Agent::new(
            self.chat.clone(),
            gateway,
            self.policy.clone(),
            self.agent_config.clone(),
            self.trace.clone(),
        )
        .with_cancel(self.cancel.clone());
        match agent.run(goal) {
            Ok(outcome) => render_outcome(&outcome),
            Err(error) => render_agent_error(&error),
        }
    }

    // ---- 后台目标（B3：REPL/TUI 运行在后台线程 + 事件通道）----

    /// 前端统一入口：`/` 命令与向导输入同步执行，自然语言目标放后台线程。
    pub fn submit_line(&mut self, input: &str) -> Vec<SessionMessage>
    where
        C: Clone + Send + 'static,
    {
        let trimmed = input.trim();
        if self.wizard.is_some() || trimmed.starts_with('/') || trimmed.is_empty() {
            return self.execute(input);
        }
        self.start_goal(trimmed)
    }

    /// 启动后台目标；立即返回提示行，完成事件由 [`Session::poll`] 消费。
    pub fn start_goal(&mut self, goal: &str) -> Vec<SessionMessage>
    where
        C: Clone + Send + 'static,
    {
        if self.goal.is_some() {
            return vec![SessionMessage::Line(String::from(
                "已有目标在后台执行：/cancel 取消后再提交新目标",
            ))];
        }
        let Some(mut gateway) = self.gateway.clone() else {
            return vec![SessionMessage::Line(String::from(
                "未连接：先 /link tcp|stdio|mqtt 建立链路，再 /wait 等 Worker 上线",
            ))];
        };
        if gateway.list_workers().is_empty() {
            return vec![SessionMessage::Line(String::from(
                "当前没有已发现的 Worker：先 /wait 等待上线（或检查 Worker 是否已连接）",
            ))];
        }
        let mut runner = self.fork_for_goal();
        runner.cancel = CancelToken::new();
        self.cancel = runner.cancel.clone();
        let goal = String::from(goal);
        // 提示行在 spawn 前拼好：`goal` 会被 move 进工作线程。
        let started_line = format!("已开始执行：{goal}");
        let (sender, receiver) = mpsc::channel();
        let spawned = std::thread::Builder::new()
            .name(String::from("sekai-router-agent"))
            .spawn(move || {
                let messages = runner.run_goal_now(&goal);
                let _ = sender.send(messages);
            });
        match spawned {
            Ok(_handle) => {
                self.goal = Some(GoalRun {
                    receiver,
                    started: Instant::now(),
                });
                vec![
                    SessionMessage::Line(started_line),
                    SessionMessage::Line(String::from(
                        "后台执行中（界面保持可响应）；/cancel 可取消",
                    )),
                ]
            }
            Err(error) => vec![SessionMessage::Line(format!(
                "无法启动后台执行线程：{error}"
            ))],
        }
    }

    /// 当前是否有后台目标在跑。
    pub fn goal_running(&self) -> bool {
        self.goal.is_some()
    }

    /// 后台目标已运行毫秒（无目标时 0；TUI 状态栏用）。
    pub fn goal_elapsed_ms(&self) -> u64 {
        self.goal
            .as_ref()
            .map(|goal| goal.started.elapsed().as_millis() as u64)
            .unwrap_or(0)
    }

    /// 请求取消当前后台目标；返回是否有目标被取消。
    pub fn cancel_goal(&mut self) -> bool {
        if self.goal.is_some() {
            self.cancel.cancel();
            true
        } else {
            false
        }
    }

    /// 复制一份会话给后台线程（丢弃向导 / 上次目标等一次性状态）。
    fn fork_for_goal(&self) -> Session<C>
    where
        C: Clone,
    {
        Session {
            chat: self.chat.clone(),
            gateway: self.gateway.clone(),
            link_desc: self.link_desc.clone(),
            policy: self.policy.clone(),
            agent_config: self.agent_config.clone(),
            trace: self.trace.clone(),
            config: self.config.clone(),
            wizard: None,
            last_workers: self.last_workers.clone(),
            hub: self.hub.clone(),
            cancel: self.cancel.clone(),
            goal: None,
        }
    }

    /// 在（后台）会话里同步跑 Agent 回路；返回渲染好的输出行。
    fn run_goal_now(&mut self, goal: &str) -> Vec<SessionMessage> {
        let Some(mut gateway) = self.gateway.clone() else {
            return vec![SessionMessage::Line(String::from(
                "未连接：先 /link tcp|stdio|mqtt 建立链路，再 /wait 等 Worker 上线",
            ))];
        };
        if gateway.list_workers().is_empty() {
            return vec![SessionMessage::Line(String::from(
                "当前没有已发现的 Worker：先 /wait 等待上线（或检查 Worker 是否已连接）",
            ))];
        }
        let mut agent = Agent::new(
            self.chat.clone(),
            gateway,
            self.policy.clone(),
            self.agent_config.clone(),
            self.trace.clone(),
        )
        .with_cancel(self.cancel.clone());
        match agent.run(goal) {
            Ok(outcome) => render_outcome(&outcome),
            Err(error) => render_agent_error(&error),
        }
    }

    // ---- 辅助 ----

    /// 把网关与策略同步进 prompt 共享核心（建链 / 关链 / 注入 /
    /// `/policy` 修改后调用；未挂接 hub 时为 no-op）。
    fn sync_hub(&mut self) {
        if let Some(hub) = &self.hub {
            hub.set_gateway(self.gateway.clone());
            hub.set_policy(self.policy.clone());
        }
    }

    /// 链路构造参数（网关装配统一走 [`crate::link_builder`] 共享层）。
    fn link_options(&self) -> LinkOptions {
        LinkOptions {
            topic_root: self.config.topic_root.clone(),
            default_ddl_ms: self.config.default_ddl_ms,
            tool_timeout_ms: self.config.tool_timeout_ms,
            task_timeout_ms: self.config.task_timeout_ms,
            worker_id_hint: String::from("pc-worker"),
            mqtt_username: self.config.mqtt_username.clone(),
            mqtt_password: self.config.mqtt_password.clone(),
            mqtt_client_id: None,
        }
    }
}

// ---- /start 建连向导：挂起态状态机（纯逻辑，便于离线单测）----

/// 向导默认值（从 `SessionConfig` 快照；向导运行期间不受配置变化影响）。
struct WizardDefaults {
    /// tcp 监听默认地址。
    listen_addr: String,
    /// stdio 默认 Worker 可执行文件。
    worker_bin: String,
    /// mqtt 默认 broker 地址（无默认时 `None`）。
    mqtt_addr: Option<String>,
}

/// 向导当前步骤。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WizardStep {
    /// 选择链路类型（1 stdio / 2 tcp / 3 mqtt / 4 取消）。
    ChooseMode,
    /// tcp：输入监听地址。
    TcpAddr,
    /// mqtt：输入 broker 地址。
    MqttAddr,
    /// stdio：输入 Worker 可执行文件。
    StdioBin,
}

/// 向导请求建立的链路与参数。
#[derive(Debug, Clone, PartialEq, Eq)]
enum WizardLink {
    /// tcp 监听地址。
    Tcp(String),
    /// mqtt broker 地址。
    Mqtt(String),
    /// stdio Worker 可执行文件。
    Stdio(String),
}

/// 向导对一行输入的处理结果（Session 负责解释执行）。
#[derive(Debug)]
enum WizardOutcome {
    /// 输出提示后停在当前步骤（非法输入 / 缺默认值）。
    Stay(Vec<String>),
    /// 输出下一步提示（`feed` 已推进内部步骤）。
    Advance(Vec<String>),
    /// 用户选择取消，向导结束。
    Cancel(Vec<String>),
    /// 参数收齐，请求建链；建链失败时向导仍停在原参数步骤供重试。
    Link(WizardLink),
}

/// `/start` 建连向导：挂起态状态机。
///
/// 纯逻辑、不碰网关：[`LinkWizard::feed`] 吃一行非 '/' 输入，返回
/// [`WizardOutcome`]；真正建链由 Session 解释执行（见
/// `Session::wizard_link`）。
struct LinkWizard {
    step: WizardStep,
    defaults: WizardDefaults,
}

impl LinkWizard {
    /// 从默认值快照构造（初始步骤：选择链路类型）。
    fn new(defaults: WizardDefaults) -> Self {
        Self {
            step: WizardStep::ChooseMode,
            defaults,
        }
    }

    /// `/start` 入口菜单（含当前链路状态，帮助用户判断是否重连）。
    fn menu_lines(&self, current_link: &str) -> Vec<String> {
        vec![
            format!("建连向导 — 当前链路：{current_link}"),
            String::from("请选择连接方式："),
            String::from("  1 stdio  本机拉起 Worker 子进程"),
            format!(
                "  2 tcp    监听端口等待 Worker 直连（默认 {}）",
                self.defaults.listen_addr
            ),
            match &self.defaults.mqtt_addr {
                Some(addr) => format!("  3 mqtt   连接 MQTT broker（默认 {addr}）"),
                None => String::from("  3 mqtt   连接 MQTT broker（需输入地址）"),
            },
            String::from("  4 取消"),
        ]
    }

    /// 当前步骤的输入提示（每步写明「回车用默认 xxx」）。
    fn prompt_lines(&self) -> Vec<String> {
        match self.step {
            WizardStep::ChooseMode => {
                vec![String::from("请输入 1-4 或链路名（stdio/tcp/mqtt）：")]
            }
            WizardStep::TcpAddr => vec![format!(
                "输入监听地址 HOST:PORT（回车用默认 {}）：",
                self.defaults.listen_addr
            )],
            WizardStep::MqttAddr => match &self.defaults.mqtt_addr {
                Some(addr) => vec![format!(
                    "输入 MQTT broker 地址 HOST:PORT（回车用默认 {addr}）："
                )],
                None => vec![String::from(
                    "输入 MQTT broker 地址 HOST:PORT（未设置默认，必须输入）：",
                )],
            },
            WizardStep::StdioBin => vec![format!(
                "输入 Worker 可执行文件（回车用默认 {}）：",
                self.defaults.worker_bin
            )],
        }
    }

    /// 喂入一行已去空白的非 '/' 输入。
    fn feed(&mut self, input: &str) -> WizardOutcome {
        match self.step {
            WizardStep::ChooseMode => self.feed_choose(input),
            WizardStep::TcpAddr => feed_addr(
                input,
                &self.defaults.listen_addr,
                WizardLink::Tcp,
                "监听地址",
            ),
            WizardStep::MqttAddr => feed_mqtt(input, &self.defaults.mqtt_addr),
            WizardStep::StdioBin => feed_stdio_bin(input, &self.defaults.worker_bin),
        }
    }

    /// 菜单步骤：消费 1-4 / stdio / tcp / mqtt / 取消。
    fn feed_choose(&mut self, input: &str) -> WizardOutcome {
        let step = match input {
            "1" | "stdio" => WizardStep::StdioBin,
            "2" | "tcp" => WizardStep::TcpAddr,
            "3" | "mqtt" => WizardStep::MqttAddr,
            "4" | "取消" => {
                return WizardOutcome::Cancel(vec![String::from("已取消建连向导")]);
            }
            _ => {
                return WizardOutcome::Stay(vec![String::from(
                    "请先选择 1-4 或输入 /start 取消向导",
                )]);
            }
        };
        self.step = step;
        WizardOutcome::Advance(self.prompt_lines())
    }
}

/// tcp 地址步骤：空输入回退默认；`HOST:PORT` 粗校验失败停留原步骤。
fn feed_addr(
    input: &str,
    default: &str,
    make: fn(String) -> WizardLink,
    label: &str,
) -> WizardOutcome {
    let text = if input.is_empty() { default } else { input }.trim();
    if text.is_empty() {
        return WizardOutcome::Stay(vec![format!(
            "{label}不能为空：请输入 HOST:PORT（如 127.0.0.1:7421）"
        )]);
    }
    if !is_listen_addr(text) {
        return WizardOutcome::Stay(vec![format!(
            "{label}非法「{text}」：应为 HOST:PORT 形式（如 127.0.0.1:7421）"
        )]);
    }
    WizardOutcome::Link(make(text.to_string()))
}

/// mqtt 地址步骤：空输入回退默认（无默认则停留）；按 `Endpoint` 规则校验。
fn feed_mqtt(input: &str, default: &Option<String>) -> WizardOutcome {
    let text = if input.is_empty() {
        match default {
            Some(addr) => addr.as_str(),
            None => {
                return WizardOutcome::Stay(vec![String::from(
                    "没有默认 broker 地址：请输入 HOST:PORT（如 127.0.0.1:1883），或设置 SEKAI_MQTT_ADDR",
                )]);
            }
        }
    } else {
        input
    }
    .trim();
    match Endpoint::parse(text) {
        Ok(_) => WizardOutcome::Link(WizardLink::Mqtt(text.to_string())),
        Err(error) => WizardOutcome::Stay(vec![format!(
            "broker 地址非法「{text}」：{error}（应为 HOST:PORT，如 127.0.0.1:1883）"
        )]),
    }
}

/// stdio 步骤：空输入回退默认；含空白视为想传参，引导改用 `/link stdio`。
fn feed_stdio_bin(input: &str, default: &str) -> WizardOutcome {
    let text = if input.is_empty() { default } else { input }.trim();
    if text.is_empty() {
        return WizardOutcome::Stay(vec![String::from(
            "Worker 可执行文件不能为空：请输入路径（如 sekai-worker）",
        )]);
    }
    if text.split_whitespace().count() > 1 {
        return WizardOutcome::Stay(vec![String::from(
            "向导不支持传参：请只输入可执行文件路径，需要参数请用 /link stdio BIN -- ARGS",
        )]);
    }
    WizardOutcome::Link(WizardLink::Stdio(text.to_string()))
}

/// 粗校验 tcp 监听地址：`SocketAddr`（含 `[IPv6]:port`）或 `主机名:端口`；
/// 端口 0（自动分配）放行，缺端口 / 端口超范围拒绝。
fn is_listen_addr(text: &str) -> bool {
    if text.parse::<std::net::SocketAddr>().is_ok() {
        return true;
    }
    match text.rsplit_once(':') {
        Some((host, port)) => {
            !host.is_empty() && !host.contains(':') && port.parse::<u16>().is_ok()
        }
        None => false,
    }
}

/// Agent 结果 → 会话输出行。
fn render_outcome(outcome: &AgentOutcome) -> Vec<SessionMessage> {
    let mut lines = Vec::new();
    if let Some(text) = &outcome.final_text {
        for line in text.lines() {
            lines.push(SessionMessage::Line(line.to_string()));
        }
    }
    if outcome.status == AgentStatus::MaxSteps {
        lines.push(SessionMessage::Line(format!(
            "（达到最大步数 {}，未获得最终回答）",
            outcome.steps
        )));
    }
    lines.push(SessionMessage::Line(format!(
        "〔步数={} 工具调用={} tokens={}〕",
        outcome.steps,
        outcome.tool_calls.len(),
        outcome.usage.total_tokens
    )));
    lines
}

/// Agent 错误 → 会话输出行（会话继续）。
fn render_agent_error(error: &AgentError) -> Vec<SessionMessage> {
    let text = error.to_string();
    if text.contains("cancelled") {
        return vec![SessionMessage::Line(String::from(
            "目标已取消：已向 Worker 下发 sekai.cancel 并停止后续派工",
        ))];
    }
    vec![SessionMessage::Line(format!("执行失败：{text}"))]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::CommandRequest;
    use crate::gateway::{CommandReceipt, GatewayError};
    use crate::llm::{ChatRequest, ChatResponse, LlmError};
    use crate::worker::WorkerInfo;
    use std::sync::{Arc, Mutex};

    /// 空 ChatClient：向导 / 链路测试不会真的调 LLM。
    #[derive(Clone)]
    struct NullChat;

    impl ChatClient for NullChat {
        fn chat(&mut self, _request: &ChatRequest) -> Result<ChatResponse, LlmError> {
            Err(LlmError::Transport(String::from("测试未配置 LLM")))
        }
    }

    /// 可注入 Worker 快照的内存网关。
    struct MockGateway {
        workers: Arc<Mutex<Vec<WorkerInfo>>>,
    }

    impl WorkerGateway for MockGateway {
        fn list_workers(&mut self) -> Vec<WorkerInfo> {
            self.workers
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone()
        }

        fn submit(&mut self, _request: CommandRequest) -> Result<CommandReceipt, GatewayError> {
            let (sender, receiver) = std::sync::mpsc::channel();
            let _ = sender.send(Err(GatewayError::new(
                "worker_offline",
                "测试网关不支持 call",
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

    fn worker(worker_id: &str, online: bool) -> WorkerInfo {
        WorkerInfo {
            worker_id: String::from(worker_id),
            online,
            boot_id: 7,
            firmware: String::from("1.2.3"),
            uptime_ms: 10,
            unix_ms: 20,
            tools: vec![String::from("gpio.info")],
            tasks: vec![String::from("echo")],
            workers: Vec::new(),
            active_tasks: Vec::new(),
            last_seen_mono_ms: 0,
            hello_seq: 1,
        }
    }

    /// 测试配置：端口 0 自动分配（避免并行测试抢固定端口）、等待窗口极短。
    fn test_config() -> SessionConfig {
        let mut config = SessionConfig::minimal();
        config.listen_addr = String::from("127.0.0.1:0");
        config.wait_worker_ms = 1;
        config
    }

    fn session() -> Session<NullChat> {
        Session::new(
            NullChat,
            Policy::default(),
            AgentConfig {
                model: String::from("test"),
                max_steps: 1,
                ddl_ms: 1_000,
                timeout_ms: None,
                temperature: 0.0,
                max_tokens: 16,
            },
            TraceWriter::disabled(),
            test_config(),
        )
    }

    fn lines(messages: &[SessionMessage]) -> Vec<&str> {
        messages
            .iter()
            .map(|message| match message {
                SessionMessage::Line(text) => text.as_str(),
                SessionMessage::Exit => "",
            })
            .collect()
    }

    fn wizard_defaults() -> WizardDefaults {
        WizardDefaults {
            listen_addr: String::from("127.0.0.1:7421"),
            worker_bin: String::from("sekai-worker"),
            mqtt_addr: Some(String::from("127.0.0.1:1883")),
        }
    }

    // ---- LinkWizard 纯逻辑 ----

    #[test]
    fn wizard_menu_renders_options_and_defaults() {
        let wizard = LinkWizard::new(wizard_defaults());
        let menu = wizard.menu_lines("未连接").join("\n");
        assert!(menu.contains("当前链路：未连接"), "{menu}");
        for needle in ["1 stdio", "2 tcp", "3 mqtt", "4 取消"] {
            assert!(menu.contains(needle), "菜单缺少 {needle}：{menu}");
        }
        assert!(menu.contains("127.0.0.1:7421"), "{menu}");
        assert!(menu.contains("127.0.0.1:1883"), "{menu}");
    }

    #[test]
    fn wizard_consumes_choice_by_number_or_name() {
        for (input, step) in [
            ("1", WizardStep::StdioBin),
            ("stdio", WizardStep::StdioBin),
            ("2", WizardStep::TcpAddr),
            ("tcp", WizardStep::TcpAddr),
            ("3", WizardStep::MqttAddr),
            ("mqtt", WizardStep::MqttAddr),
        ] {
            let mut wizard = LinkWizard::new(wizard_defaults());
            match wizard.feed(input) {
                WizardOutcome::Advance(lines) => {
                    assert!(!lines.is_empty(), "{input} 应输出下一步提示");
                }
                other => panic!("输入 {input} 应推进步骤，得到 {other:?}"),
            }
            assert_eq!(wizard.step, step, "输入 {input}");
        }
    }

    #[test]
    fn wizard_menu_rejects_unknown_choice_and_stays() {
        let mut wizard = LinkWizard::new(wizard_defaults());
        match wizard.feed("vpn") {
            WizardOutcome::Stay(lines) => {
                assert!(
                    lines
                        .join("\n")
                        .contains("请先选择 1-4 或输入 /start 取消向导"),
                    "{lines:?}"
                );
            }
            other => panic!("未知选项应停留：{other:?}"),
        }
        assert_eq!(wizard.step, WizardStep::ChooseMode);
    }

    #[test]
    fn wizard_choice_four_cancels() {
        let mut wizard = LinkWizard::new(wizard_defaults());
        match wizard.feed("4") {
            WizardOutcome::Cancel(lines) => {
                assert!(lines.join("\n").contains("已取消"), "{lines:?}");
            }
            other => panic!("4 应取消：{other:?}"),
        }
        let mut wizard = LinkWizard::new(wizard_defaults());
        assert!(matches!(wizard.feed("取消"), WizardOutcome::Cancel(_)));
    }

    #[test]
    fn wizard_tcp_addr_falls_back_to_default_and_validates() {
        let mut wizard = LinkWizard::new(wizard_defaults());
        wizard.feed("tcp");
        match wizard.feed("") {
            WizardOutcome::Link(WizardLink::Tcp(addr)) => {
                assert_eq!(addr, "127.0.0.1:7421", "空输入应回退默认监听地址");
            }
            other => panic!("空输入应回退默认：{other:?}"),
        }
        match wizard.feed("localhost:7422") {
            WizardOutcome::Link(WizardLink::Tcp(addr)) => assert_eq!(addr, "localhost:7422"),
            other => panic!("主机名+端口应被接受：{other:?}"),
        }
        for bad in ["abc", "127.0.0.1", "host:99999"] {
            match wizard.feed(bad) {
                WizardOutcome::Stay(lines) => {
                    assert!(
                        lines.join("\n").contains("非法"),
                        "{bad} 应报中文错误：{lines:?}"
                    );
                }
                other => panic!("{bad} 应报错停留：{other:?}"),
            }
            assert_eq!(wizard.step, WizardStep::TcpAddr, "{bad} 后应停留原步骤");
        }
    }

    #[test]
    fn wizard_mqtt_addr_default_missing_and_invalid() {
        let mut wizard = LinkWizard::new(wizard_defaults());
        wizard.feed("mqtt");
        match wizard.feed("") {
            WizardOutcome::Link(WizardLink::Mqtt(addr)) => {
                assert_eq!(addr, "127.0.0.1:1883");
            }
            other => panic!("空输入应回退默认 broker：{other:?}"),
        }
        // 无默认值：空输入停留并提示设置 SEKAI_MQTT_ADDR。
        let mut no_default = wizard_defaults();
        no_default.mqtt_addr = None;
        let mut wizard = LinkWizard::new(no_default);
        wizard.feed("3");
        match wizard.feed("") {
            WizardOutcome::Stay(lines) => {
                assert!(lines.join("\n").contains("SEKAI_MQTT_ADDR"), "{lines:?}");
            }
            other => panic!("无默认时空输入应停留：{other:?}"),
        }
        // mqtt 端口 0 非法（Endpoint 规则），停留原步骤。
        match wizard.feed("127.0.0.1:0") {
            WizardOutcome::Stay(lines) => {
                assert!(lines.join("\n").contains("非法"), "{lines:?}");
            }
            other => panic!("端口 0 应报错：{other:?}"),
        }
        assert_eq!(wizard.step, WizardStep::MqttAddr);
    }

    #[test]
    fn wizard_stdio_bin_default_and_rejects_args() {
        let mut wizard = LinkWizard::new(wizard_defaults());
        wizard.feed("1");
        match wizard.feed("") {
            WizardOutcome::Link(WizardLink::Stdio(bin)) => {
                assert_eq!(bin, "sekai-worker", "空输入应回退默认可执行文件");
            }
            other => panic!("{other:?}"),
        }
        match wizard.feed(r"C:\tools\worker.exe") {
            WizardOutcome::Link(WizardLink::Stdio(bin)) => {
                assert_eq!(bin, r"C:\tools\worker.exe");
            }
            other => panic!("{other:?}"),
        }
        match wizard.feed("worker.exe --verbose") {
            WizardOutcome::Stay(lines) => {
                assert!(lines.join("\n").contains("不支持传参"), "{lines:?}");
            }
            other => panic!("带参输入应停留：{other:?}"),
        }
        assert_eq!(wizard.step, WizardStep::StdioBin);
    }

    // ---- 真实地址提示 ----

    #[test]
    fn link_tcp_prints_real_copyable_worker_command() {
        let mut session = session();
        let text = lines(&session.execute("/link tcp 127.0.0.1:0")).join("\n");
        assert!(text.contains("已监听 127.0.0.1:"), "实际地址应出现：{text}");
        assert!(
            text.contains("在另一终端运行：sekai-worker connect 127.0.0.1:"),
            "应给出可直接复制的 worker 命令：{text}"
        );
        assert!(!text.contains("<地址>"), "不应再有占位符：{text}");
    }

    #[test]
    fn link_tcp_defaults_to_configured_listen_addr() {
        let mut session = session(); // listen_addr = 127.0.0.1:0
        let text = lines(&session.execute("/link tcp")).join("\n");
        // 默认地址（端口 0 自动分配）应补全为真实端口。
        let addr = text
            .split("已监听 ")
            .nth(1)
            .and_then(|rest| rest.split('，').next())
            .unwrap_or_default();
        assert!(addr.starts_with("127.0.0.1:"), "应回退默认地址：{text}");
        let port: u16 = addr
            .rsplit(':')
            .next()
            .unwrap_or_default()
            .parse()
            .unwrap_or(0);
        assert!(port > 0, "端口 0 应替换为实际分配端口：{text}");
    }

    #[test]
    fn link_mqtt_hints_worker_side_broker() {
        let mut session = session();
        session.config.mqtt = Some(Endpoint::parse("127.0.0.1:1").expect("端点合法"));
        let text = lines(&session.execute("/link mqtt")).join("\n");
        assert!(text.contains("正在连接 broker 127.0.0.1:1"), "{text}");
        assert!(
            text.contains("Worker 侧请连接同一 broker：127.0.0.1:1"),
            "应提示 Worker 侧连同一 broker：{text}"
        );
    }

    // ---- /start 向导（Session 集成）----

    #[test]
    fn start_wizard_flow_tcp_end_to_end() {
        let mut session = session();
        let menu = lines(&session.execute("/start")).join("\n");
        assert!(menu.contains("建连向导"), "{menu}");
        assert!(menu.contains("1 stdio"), "{menu}");
        assert!(menu.contains("当前链路：未连接"), "{menu}");

        // 挂起态：不认识的输入不交给 Agent，而是提示先选择。
        let hint = lines(&session.execute("帮我开灯")).join("\n");
        assert!(
            hint.contains("请先选择 1-4 或输入 /start 取消向导"),
            "{hint}"
        );

        // 选 tcp → 提示输入地址（写明回车用默认）。
        let prompt = lines(&session.execute("2")).join("\n");
        assert!(prompt.contains("输入监听地址 HOST:PORT"), "{prompt}");
        assert!(prompt.contains("回车用默认 127.0.0.1:0"), "{prompt}");

        // 回车用默认 → 建链 + 自动 /wait + 展示 /workers。
        let done = lines(&session.execute("")).join("\n");
        assert!(done.contains("已监听 127.0.0.1:"), "{done}");
        assert!(done.contains("sekai-worker connect 127.0.0.1:"), "{done}");
        assert!(done.contains("正在等待 Worker 上线"), "{done}");
        assert!(
            done.contains("等待超时") || done.contains("已上线"),
            "{done}"
        );
        assert!(done.contains("（当前没有已发现的Worker）"), "{done}");

        // 向导已结束：后续普通输入恢复 Agent 路径（无 Worker 提示）。
        let after = lines(&session.execute("看看状态")).join("\n");
        assert!(after.contains("当前没有已发现的 Worker"), "{after}");
    }

    #[test]
    fn wizard_slash_command_cancels_and_executes() {
        let mut session = session();
        session.execute("/start");
        let text = lines(&session.execute("/help")).join("\n");
        assert!(text.contains("已取消建连向导"), "{text}");
        assert!(text.contains("命令："), "取消后应正常执行 /help：{text}");

        // 挂起态已清：普通输入走 Agent 路径（未连接提示）。
        let after = lines(&session.execute("hello")).join("\n");
        assert!(after.contains("未连接"), "{after}");
    }

    #[test]
    fn wizard_invalid_addr_stays_then_recovers() {
        let mut session = session();
        session.execute("/start");
        session.execute("tcp");
        let bad = lines(&session.execute("not-an-addr")).join("\n");
        assert!(bad.contains("非法"), "{bad}");
        assert!(
            bad.contains("回车用默认"),
            "报错后应重新提示当前步骤：{bad}"
        );
        // 停留在地址步骤：再输入合法地址可继续完成建链。
        let done = lines(&session.execute("127.0.0.1:0")).join("\n");
        assert!(done.contains("已监听 127.0.0.1:"), "{done}");
    }

    #[test]
    fn wizard_stdio_spawn_failure_stays_in_wizard() {
        let mut session = session();
        session.execute("/start");
        session.execute("stdio");
        let bad = lines(&session.execute("sekai-no-such-binary-xyz")).join("\n");
        assert!(bad.contains("拉起失败"), "{bad}");
        assert!(bad.contains("回车用默认"), "失败后应停留在向导步骤：{bad}");
        // 仍挂起：后续输入继续被向导消费（而非交给 Agent）。
        // 带空格的输入在向导里报「不支持传参」，而不是进入 Agent 回路。
        let hint = lines(&session.execute("随便 说点什么")).join("\n");
        assert!(hint.contains("不支持传参"), "{hint}");
    }

    #[test]
    fn wizard_mqtt_without_default_requires_address() {
        let mut session = session(); // minimal：mqtt = None
        session.execute("/start");
        session.execute("3");
        let stay = lines(&session.execute("")).join("\n");
        assert!(stay.contains("没有默认 broker 地址"), "{stay}");
        assert!(stay.contains("必须输入"), "{stay}");
        // '/' 命令取消向导并照常执行。
        let status = lines(&session.execute("/status")).join("\n");
        assert!(status.contains("已取消建连向导"), "{status}");
        assert!(status.contains("链路：未连接"), "{status}");
    }

    // ---- poll / 访问器 ----

    #[test]
    fn poll_reports_only_newly_seen_workers() {
        let mut session = session();
        assert!(session.poll().is_empty(), "无网关应返回空");

        let workers = Arc::new(Mutex::new(Vec::<WorkerInfo>::new()));
        let gateway: GatewayHandle = Arc::new(Mutex::new(Box::new(MockGateway {
            workers: Arc::clone(&workers),
        })));
        session.set_gateway(gateway, "mock（测试）");
        assert!(session.poll().is_empty(), "注入时无 Worker，基线为空");

        workers.lock().unwrap().push(worker("w1", true));
        assert_eq!(lines(&session.poll()), vec!["Worker w1 已上线"]);
        assert!(session.poll().is_empty(), "无变化应返回空");

        workers.lock().unwrap().push(worker("w2", true));
        workers.lock().unwrap().push(worker("w3", false));
        let text = lines(&session.poll()).join("\n");
        assert!(text.contains("Worker w2 已上线"), "{text}");
        assert!(text.contains("Worker w3 已上线"), "{text}");
    }

    #[test]
    fn poll_after_link_uses_empty_baseline() {
        let mut session = session();
        session.execute("/link tcp 127.0.0.1:0");
        assert!(session.poll().is_empty(), "建链时的空 Worker 集应记为基线");
    }

    #[test]
    fn poll_does_not_duplicate_wait_announcement() {
        let mut session = session();
        let workers = Arc::new(Mutex::new(vec![worker("w1", true)]));
        let gateway: GatewayHandle = Arc::new(Mutex::new(Box::new(MockGateway {
            workers: Arc::clone(&workers),
        })));
        session.set_gateway(gateway, "mock（测试）");
        let waited = lines(&session.execute("/wait")).join("\n");
        assert!(waited.contains("Worker 已上线：w1"), "{waited}");
        assert!(session.poll().is_empty(), "/wait 已宣告，poll 不应重复报");
    }

    #[test]
    fn accessors_report_gateway_state() {
        let mut session = session();
        assert!(!session.gateway_ready());
        assert_eq!(session.worker_count(), 0);

        let workers = Arc::new(Mutex::new(vec![worker("w1", true)]));
        let gateway: GatewayHandle = Arc::new(Mutex::new(Box::new(MockGateway {
            workers: Arc::clone(&workers),
        })));
        session.set_gateway(gateway, "mock（测试）");
        assert!(session.gateway_ready(), "MockGateway 恒就绪");
        assert_eq!(session.worker_count(), 1);
    }
}
