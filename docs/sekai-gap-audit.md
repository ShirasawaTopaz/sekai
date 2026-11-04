# Sekai 缺口审计（2026-11-04）

> **状态**：审计记录，**不是修复记录**。本文只陈述现状、证据与建议；修复工作包见 §9，需另行批准。
> **审计基线**：`main` @ `32027b4`（2026-11-04）。
> **方法**：静态阅读 + `grep` + 文档比对。本机 DSH 沙箱内 `cargo` 无法启动 `rustc`
>（`os error 5`），因此本审计**不包含运行时复现**；文中"证据"均为源码/文档事实，
> 运行行为以 CI 或正常 cargo 环境为准。
> **证据强度**：默认 ✅ 已按源码逐条核对；标 ◐ 的条目由控制流静态推断（未运行复现）；
> 标 ○ 的仅为文档声明。

---

## 0. 摘要

### 0.1 结论

- **单 Worker 的 PC 闭环已经跑通**：目标 → Router（LLM tool-calling）→ 直连
  Worker → `pc.*` → 结果回填 → 自然语言回答，有测试与手工验收记录。
- **"一个或多个 Worker"只完成了"一个"**：主线的直连网关同一时刻只持有 1 条连接、
  1 个 Worker 端点；多 Worker 能力目前只存在于置后的 MQTT/嵌入式路径。
- **长任务链路没有闭环**：没有取消入口；serve 的单 dispatcher 会被长命令阻塞；
  V1 没有进度事件；`pc.exec` 同步阻塞 Worker 帧循环；默认 3s 预算与
  `pc.exec` 的 10s 预算互相矛盾。
- **协议里"写了"但 Router 侧"没做"的能力**：`sekai.set_time` 时钟校准、
  `sekai.workers.set`、`sekai.cancel` 虽在内置工具表里，但参数是二进制，
  Router 的文本参数通道无法表达；Jump 只有 Worker 侧实现。
- **工程闭环不足**：CI 只跑 ubuntu + 默认 feature，真实子进程 E2E 与真实 API
  测试全部 `#[ignore]`，没有 esp32 构建、没有 release/tag、MSRV 声明错误。

### 0.2 已闭环（有测试或验收证据）

| 能力 | 证据 |
| --- | --- |
| 单 Worker 的 LLM 派工闭环 | `crate/sekai-router/src/agent.rs`；`crate/sekai-router/tests/agent_loop.rs`；`docs/sekai-dsh-plugin.md` §5（14/14 手工验收） |
| V1 帧协议 + `no_std` Runtime + MQTT5 客户端 | `crate/sekai`；`docs/sekai-ai.md` §9 记录 2026-11-03 外部 cargo 门禁：564 passed / 0 failed |
| 直连链路（stdio/TCP）+ REPL/TUI/`/start` 向导 + 批处理 `--json` | `crate/sekai-router/src/{direct_gateway,direct,session,repl,tui}.rs`、`config.rs` |
| serve JSONL v1 + prompt v1.1（ingress / serve `prompt`） | `serve.rs`、`prompt_server.rs`、`tests/serve.rs`、`tests/prompt_ingress.rs` |
| 安全策略（默认只读、发布前强制、防递归） | `tools.rs::check_policy`；`tests/agent_loop.rs` 策略用例 |
| GPIO 抽象 + 虚拟后端 + `gpio.*` 工具 | `crate/sekai-gpio`（spec_conformance/runtime_tools 等测试） |
| ESP32/STM32 后端**代码** | `crate/sekai-gpio/src/{esp32,stm32}`；ESP32 固件 2026-11-03 构建成功（`crate/sekai-embedded/README.md`） |

### 0.3 未闭环总表

| ID | 主题 | 严重度 | 何时会踩到 |
| --- | --- | --- | --- |
| B1 | 多 Worker | 高 | 想同时操作两台机器 / 两个 Worker |
| B2 | 取消与中断 | 高 | 长任务失控、TUI/REPL 卡住、插件 abort |
| B3 | 长任务与进度 | 高 | `pc.job` / `pc.ask` / 长 `pc.exec` |
| B4 | 执行预算与进程管理 | 高 | 任何 >3s 的 `pc.exec`；超时后的子进程 |
| B5 | 内层 prompt 与在途命令互斥 | 中高 | 单 Worker 下 `pc.ask` 想让 LLM 用工具 |
| B6 | 二进制内置工具与时钟 | 中高 | 对时、取消、DeviceList 更新、Expired |
| B7 | Jump（Router 侧） | 中 | MQTT/嵌入式链路迁移 |
| B8 | 可观测性与晚到事件 | 中 | 超时/断链后诊断、外网监控 |
| B9 | prompt ingress 安全 | 中高 | 非本机/多用户机器、误绑 `0.0.0.0` |
| B10 | HTTPS 依赖外部 helper | 中 | 裸 Rust 部署调 DeepSeek API |
| B11 | DSH 插件 / serve 细节 | 中低 | DSH 主会话日常使用 |
| K1–K8 | 核心协议/运行时细节 | 中/低 | MQTT 链路、Jump 回退、重连 |
| P1–P14 | PC Worker / GPIO 细节 | 高/中/低 | `pc.exec`、`pc.ls`、prompt 通道、直连链路 |
| C1–C4 | 工程闭环（CI/发布/MSRV/阶段） | 中高 | 每次提交、发布、新人首次构建 |
| D1–D2 | ESP32/STM32 实机验证 | 置后 | P3 阶段 |

---

## 0.4 本轮闭环结果（2026-11-04，分支 `feat/gap-closure`）

> 门禁：`scripts/gate.sh` 全绿（fmt / check --all-targets / clippy / test --workspace / 插件 smoke 24 项）。
> 下表每项都有随代码提交的测试；`P13` 与 D1/D2 仍为置后项（无实机）。

| ID | 状态 | 实现要点 | 证据（测试名/命令） |
| --- | --- | --- | --- |
| B1 | 已修复 | 直连常驻 accept 多端点、按 Hello `worker_id` 路由、同 id 顶替、聚合 `list_workers` | `tests/direct_gateway.rs::tcp_listen_serves_two_workers_and_routes_by_id` 等 4 例 |
| B2 | 已修复 | `CommandReceipt.cmd_id`、`submit_control`/`cancel`、serve `cancel`/`commands`/`async:true`、REPL `/cancel`、TUI Ctrl-C、agent `CancelToken` | `tests/serve.rs::async_tool_call_replies_accepted_then_notifies`、`cancel_routes_to_gateway_and_marks_known`；`tests/agent_loop.rs::cancel_during_tool_wait_dispatches_sekai_cancel` |
| B3 | 已修复 | serve dispatcher 只分发、长方法工作线程 + 按 id 写队列；REPL/TUI 后台执行 + 事件通道；ingress 并发 4 | `tests/session.rs::background_goal_runs_through_event_channel`、`tests/serve.rs::status_exposes_diagnostics_fields` |
| B4.1 | 已修复 | `DEFAULT_DDL_MS` 10000、`TOOL_TIMEOUT_EXTRA_MS` 5000、提示要求长任务显式 ddl | `tests/agent_loop.rs` 断言更新；`config.rs` 单测 |
| B4.2–4.7 | 已修复 | `pc.exec` 任务化、杀进程树、stdin null、输出泵、ls/read 预算、UTF-8 lossy | `capability.rs` 单测（exec Task/DDL/进程树/stdin/pump/ls/read/lossy）、`host_flow.rs` 3 段帧 |
| B5 | 已修复 | `max_inflight_per_worker=4`、prompt 内层可复用工具、文案说明 | `tests/serve.rs` D5 用例、`tests/control_plane.rs::submit_control_bypasses_inflight_limit` |
| B6 | 已修复 | Hello 时钟偏差 >2s 或 0 自动 `sekai.set_time`（按 boot 去重）；serve `admin`（set_time/cancel/workers.set，allow_admin）；`clock_synced` | `tests/control_plane.rs::hello_clock_skew_triggers_automatic_set_time_once`、`tests/serve.rs::admin_methods_encode_payloads` |
| B7 | 已修复 | `CommandKind::Jump` + `CommandRequest::jump` + serve `jump`；Worker `FallingBack` 5s 超时/幂等失败帧；跳转期 `Failed(Busy)` | `tests/control_plane.rs::jump_request_encodes_jump_body`、`tests/serve.rs::jump_requires_admin_and_dispatches_jump_kind`、`sekai/tests/sekai_runtime.rs::jump_fallback_*` |
| B8 | 已修复 | status `link_info/inflight/counters/clock_synced/late_results`；失败结果补 cmd_id/elapsed/phases；serve 下 trace 生效；晚到终态入环形 | `tests/control_plane.rs::late_final_event_after_timeout_is_recorded_as_late_result`、`unknown_cmd_id_final_frames_are_recorded_as_late` |
| B9 | 已修复 | 默认回环、远程需 `SEKAI_INGRESS_ALLOW_REMOTE` + token（hello 握手）、连接/速率上限、审计日志 | `tests/prompt_ingress.rs::ingress_requires_token_handshake`、`ingress_rejects_remote_bind_without_explicit_allow`、`ingress_rejects_connections_over_cap`、`ingress_stats_track_rejections` |
| B10 | 已修复 | 默认 `native-tls` 进程内 HTTPS（`tls` feature），helper 可用 `SEKAI_HTTPS_MODE=helper` 回退 | `tests/llm_http.rs::https_backend_defaults_to_native_tls`、`https_uses_native_tls_and_reports_connect_error` |
| B11 | 已修复 | 插件读 `addr`；serve 断链补发 `link_state` + 插件幂等兜底；任务描述提示 `ddl_ms>=60000` | `tests/serve.rs` link_state 用例、插件 smoke `router 插件：link 读 addr；…link_state…` |
| K1 | 已修复 | 协议文档统一为 `sekai.workers` / `sekai.workers.set`、Hello 字段 `workers` | `docs/sekai-protocol.md` §5.6/§7.2.1/§10 |
| K2 | 已修复 | 配置 `authentication_method` 时应答 AUTH（ContinueAuthentication） | `sekai/tests/sekai_runtime.rs::auth_challenge_is_answered_with_configured_data` |
| K3 | 已修复 | `FallingBack` deadline（5s）+ 幂等 `JumpUnreachable`；跳转期 `Failed(Busy)` | `commands_during_jump_fail_busy_instead_of_silent_drop`、`jump_fallback_timeout_emits_failed_and_recovers` |
| K4 | 已修复 | 默认 `clean_start=true` + `session_expiry=0`；拒绝可重放组合 | `clean_start_and_session_expiry_defaults_are_safe`、`every_connect_uses_clean_start` |
| K5 | 已修复 | SUBACK 错误码入 status + 指数退避重订阅 | `suback_error_is_visible_and_retried_with_backoff` |
| K6 | 已修复 | 每次 link_up 前重建 LWT（时间戳取本次 CONNECT） | `will_payload_timestamp_is_rebuilt_per_connect` |
| K7 | 已修复 | Outbox 最终/非最终独立预算、不淘汰 in-flight、`dropped_events` 计数 | `outbox_keeps_final_events_under_non_final_pressure` 等 3 个单测 + `outbox_final_budget_pressure_counts_dropped_events` |
| K8 | 已修复 | `Stdout::lwt_offline`、set_time 1s 限频、时间基准/`unsubscribe` 文档澄清 | `lwt_offline_roundtrip_and_decode`、`set_time_is_rate_limited` |
| P1–P7 | 已修复 | 见 B4.2–4.7 | 同上 |
| P8 | 已修复 | Router 通道按 id 过滤 + 有界缓冲 + 连接超时；`ask_cancellable` 到期退出 | `llm_channel` 单测、`capability.rs::pc_ask_*` |
| P9 | 已修复 | PUBACK 写成功后才补；写失败保留重放 | `tests/direct_link.rs::write_failure_keeps_outgoing_event_for_replay` |
| P10 | 已修复 | PINGREQ/PINGRESP 真实跨链路 + 双侧 1.5×keepalive 读空闲判定 | `silent_peer_triggers_read_idle_link_lost`、`healthy_idle_link_survives_keepalive_rounds`、`worker_pingreq_crosses_link_and_router_answers` |
| P11 | 已修复 | `on_link_up` 后清 `disconnect_requested` | `host.rs` 单测（P11 路径）；已有断链/重连用例覆盖 |
| P12 | 已修复 | `--gpio-inputs` + `VirtualGpio::with_inputs`；`gpio.read` 返回注入值 | `gpio` 单测 3 例 + `tests/tools.rs::injected_inputs_are_readable_through_tools`、bin 单测 2 例 |
| P13 | 置后 | 无 ESP32/STM32 实机 E2E（D1/D2） | — |
| P14 | 已修复 | `sekai-worker/README.md` 的 `host_registries` 描述已与代码一致 | 文档审阅 |

**仍置后/未做**：P13（实机）、D1/D2、B11 的 Web UI 与 npm 发布、C2（tag/release/CHANGELOG 实际发布流程）、
C4（阶段目标文档改写）。C1（CI 扩展：fmt 门禁、Windows job、real_api 可选 job）与 C3（MSRV/README）已完成。

---

## 1. 闭环判定标准

| 旅程 | "闭环"的最低要求 | 当前 |
| --- | --- | --- |
| J1 单 Worker PC 目标 | 自然语言 → 派工 → 结果可验证 → 错误可解释 | ✅ 基本闭环 |
| J2 多 Worker 协同 | 同时连接 ≥2 个 Worker，可按 id 定向/汇总 | ✅ 已闭环（B1，见 §0.4） |
| J3 长任务 | 可观测进度、可取消、超时语义一致、进程不残留 | ✅ 已闭环（B2/B3/B4，见 §0.4） |
| J4 DSH 集成 | 工具可调用、状态真实、长任务不阻塞、可中止 | ✅ 可用（B11 已修；Web UI/npm 发布仍置后） |
| J5 嵌入式 MQTT | 固件 + 真 broker + 真 GPIO 端到端、时钟校准、Jump | ⚠️ 仅构建过固件（D1） |
| J6 STM32 板级 | 板级工程 + 电气验证 | ❌ 仅交叉编译检查（D2） |

---

## 2. 主线产品缺口（B）

### B1 多 Worker 不可用（高）✅

- **现象**：`DirectGateway` 只持有一个 `IoState::Connected { endpoint, .. }`；
  `Accepting` 状态每次只 `accept` 一条连接，进入 `Connected` 后监听器被保留但
  不再 accept；PC Worker 只支持 `stdio`/`tcp`（无 MQTT）。
- **证据**：`crate/sekai-router/src/direct_gateway.rs`（`IoState`、`DirectIoWorker::run`）；
  `crate/sekai-router/src/link_builder.rs:50`（`max_inflight_per_worker = 1`）；
  `crate/sekai-worker/src/bin/sekai-worker.rs`（`--mode stdio|tcp`，无 mqtt）。
- **影响**：`README.md` 的"派给一个或多个 Worker"与 `worker_filter`/`list_workers`
  的产品承诺在主路径上不成立；第二个 Worker 只能排队等第一个断开。
- **建议**：`DirectGateway` 改为多连接表（`worker_id → endpoint`）+ 每连接独立
  `ControlPlane`；或为 PC Worker 增加 MQTT 传输。若短期不做，README 必须降级为
  "单 Worker（MQTT 链路除外）"。

### B2 取消/中断缺失（高）✅

- **现象**：serve 方法表、REPL 命令、agent 循环都没有 cancel；`sekai.cancel`
  要求 8 字节大端 cmd_id，而 Router 的 `worker_tool.args` 是 UTF-8 文本，
  cmd_id 也只在成功结果里回传。
- **证据**：`crate/sekai-router/src/serve.rs`（dispatch 方法表无 cancel）；
  `crate/sekai-router/src/session.rs`（`/help` 无 cancel）；
  `crate/sekai/src/sekai_runtime.rs::handle_builtin`（`sekai.cancel` 8 字节）；
  `crate/sekai-router/src/tools.rs`（args 为字符串→`as_bytes`）；`command_result_json`
  成功分支才有 `cmd_id`。
- **影响**：失控的 `pc.job`/`pc.ask`（最长 300s）只能杀 Worker 进程；DSH 的
  `exec.signal` 只停止本地等待（插件自己也这么写）。
- **建议**：serve 增加 `cancel{cmd_id}`；`worker_tool`/`worker_task` 在
  Accepted/Started 后回传 cmd_id；REPL 加 `/cancel`；agent 循环加中止位。

### B3 长任务阻塞与无进度（高）✅

- **现象**：serve 是单 dispatcher 线程同步 `gateway.call`；REPL/TUI 在输入循环里
  内联 `agent.run`；ingress 用 `run_lock` 串行化所有 prompt；V1 没有 Progress 帧
  （文档已声明，但产品层没有替代方案）。
- **证据**：`serve.rs`（dispatcher 线程 + `gateway.call`）；
  `session.rs::run_goal`；`tui.rs::submit`；`prompt_server.rs:203`（`run_lock`）；
  `crate/sekai-router/src/control.rs`（`Phase::Progress => Ok(())`）。
- **影响**：一个 300s 的 `pc.ask`/`pc.job` 期间，`status`/`workers`/`shutdown`/`link_off`
  全部排队；TUI 无重绘、PgUp/PgDn/Ctrl-C 不生效；ingress 多连接只能串行。
- **建议**：serve 命令异步化（submit → 事件/cancel/轮询）；TUI/REPL 后台执行 +
  事件通道；至少先给 serve 加请求级超时与配合 B2 的取消。

### B4 执行预算与进程管理（高）✅/◐

| 子项 | 现象与证据 | 影响 |
| --- | --- | --- |
| B4.1 默认预算不匹配 | Router 默认 `DEFAULT_DDL_MS=1000` + `TOOL_TIMEOUT_EXTRA_MS=2000` = 3s；`pc.exec` 自身允许 10000ms；`worker_tool.ddl_ms` 描述为 "omit for default"，system prompt 说"不确定就省略" | 3–10s 的命令（多数真实命令，例如大仓库的 `git status`）被 Router 判 timeout；Worker 仍在跑，结果晚到被丢（B8） |
| B4.2 同步阻塞帧循环 | `PcExecTool::call` 在 `Runtime::feed` 内同步 sleep/poll（最长 10s）；DDL 是**执行后**才比较（`sekai_runtime.rs` `elapsed >= ddl_ms`） | 一次 `pc.exec` 阻塞整个 Worker：无法处理 keep-alive/其他命令；超时后副作用已发生、输出被丢弃 |
| B4.3 进程树不杀 | 超时/DDL/Drop 只 `child.kill()` 直接子进程（Windows 为 `cmd /C`）；无 Job Object/`taskkill /T`/进程组 | 孙进程（cargo、编译器、脚本子进程）继续运行并占着管道 |
| B4.4 输出泵竞态 | `wait_pumps(2, 200ms)` 返回值被丢弃后 `take()`；截断标记只在达到容量时设置 | 200ms 内的尾部输出静默丢失，且不标 `[[truncated]]` |
| B4.5 `pc.ls` 可超协议上限 | 只有 200 行上限，没有字节上限；Runtime 对 >4096 字节 payload 返回 `ResultTooLarge` | 中等规模目录直接失败，而不是截断列表 |
| B4.6 `pc.read` 全量读 | `std::fs::read` 先读整文件再截 4000 字节 | 大文件（日志/二进制）可能 OOM |
| B4.7 编码 | `pc.read`/`pc.exec` 返回原始字节，文档称"UTF-8 文本"；Windows `cmd /C` 重定向是 OEM 代码页 | 非 UTF-8 输出在 Router 侧编码/展示异常 |

- **建议**：`pc.exec`/`pc.job` 改为子进程/任务化（至少不阻塞帧循环）；预算分层
  （Tool 默认 ≥ 工具内部上限）；Windows 用 Job Object 杀树；`pc.ls` 加字节预算；
  `pc.read` 用有界读取；明确编码契约（UTF-8 lossy 或 `encoding=hex`）。

### B5 内层 prompt 与在途命令互斥（中高）✅

- **现象**：`pc.ask` 作为 Task 占住 Worker 的唯一 in-flight 名额；Worker 把 prompt
  发到 Router ingress 后，Router 内层 agent 复用**同一个 gateway**，再派同一 Worker
  的命令会 `worker_busy`。
- **证据**：`link_builder.rs:50`（`max_inflight_per_worker = 1`）；
  `control.rs`（`inflight >= max` → `WorkerBusy`）；
  `prompt_server.rs::run_prompt`（用 `PromptCore` 里同一 `GatewayHandle`）。
- **影响**：单 Worker 下 `pc.ask` 的内层 agent **一定**无法使用该 Worker 的工具，只能
  纯问答；serve `prompt` 在已有在途命令时同理。但 system prompt 仍在指导它调用
  `worker_tool`，结果是 `worker_busy` 而非有效回答。
- **建议**：prompt 运行使用独立的 in-flight 策略（按 cmd 而非按 Worker 计数），
  或为 ingress 指定专用 Worker；或在文档中明确"prompt 只做纯问答"并调整 system prompt。

### B6 二进制内置工具与时钟校准（中高）✅

- **现象**：`sekai.set_time`（8 字节）、`sekai.cancel`（8 字节）、
  `sekai.workers.set`（DeviceList 编码）都只接受二进制；Router 的工具通道只有
  UTF-8 文本；全仓 Router 无 `set_time` 调用点。`--allow-admin` 帮助文本却宣传
  这些工具。
- **证据**：`crate/sekai/src/sekai_runtime.rs::handle_builtin`；
  `crate/sekai-router/src/tools.rs`（文本 args）；
  `crate/sekai-router/src/config.rs`（`--allow-admin` 文案）；
  `docs/sekai-protocol.md` §13.2（要求"连接后用 `sekai.set_time` 校准 Worker 时间"）；
  `crate/sekai-embedded/src/time.rs`（ESP32 未校准 → `None` → Hello `unix_ms=0`）。
- **影响**：MQTT/ESP32 的 `unix_ms` 恒为 0、`sekai.time` 返回 0、命令 `Expired`
  检查永不生效；运行时无法更新 Jump 目标；`sekai.cancel` 不可达（见 B2）。
- **建议**：为这三类能力增加 Router 侧专用方法（文本编码 hex/JSON，或 serve
  新方法）并在连接后自动对时；或修改协议增加文本化管理命令。

### B7 Jump（中）✅

- **现象**：
  - Router 侧：`CommandKind` 无 Jump，`direct.rs` 注释"Jump/迁移，当前预留"。
  - Worker 侧：`FallingBack` 状态没有 deadline，`process_inbox` 在 jump 活跃时被跳过，
    `on_publish` 拒绝且不回 `Failed`；原端点也失败时命令被静默丢弃。
  - PC Worker：`disconnect_requested` 置位后从不清零，一旦触发会每次重连后
    立刻关闭新链路。
- **证据**：`crate/sekai-router/src/control.rs`（`CommandKind::parse("jump") == None`）、
  `crate/sekai-router/src/direct.rs`；`crate/sekai/src/sekai_runtime.rs`
  （`link_failed`、`next_deadline` 只看 `Draining`、`process_inbox` 条件）；
  `crate/sekai-worker/src/host.rs`（`disconnect_requested` 只置位不清零）；
  `crate/sekai-worker/src/bin/sekai-worker.rs`（每轮检查该标志）。
- **影响**：MQTT 链路迁移只在固件"成功路径"可用；失败回退会挂死；PC 直连
  一旦收到 Jump 会进入重连-关闭循环。
- **建议**：Router 实现 Jump 命令与超时；Worker `FallingBack` 加 deadline 与
  `JumpUnreachable` 失败路径；`on_link_up` 清零 `disconnect_requested`。

### B8 可观测性与晚到事件（中）✅

- **现象**：
  - 超时后 pending 被移除，最终事件到达只 `note_error("忽略未知 cmd_id")`，
    `last_error` 不出现在 `serve status` / REPL `/status` / `workers`。
  - `active_tasks` 只来自最后一次 Hello，Router 不维护自己的在途账本。
  - 失败结果 JSON 无 `cmd_id`/`elapsed_ms`/`phases`（成功才有）。
  - `--trace` 在 `--serve` 下直接被跳过，serve 的 prompt 用 `TraceWriter::disabled()`。
  - LWT 的 timestamp 在 `Runtime::new` 时固化，`set_time` 后重连仍用旧时间。
  - Outbox 淘汰的兜底分支可能淘汰最终/Hello/Offline 甚至 in-flight 条目。
- **证据**：`control.rs::finish_from_stdout` / `note_phase`；`serve.rs::status_json`；
  `session.rs::status_lines`；`worker.rs`（`active_tasks` 来自 Hello）；
  `tools.rs::command_result_json` 失败分支；`bin/sekai-router.rs`（`--serve` 提前返回）；
  `serve.rs:867`；`sekai_runtime.rs`（Will 构造、`Outbox::evict_one`）。
- **影响**：超时/断链后无法回答"任务到底执行了没有"；监控数据陈旧；审计缺字段。
- **建议**：`status` 暴露 `last_error` 与在途命令；Router 维护 in-flight 账本；
  失败结果补齐字段；serve 打开 trace；LWT 在 CONNECT 时重建。

### B9 prompt ingress 无认证（中高）✅

- **现象**：`start_ingress` 只按 method 分发，无 token/对端校验；每连接一个线程、
  无上限/限流；`validate_prompt_addr` 接受任意 `HOST:PORT`（含 `0.0.0.0`）且无警告；
  文档把策略开关称为"鉴权"。
- **证据**：`prompt_server.rs`（`start_ingress`/`handle_ingress_line`）；
  `config.rs::validate_prompt_addr`；`docs/sekai-pc-agent.md` §4、`docs/sekai-architecture.md` §5
  （"鉴权"措辞已在本次审计中改为"只做策略授权"）。
- **影响**：本机任意进程（或误绑局域网的场景）可以消耗 API key、在策略内派工；
  若开了 `--allow-write`，等于把 `pc.exec` 暴露给能连上该端口的人。
- **建议**：ingress 增加 token（env/CLI 文件）、默认只允许回环、连接/速率上限、
  记录 remote addr；文档改为"只做策略授权，不做调用方认证"。

### B10 HTTPS 依赖外部 helper（中）✅

- **现象**：`http://` 进程内实现；`https://` 交给 Node.js 或 OpenSSL curl（临时文件
  交换请求/响应，Authorization 走环境变量）。
- **证据**：`crate/sekai-router/src/http.rs` 模块头注释与 `execute_https`。
- **影响**：裸 Rust 部署（无 node/curl）无法调用 DeepSeek API，`serve prompt`/agent
  形态不可用；发布物多了外部运行时依赖。
- **建议**：引入 rustls（可选 feature）或随包提供内置 helper；至少把依赖写进
  README 前置要求并在启动时给出明确诊断。

### B11 DSH 插件 / serve 细节（中低）✅

| 子项 | 证据 | 影响 |
| --- | --- | --- |
| link 地址丢失 | serve `link` 返回 `addr`；插件读 `linked.listen_addr ?? linked.mqtt_addr`（`plugins/sekai-dsh/lib/router.js`） | `linkDesc` 只剩模式，排障看不到实际端口 |
| 无链路断开通知 | `link_state` 只在 `link`/`link_off` 时发；poll 线程只 diff Worker | TCP 对端断开后插件 `ready` 仍为 true |
| 长任务 DDL 提示缺失 | 插件 `sekai_worker_task` 描述只写"omit for the worker default"；Router 默认 1s | DSH 模型跑 `pc.ask` 默认必超时（配合 B4） |
| 工具描述不实 | `plugins/sekai-dsh/README.md` 原写"5 个工具都经插件串行转发"（本次已修） | `sekai_worker_status` 是本地工具 |
| Web UI / 分发 | `docs/sekai-dsh-plugin.md` §6 | 状态面板、npm 发布、内置二进制仍是"下轮" |

- **建议**：修 `addr` 读取；`link_state` 在链路断开时补发；任务工具描述加
  `pc.ask/pc.job 建议 ddl_ms>=60000`；README 措辞修正；Web UI/分发按计划推进。

---

## 3. 核心协议/运行时缺口（K）

> 结论：`crate/sekai` 对 V1 协议**基本完整**（帧、Hello、去重、DDL、cancel、
> Jump 成功路径、LWT、retain/QoS 降级、常量与错误码都有实现与测试）。
> 以下为真正的缺口或文档不一致。

### K1 文档的内置工具名与代码不一致（中）✅

- 文档写 `sekai.devices` / `sekai.devices.set`（`docs/sekai-protocol.md` 内置工具表），
  代码实现 `sekai.workers` / `sekai.workers.set`（`sekai_runtime.rs::BUILTIN_TOOLS`）。
  Hello 的字段文档叫 `devices`，代码字段是 `workers`（线布局相同）。
- 影响：按文档实现的第三方 Router 会得到 `UnknownTool`。
- 建议：以代码为准统一文档命名（或反过来，但需要迁移）。

### K2 MQTT 5 增强 AUTH 实际不可用（中）✅

- 客户端支持 `Client::auth()` 与 `Event::Auth`，`ClientConfig` 暴露
  `authentication_method/data`；但 `Runtime::drain_events` 把 `Event::Auth(_)` 直接丢弃，
  无人调用 `auth()`。
- 影响：一旦启用增强认证，broker 的 AUTH challenge 被忽略，连接挂到 CONNECT 超时。
- 建议：要么在 Runtime 里实现 AUTH 应答，要么删除/隐藏 `authentication_method` 配置。

### K3 Jump 回退没有失败路径（中）✅

- `link_failed` 只处理 `JumpState::Switching`；`FallingBack` 无 deadline；
  `advance_jump` 只推进 `Draining`；`process_inbox` 在 jump 活跃时跳过；
  `on_publish` 拒绝且不回 `Failed`。
- 影响：原端点也不可达时，命令静默丢失、状态卡死。
- 建议：`FallingBack` 加超时 → `JumpUnreachable` 失败帧；拒绝期间回 `Failed`。

### K4 broker 会话可能跨重启重放 Stdin（中，条件性）◐

- 默认 `clean_start: false`、`session_expiry_interval: 300`，而 `cmd_id` 去重窗口
  是纯内存的；broker 可在 Worker 停机期间排队 Stdin，重启后投递 → 去重窗口为空 →
  重复执行。
- 影响：仅嵌入式 MQTT 链路；PC 直连合成 `session_present:false`，不触发。
- 建议：clean start + 明确 session expiry，或把去重窗口持久化/落盘；文档补充该风险。

### K5 SUBSCRIBE 失败被吞掉（低）✅

- `let _ = self.client.subscribe(...)`；`on_suback` 只把 `subscribed` 设为
  "任一 non-error"。订阅被拒后只会在下次 CONNECT 重试。
- 建议：记录 SUBACK 错误码并在 `status`/Hello 可见；必要时按退避重订阅。

### K6 LWT 时间戳固化为 Runtime 创建时刻（低）✅

- Will 在 `Runtime::new` 构造一次，之后每次 CONNECT 复用；文档说时间戳取
  CONNECT 那一刻。`set_time` 后重连仍带旧/0 时间。
- 建议：CONNECT 时重建 Will payload。

### K7 Outbox 淘汰可能丢最终事件（低）✅

- `evict_one` 优先淘汰非最终/非 Hello/Offline，但兜底会淘汰任意 Queued
  （含最终事件），最后甚至取 in-flight 条目；`emit` 不看 `enqueue` 返回值。
- 建议：淘汰时计数并暴露 `dropped_events`；对最终事件保留独立预算（文档 §9.6 已有承诺）。

### K8 低优先观察

- `Client::unsubscribe` 有实现但 Runtime 不用（V1 无需求，属死 API）。
- `Stdout::validate` 对任意 `Offline` 都跳过 `seq` 检查（文档限定 LWT 的 `seq=0`）。
- `sekai_clock.rs` 注释说"运行时限频校准"，实际 `handle_builtin` 每次调用都直接校准。
- MQTT 客户端文档说 `now` 是 Unix ms，Runtime 实际传 `monotonic_ms`；内部自洽，
  但按客户端文档写嵌入层会混用时间基准。

---

## 4. PC Worker / GPIO 缺口（P）

### P1 进程树不杀（高）✅
见 B4.3；`capability.rs` 的 `pc.exec`/`pc.job` 都只 `child.kill()`。

### P2 子进程继承 Worker stdin（高）✅

- `spawn_shell` 只 pipe `stdout`/`stderr`，随后 `drop(child.stdin.take())` 是空操作
  （`Child::stdin` 为 `None`）→ 子进程继承 Worker 的 stdin。
- 影响：stdio 模式下子进程可抢走 V1 帧字节；`--llm dsh` 下可抢走 prompt JSONL。
- 建议：显式 `.stdin(Stdio::null())`。

### P3 `pc.exec` 阻塞 + DDL 后验（高）✅
见 B4.2。

### P4 输出泵竞态（中）✅
见 B4.4。

### P5 `pc.ls` 超协议上限（中）✅
见 B4.5。

### P6 `pc.read` 全量读（中）✅
见 B4.6。

### P7 编码假设（低）✅
见 B4.7。

### P8 prompt 通道健壮性（中）◐

| 子项 | 证据 | 影响 |
| --- | --- | --- |
| Router 通道忽略响应 id | `llm_channel.rs`：`Some((_, result)) => result` | 同 socket 上任何 `ok` 行都会被当成本次回答 |
| DSH 响应缓冲不清理 | `feed_line` 入队，只有匹配 id 才移除 | 迟到/无主响应无限增长 |
| 无握手/探活 | 两条通道都不发 `ping`；插件缺失时只能等满 `SEKAI_PC_ASK_MS` | `pc.ask` 默认 300s 后 `timeout` |
| ask 线程不可取消 | `thread::spawn(...ask...)`，任务超时/DDL 只丢弃接收端 | 线程最多再占 300s socket |
| 连接无超时/重试 | `TcpStream::connect` 无超时，单次尝试 | 网络抖动直接 `unreachable` |

### P9 直连链路的"未确认重放"不成立（中）✅

- `WorkerHost::pump_outgoing` 在把帧交给写循环**之前**就向 Runtime 喂了合成
  PUBACK，`on_puback` 立刻从 outbox 移除；写失败时该事件已"已交付"。
- 与 `crate/sekai-worker/src/host.rs` 的"未确认的出站事件保留，重连后重放"及
  README 的承诺矛盾。
- 影响：直连链路无真正的至少一次投递；断链瞬间的最终事件会丢。
- 建议：把 PUBACK 推迟到写成功之后（或实现真正的发送队列与重放）。

### P10 直连链路无活性探测（中）✅

- Runtime 的 PINGREQ 被 `WorkerHost` 就地应答，没有报文跨过 TCP；读超时只是 tick。
- 影响：对端静默死亡（拔网线/休眠）不会触发"断线按指数退避重连"。
- 建议：加应用层心跳/读超时判定，或使用 TCP keepalive。

### P11 `disconnect_requested` 不自清（中）✅
见 B7；PC 直连一旦收到 `DISCONNECT` 会进入"连上→立刻关"的循环。

### P12 虚拟 GPIO 无法注入输入（中）✅

- Worker 注册 `VirtualGpio::new(family)`，不读任何初始电平；`set_external_level`
  只在测试里用。`gpio.read` 在宿主上恒为 0。
- 影响：文档的"完整闭环演示"只覆盖低电平；无法演示/验收输入路径。
- 建议：加 `--gpio-inputs IO4=1,PA5=0`（或复用一个 env）并接入注册。

### P13 真实硬件未验证（置后）○
见 D1/D2。

### P14 `host_registries` 与文档不符（低）✅

- `crate/sekai-worker/README.md` 模块表写 `host_registries()` 提供"`pc.*` 工具/任务"，
  实际它是 4 个 gpio 工具 + `echo`/`sleep`（Router 测试用）；PC agent 能力在
  `agent_registries*`。
- 建议：修正文档（或删掉这个仅测试用的入口）。

---

## 5. 工程闭环缺口（C）

### C1 CI 覆盖面不足（中高）✅

- 只有 ubuntu + 默认 feature；无 `cargo fmt --check` 门禁；clippy 不阻断
  （注释里承认有 5 条既有告警）。
- `direct_gateway`/`direct_link` 的真实子进程 E2E、`real_api`（7 个）全部
  `#[ignore]`；CI 也不先构建二进制，所以这些测试永远不会在 CI 跑。
- `crate/sekai-worker/tests/smoke_stdio.py` 未接入任何流水线。
- 无 `esp32` 固件构建；无真实 broker（测试用进程内最小 broker）。
- 无 Windows job，而 `pc.exec`（`cmd /C`）、`pc.info`（`COMPUTERNAME`）、路径行为
  恰恰是 Windows 主场景。
- **结论**：CI 绿 ≠ 主闭环可跑；需要 Windows matrix、fmt 门禁、E2E 步骤、
  feature 组合检查（stm32f1/f4、send）与 esp32 构建（可为可选 job）。

### C2 版本与发布（中）✅

- `git tag` 为空；无 CHANGELOG/release；workspace `0.1.0` 与提交信息 "v0.2" 不一致；
  `sekai_worker::VERSION` 硬编码 `"0.1.0"`；插件 `0.1.0`。
- DSH 插件不内嵌二进制，用户必须自己 `cargo build --release` 并配置路径。
- 建议：确定版本策略（0.1.0 还是 0.2.0）、打 tag、加 CHANGELOG、发布矩阵至少
  覆盖 win-x64/linux-x64。

### C3 MSRV 声明错误（中）✅

- README（中/英）写 Rust **1.85+**，但代码使用 let-chains（`tools.rs::check_policy`、
  `direct_gateway.rs` 等，1.88 才稳定）；无 `rust-toolchain.toml`/`rust-version`。
- 影响：按 README 准备的环境首次 `cargo build` 直接失败。
- 本次状态：README 已改 1.88+；`rust-toolchain.toml` 与各 crate 的 `rust-version`
  尚未加（工作包 W9）。
- 建议：加 `rust-toolchain.toml`（stable）与各 crate 的 `rust-version`。

### C4 P2 状态没有退出条件（低）✅

- README/架构表 P2 只写"进行中"，没有清单与判定标准。
- 本次状态：已在 README（中/英）与架构文档补上"单 Worker 闭环完成 + 剩余项链接"，
  并以本审计 §1/§9 作为退出条件。
- 建议：在架构文档列出 P2 DoD（单 Worker 闭环、多 Worker 或明确限制、取消/长任务、
  DSH 验收、观测、CI 覆盖主路径），完成一项勾一项。

---

## 6. 置后与验证缺口（D）

### D1 ESP32 实机闭环未验证（置后）○

- 固件 `sekai-fw` 2026-11-03 构建成功；但无实机 WiFi/MQTT/真实 GPIO 的端到端记录，
  CI 无固件构建；没有 host 侧 MQTT Worker/模拟器可替代实机演练。
- 叠加 B6（时钟未校准）、K3（Jump 回退）、K4（会话重放），P3 的"MQTT 链路回归"
  目前没有可复现证据。

### D2 STM32 实板未验证（置后）○

- `stm32f1`/`stm32f4` PAC 后端已实现，代码自述"真实寄存器路径只做交叉编译检查"；
  无板级工程（链接脚本/启动文件）与电气验证。

---

## 7. 明确非目标（不计为缺口）

以下项在文档中已声明为**非目标**，本审计不按缺陷计：

- MQTT TLS/加密、DNS 主机名、静态 IP、NVS/WiFi 配网、AP 模式、BLE、企业 WiFi。
- ESP32-S3/C3/C6 变体；STM32 其他子家族与网络链路。
- OTA/双分区、light/deep sleep。
- GPIO 中断/边沿/PWM/ADC/复用功能；二进制工具参数。
- Jump 的任务 handoff；Progress 事件；多帧分片；帧加密/签名。
- Ticket/Slot/Eval/Outbox/Channel 等 DSH 概念（刻意不引入）。
- Web UI 状态面板、npm 发布、插件内嵌二进制（"下轮"）。
- 串行执行工具调用、超时/断链不自动重试（设计取舍，见 `control.rs`、`prompt.rs`）。

---

## 8. 文档/代码漂移清单

| # | 位置 | 漂移 | 状态 |
| --- | --- | --- | --- |
| D1 | `README.md` / `README.en.md` | MSRV 1.85+，实际 let-chains 需 1.88+ | 本次已修 |
| D2 | `crate/sekai-router/src/config.rs`（usage） | `--serve` 写"不调用 LLM，不需要 API key"，但 v1.1 `prompt` 需要 | 本次已修 |
| D3 | `docs/sekai-architecture.md` §5、`docs/sekai-pc-agent.md` §4、`crate/sekai-router/README.md` | "ingress 鉴权沿用当前会话策略"——实际无认证 | 本次已修 |
| D4 | `plugins/sekai-dsh/README.md` | "5 个工具都经插件串行转发"——`sekai_worker_status` 是本地工具 | 本次已修 |
| D5 | `crate/sekai-router/README.md` 退出码表 | 缺 130（TUI Ctrl-C）与 1（异常 I/O）；6 的 `AgentError::Gateway` 分支不可达 | 本次已修 |
| D6 | `docs/sekai-ai.md` §7.2/§8.6、`docs/sekai-ai-plan.md` | 引用已移除的 `.build/check-lib.sh` / `build.py` | 本次已修（加注记） |
| D7 | `docs/sekai-architecture.md` §2.1 | "2025-11 起" 应为 "2026-11 起" | 本次已修 |
| D8 | `README.md` / `README.en.md` | "多轮可续"未说明不保留跨目标上下文 | 本次已修 |
| D9 | `docs/sekai-pc-agent.md` | Worker 4096 字节上限与 Router 展示 3584 字节未区分 | 本次已修 |
| D10 | `docs/sekai-protocol.md` 内置工具表 | `sekai.devices*` vs 代码 `sekai.workers*`；Hello 字段 `devices` vs 代码 `workers` | 待修（协议文档需单独评审） |
| D11 | `docs/sekai-protocol.md` GPIO 段、`docs/sekai-gpio.md` §5.8 | "`Unsupported`（0x000A）" 错误；0x000A 是 `Cancelled`，`Unsupported` 是 0x000E | 本次已修 |
| D12 | `docs/sekai-protocol.md` §12.3 | 仍描述 `sekai-host` 与 `SEKAI_*` 环境变量，现行是 `sekai-worker` CLI | 本次加历史注记 |
| D13 | `docs/sekai-gpio.md` §1.2 | "host 默认 11 个工具"——默认 11 是内置+`pc.*`，加 `--gpio` 是 15 | 本次已修 |
| D14 | `crate/sekai-worker/README.md` 模块表 | `host_registries()` 被写成 `pc.*`，实际是 gpio + echo/sleep | 本次已修 |
| D15 | `crate/sekai-worker/README.md` / `host.rs` | "重连后重放未确认事件"——实际写前已 PUBACK（见 P9） | 待修（先定行为再改文档） |
| D16 | `docs/sekai-pc-agent.md` §4 | prompt 文本 8192 字节，但 `pc.ask` 走 V1 Task 载荷 ≤4096 | 本次加注记 |
| D17 | `docs/sekai-gpio.md` §1.2 交付表 | ESP32/F1/F4 标"真实硬件"，实际仅交叉编译检查、无实板验证 | 本次已修（标注） |
| D18 | `docs/sekai-protocol.md` §12.3 示例工具/task | "6 内置 + echo + 4 gpio = 11" 与现行能力不符 | 本次加历史注记 |

---

## 9. 建议工作包（W1–W10，需另行批准）

| 包 | 内容 | 依赖 | 验收 |
| --- | --- | --- | --- |
| W1 | 多 Worker 直连：多连接表 + 每连接独立 ControlPlane；或 PC Worker 加 MQTT | — | 两个 Worker 同时在线，可分别派工并出现在 `workers` |
| W2 | 取消/长任务：serve `cancel` + 异步命令 + REPL `/cancel` + cmd_id 回传 + agent 中止位 | W1 可并行 | 长任务可在任意时刻取消；取消后 Worker 无残留 |
| W3 | 内层 prompt 并发：按 cmd 计 in-flight / prompt 独立预算；文档明确边界 | W2 | 单 Worker 下 `pc.ask` 内层 agent 可调用工具 |
| W4 | 管理工具与时钟：`set_time`/`cancel`/`workers.set` 的文本或 JSON 编码 + 连接后对时 + Expired 生效 | — | 时钟校准后 `sekai.time` 正确；`sekai.cancel` 可用 |
| W5 | Jump：Router ControlPlane Jump 命令/超时/`JumpOutcome`；Worker `FallingBack` 超时与失败路径；PC Worker latch 清零 | W4 | MQTT 迁移成功/失败/回退均有测试 |
| W6 | ingress 安全：token、默认回环、连接/速率上限、审计日志 | — | 无 token 的连接被拒绝；0.0.0.0 需显式开关 |
| W7 | 观测：status 暴露 last_error/在途；失败结果补字段；trace 补全；serve 打开 trace；LWT 重建 | W2 | 超时/断链后可定位 |
| W8 | DSH 插件：`addr`、link-down 通知、`ddl_ms>=60000` 提示、Web UI/npm | W2 | 插件状态与链路真实一致 |
| W9 | 工程闭环：CI Windows matrix + fmt 门禁 + E2E/real_api 接入 + feature 组合 + esp32 构建 + release/tag/CHANGELOG + MSRV | — | 主路径在 CI 可重复验证 |
| W10 | P3：host 侧 MQTT Worker/模拟器、真 broker 集成、ESP32 实机验收清单、STM32 板级工程 | W9 | P3 有可复现的端到端记录 |

**建议顺序**：W9 → W2 → W1 → W4 → W3 → W6 → W7 → W5 → W8 → W10。

---

## 10. 复现与验证

本审计的每条证据都可用只读命令复核，例如：

```bash
# B1：直连网关只有单个端点状态
grep -rnE "enum IoState|IoState::Connected|accept_link_timeout" crate/sekai-router/src/direct_gateway.rs

# B2：serve 方法表里没有 cancel
grep -rnE '"link"|"link_off"|"tool_call"|"cancel"' crate/sekai-router/src/serve.rs

# B4：默认预算与 pc.exec 预算
grep -rnE "DEFAULT_DDL_MS|TOOL_TIMEOUT_EXTRA_MS|exec_timeout_ms|omit for default" \
  crate/sekai-router/src crate/sekai-worker/src

# B6：二进制内置工具参数
grep -rnE "args.len\(\) != 8|sekai.cancel|sekai.set_time|sekai.workers.set" crate/sekai/src/sekai_runtime.rs

# P2：子进程 stdin
grep -rnE "stdout\(Stdio::piped\)|stdin\(Stdio::null\)|child.stdin.take" crate/sekai-worker/src/capability.rs

# P9：写前 PUBACK
grep -rnE "Packet::Publish|Packet::Puback|pump_outgoing" crate/sekai-worker/src/host.rs

# B9：ingress 无认证
grep -rnE "handle_ingress_line|validate_prompt_addr|TcpListener::bind" crate/sekai-router/src
```

**验证限制**：本机 `cargo test` 无法运行（`cargo` 无法 spawn `rustc`，`os error 5`）。
需要运行时验证的结论应使用仓库现有门禁（`.github/workflows/ci.yml`）或正常 cargo 环境：
`cargo test --workspace`、`cargo clippy --workspace --all-targets`、
`cargo test -p sekai-router --test real_api -- --ignored`（需 `DEEPSEEK_API_KEY` 与
`SEKAI_REAL_API=1`）、`node plugins/sekai-dsh/scripts/smoke.mjs`。

---

## 11. 变更记录

- 2026-11-04：首次审计（`main` @ `32027b4`）。覆盖主路径、核心协议、PC Worker/GPIO、
  工程闭环与置后项；同时修复 D1–D9、D11–D14、D16–D18 等文档漂移（见 §8 状态列），
  仅剩 D10（协议文档内置工具命名，需单独评审）与 D15（直连重放语义，需先定行为）。
- 2026-11-04（实现轮，`feat/gap-closure`）：B1–B11、K1–K8、P1–P12/P14 全部落地并有测试
  （见 §0.4）；D10 随 K1 一并修复（协议文档统一为 `sekai.workers*`），D15 随 P9 修复
  （PUBACK 写成功后才补，失败保留重放）。同时完成：
  - serve 协议 v1.2（`async:true` / `cancel` / `commands` / `admin` / `jump` / status 诊断字段，
    纯 additive）；
  - ingress 访问控制（回环默认、远程 token + hello 握手、连接/速率上限、审计日志）；
  - Router 进程内 HTTPS（`native-tls`，`SEKAI_HTTPS_MODE=helper` 可回退）；
  - CI 增加 fmt 门禁、Windows job、可选 real_api job；新增 `scripts/gate.sh` 统一门禁。
  - 明确仍置后：P13 实机 E2E（D1/D2）、B11 的 Web UI/npm 发布、C2/C4。
  - 已知取舍：`pc.exec` 线类型 Tool→Task（策略仍按名字归写操作）；Jump 活跃期命令由
    “静默忽略”改为 `Failed(Busy)`（协议文档 §7.3.5 已同步）；直连链路 PINGREQ/PINGRESP
    真实跨链路（Router 与 PC Worker 需同版本）；`clean_start=true` 为默认。
