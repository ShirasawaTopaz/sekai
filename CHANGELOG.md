# Changelog

本文件记录面向使用者的变更。格式参考 Keep a Changelog；版本遵循
[SemVer](https://semver.org/lang/zh-CN/)。

## [Unreleased]

### 新增

- **上下文治理（工具结果滚动压缩 + 上下文预算）**：agent 循环只保留最近
  `SEKAI_CONTEXT_RETAIN_STEPS`（默认 4，`0` 关闭）轮工具结果原文，更早的 `tool`
  消息替换为带成功/失败、命令名与 160 字符摘要头的紧凑占位（`{"compacted":true,...}`）；
  只改内容、不删消息，调用/结果配对与 `reasoning_content` 不受影响。估算字节超过
  `SEKAI_CONTEXT_BUDGET_BYTES`（默认 65536，`8192..=1048576`）时不再发起下一次 LLM
  请求，以 `context_budget` 结束。`llm_request` trace 追加 `message_bytes`，新增
  `context_prune` / `context_budget_exceeded` 事件。system prompt 的 GPIO 段只在在线
  Worker 通告 `gpio.*` 时输出（纯 PC Worker 每步少约 1.5 KiB）。见
  [docs/sekai-ai.md](docs/sekai-ai.md) §4.4 与
  [crate/sekai-router/README.md](crate/sekai-router/README.md)「上下文治理」。
- **插件页显示（DSH 插件管理界面）**：bundle 与两行插件各自带本地化标题/简介/图标——
  `locale/{en,zh}.json` 的 `meta.title`/`meta.description`/`meta.detail`、顶层
  `icon`（`icon.svg`）、详情文档 `docs/DETAIL.md` / `docs/DETAIL.zh.md`（各一张
  `preview.svg`），以及子路径插件专属的 `locale/router|worker/*.json` 与
  `./router/icon`、`./worker/icon`（`icons/router.svg`、`icons/worker.svg`）。
  行设置表单来自 `static Config`，所有字段补齐英文 `.description()` 与默认值；
  保存写 profile 的 `cordis.patch.yml` 覆盖层。安装脚本对 desktop profile 的
  `--dump-config` 被桌面端独占管理的情况改为「跳过校验」而非告警，并在
  「依赖已装但 bundle 未选中」时给出重新启用的可操作提示。见
  [docs/sekai-dsh-plugin.md](docs/sekai-dsh-plugin.md) §1「插件页显示」与
  [plugins/sekai-dsh/README.md](plugins/sekai-dsh/README.md)。
- **Worker → DSH 工具桥（`--dsh-tools` / 插件 `dshTools`）**：PC Worker 新增两个 Task
  `dsh.tools`（发现本机 DSH 工具）与 `dsh.call`（args 为
  `{"tool":"<名>","arguments":{...},"timeout_ms":<可选>}`），经与 prompt 通道并列的
  **带外 JSONL**（worker stdin/stdout ↔ DSH worker 插件）调用本机 DSH 工具；**V1 帧不变**。
  插件 config 新增 `dshTools`（`enabled` 默认 `false`，`allow` 默认只读
  `[read,glob,grep]`，另有 `deny`/`cwd`/`agentPreset`/`maxConcurrent`/`timeoutMs`/
  `maxResponseBytes`），`--dsh-tools` 仅 connect/TCP，env `SEKAI_DSH_TOOL_MS` 默认
  300000。Router 侧 `dsh.tools`/`dsh.call` 是 Task，由 `allow_tasks` 统一门控；system
  prompt 会先提示用 `dsh.tools` 发现工具、长调用给 `ddl_ms >= 60000`。桥调用在独立
  bridge agent 上执行，硬拒绝 `run_code` 与 `sekai_worker_*`。见
  [docs/sekai-pc-agent.md](docs/sekai-pc-agent.md) §3.4 与
  [docs/sekai-dsh-plugin.md](docs/sekai-dsh-plugin.md) §3.3。
- **一键安装脚本 `scripts/install-dsh-plugin.mjs`**：一条命令完成
  `cargo build`（`--no-build` / `--bin-dir` 可跳过或复用已有构建）→ 把
  `sekai-router`/`sekai-worker` 原子部署到 `<DSH_HOME>/sekai/bin` →
  `dsh plugin --profile <p> add file:<repo>/plugins/sekai-dsh`（自动登记 profile
  `dsh.profile.bundles`）→ 自检（部署文件、依赖与 bundles、从 profile import 并
  `resolveBin` 闭环、`--dump-config` 两行）。`--dry-run` 只看计划、`--uninstall`
  卸载、`--strict` 让组合校验失败也算失败；profile 自动选择为 `--profile` >
  `$DSH_PROFILE` > 唯一 profile > `desktop`。装/升级后需重启 DSH 会话生效。见
  [docs/sekai-dsh-plugin.md](docs/sekai-dsh-plugin.md) §1。
- **插件二进制零配置自动发现**：`resolveBin` 解析顺序改为显式
  `routerBin`/`workerBin` config > `SEKAI_ROUTER_BIN`/`SEKAI_WORKER_BIN` >
  `$DSH_HOME/sekai/bin/<name>(.exe)` > `<repo>/target/release/<name>(.exe)` >
  PATH；一键脚本负责部署到 `$DSH_HOME/sekai/bin`，安装后默认零配置，
  PATH / 环境变量 / profile 行级覆盖不再是主路径（仍可显式覆盖，config 优先级最高）。
- **PC Worker 自动连接开关（DSH worker 插件）**：新增 config `autoConnect`（默认 `true`）——
  DSH 启动且 worker 启用时自动连接**上次连接的 Router**（插件从 worker stderr 的
  `TCP 已连接 <addr>` / `切换到新端点 <addr>` 识别，按 `workerId` 持久化到
  `$DSH_HOME/sekai/worker-state.json`，无记录回退 `routerAddr`；`stateFile` 可覆盖路径）。
  `autoConnect:false` 时启动不 spawn，新增 `sekai_worker_connect` / `sekai_worker_disconnect`
  手动控制工具（`publishControlTools` 控制注册）：手动断开抑制退避重启直到再次 connect，
  运行中 connect 到新地址会断旧连新；`sekai_worker_status` 追加 `autoConnect` /
  `routerAddr` / `lastRouterAddr`。插件冒烟新增 autoConnect、持久化与手动控制用例。见
  [docs/sekai-dsh-plugin.md](docs/sekai-dsh-plugin.md) §3.2 与
  [plugins/sekai-dsh/README.md](plugins/sekai-dsh/README.md)。
- **飞书对话入口**（`--feishu`）：企业自建应用机器人经 WebSocket 长连接接收
  IM 事件、REST 回复原消息，复用 prompt 通道共享核心（`deny_llm_relay`、全局
  4 路并发、策略/网关快照）；支持 `/cancel`、`/status`、allowlist（默认拒绝）、
  每 chat 限流与 stderr 审计；建连引导见
  [skills/feishu-setup](skills/feishu-setup/SKILL.md)，协议与安全模型见
  [docs/sekai-feishu.md](docs/sekai-feishu.md)。零新增依赖（SHA-1/base64/
  WebSocket/pbbp 均为仓内实现）。
- **LLM 思考模式**：`SEKAI_ROUTER_REASONING_EFFORT` 启用 thinking 并指定强度
  （`low`/`high`/`max` 等，缺省仍关闭）；响应 `reasoning_content` 会回填到
  assistant 消息（带 tools 的多轮必需），`LlmConfig`/`ChatResponse` 同步扩展。
- **飞书会话上下文（多轮记忆）**：Router 在飞书前端内直接读取 `chat_id`/
  `chat_type`/`thread_id`/`root_id`/`parent_id`/`message_id`/发送者/消息时间，
  注入 agent 的会话元数据段；并按会话（话题消息按 `chat_id/thread_id`）在内存
  中保留最近 `SEKAI_FEISHU_CONTEXT_TURNS` 轮问答（默认 8，0 关闭）作为
  user/assistant 上下文。仅内存、不落盘、不写审计/trace；重启清空、WebSocket
  重连保留；长连接与上下文全部在 Router，Worker 不参与飞书。见
  [docs/sekai-feishu.md](docs/sekai-feishu.md) §5.5。

### 变更

- **`context_budget` 状态**：`AgentStatus` 新增 `ContextBudget`；serve/ingress 的
  `prompt` 结果 `status` 增加 `context_budget`，飞书回复「（上下文预算用尽，已停止）」，
  CLI `--json` 同名字段；退出码 5 的含义扩为「达到步数上限或上下文预算」。状态为
  additive（旧消费方仍可只认 `completed`/`max_steps`）。
- **agent 步数上限按 Worker 形态区分**：直连 PC Worker（`--link stdio/tcp`，含 DSH
  插件 `--serve --link tcp` 与飞书默认 tcp 形态）默认**不设上限**，只由模型完成、
  取消（REPL `/cancel`、TUI Ctrl-C、serve `cancel`、飞书 `/cancel`）或 LLM 错误
  终止；非 PC Worker（`--link mqtt`，嵌入式）**保留原上限 12**。显式
  `--max-steps`/`SEKAI_MAX_STEPS`（1..=64）与 prompt 请求级 `max_steps`（1..=32）
  对两类链路仍然优先。未显式指定时，交互 REPL/TUI 的 `/link` 与 serve 的 `link`
  请求在运行期改写链路后也会按新链路重算（同步到会话与 prompt hub）。
  `--help`、README、`docs/sekai-ai.md`、`docs/sekai-pc-agent.md`、
  `docs/sekai-feishu.md` 同步更新。

### 修复

- **适配 DSH 0.2.2-rc.3**：`plugins/sekai-dsh/package.json` 新增
  `engines.dsh = ">=0.2.2-rc.3 <0.3.0"`；工具 `output.schema` 按宿主 JSON Schema
  子集核对（只认 `type`/`oneOf`/`properties`/`required`/`additionalProperties`/
  `items`/`enum`/`const` 与注释键）。DSH 0.2.2 起 profile 条目不再保证按 bundle
  顺序完成挂载，worker 插件激活时 `ctx.get('tools')` 可能是 `undefined`，导致
  `sekai_worker_status` / `sekai_worker_connect` / `sekai_worker_disconnect` 被静默
  跳过；改用 `ctx.inject(['tools'], …)` 等工具服务出现后再注册（宿主没有工具
  注册表时只跳过注册，spawn 与 prompt 中转照常）。本机 rc.3 验证：挂载后工具
  注册表可见 7 个 `sekai_*` 工具，`sekai_status` 走完整执行管线返回
  `is_error:false`，bundle 冒烟 47/47。见
  [docs/sekai-dsh-plugin.md](docs/sekai-dsh-plugin.md) §1「DSH 版本兼容」与
  [plugins/sekai-dsh/README.md](plugins/sekai-dsh/README.md)。
- 飞书长连接心跳：改为以协议层 **WS ping/pong** 判定存活（`WsMessage::Pong`
  刷新存活时间），修复真连环境每 2×PingInterval 误判「心跳超时」反复重连的问题；
  应用层 CONTROL pong 的解析保留（差异记录见
  [docs/sekai-feishu.md](docs/sekai-feishu.md) §14「心跳实现」）。

## [0.2.0] - 2026-11-04

本轮把 [缺口审计](docs/sekai-gap-audit.md) 的 B/K/P 组全部闭环（P13 实机除外）。
门禁：`scripts/gate.sh`（fmt / check / clippy / test / 插件 smoke 24 项）。

### 新增

- **多 Worker 直连**：TCP 监听常驻 accept，按 Worker id 同时路由多个 Worker；
  同 id 新连接顶替旧连接（B1）。
- **取消与异步**：serve 的 `tool_call`/`task` 支持 `async:true`（立即返回 `cmd_id`，
  完成后发通知），新增 `cancel {cmd_id}`、`commands`；REPL `/cancel`、TUI `Ctrl-C`
  可中止在途长任务；agent 循环支持取消（B2/B3）。
- **管理命令**：serve `admin`（`set_time` / `cancel` / `workers.set`）与 `jump`
  （均受 `--allow-admin`）；连接后按 Hello 时钟偏差自动 `sekai.set_time`（B6/B7）。
- **可观测性**：`status` 增加 `link_info` / `inflight` / `counters{late_results,
  dropped_events}` / `clock_synced` / `late_results[]`；失败结果补
  `cmd_id`/`elapsed_ms`/`phases`；晚到终态进入环形账本（B8）。
- **ingress 访问控制**：默认仅回环；非回环需 `SEKAI_INGRESS_ALLOW_REMOTE=1` +
  `SEKAI_INGRESS_TOKEN`（首行 `hello` 握手）；连接/速率上限与审计日志（B9）。
- **进程内 HTTPS**：默认 `native-tls`（Windows schannel / 系统 OpenSSL），不再要求
  node 或 curl；`SEKAI_HTTPS_MODE=helper` 可回退（B10）。
- **虚拟 GPIO 输入注入**：`sekai-worker --gpio-inputs IO4=1,PA5=0`（P12）。
- **统一门禁**：`scripts/gate.sh`；CI 增加 fmt 门禁、Windows job、可选 real_api job。

### 变更

- `pc.exec` 从 Tool 改为 **Task**（不阻塞 Worker 帧循环）；名字/参数/结果格式不变，
  Router 按 Worker 通告的 tasks 列表自动按 Task 下发，策略仍按名字归写操作（B4.2/P3）。
- 默认执行预算 `SEKAI_DDL_MS` 1000 → **10000**，Tool 超时余量 2000 → **5000**（B4.1）。
- 同一 Worker 在途命令上限 1 → **4**（B5）：`pc.ask` 内层 agent 可在任务运行期间
  继续调用同一 Worker 的工具。
- Jump 活跃期的新命令由“静默忽略”改为 **`Failed(Busy)`**（协议文档 §7.3.5 同步）。
- MQTT 默认 **`clean_start=true` + `session_expiry=0`**，避免 broker 跨重启重放
  Stdin；`RuntimeConfig` 拒绝可重放组合（K4）。
- 直连链路 **PINGREQ/PINGRESP 真实跨链路** + 1.5×keepalive 读空闲判定；Router 与
  PC Worker 需同版本配套（P10）。
- 直连投递语义：PUBACK 改为**写成功后**补发，写失败保留重放（P9）。

### 修复

- 超时/取消杀**整棵进程树**（Windows 先 `taskkill /T` 再 kill；Unix 进程组 SIGKILL），
  子进程 stdin 设为 null，不再抢 Worker 帧流（P1/P2）。
- 输出泵竞态不再静默丢尾部（未收全标 `[[truncated]]`）；结果统一 UTF-8 lossy（B4.4/B4.7）。
- `pc.ls` 增加字节预算、`pc.read` 改为有界读取（B4.5/B4.6）。
- `pc.ask` prompt 通道按响应 id 过滤、迟到响应有界、连接超时、可取消（P8）。
- MQTT：AUTH challenge 应答、SUBACK 错误可见并退避重订阅、LWT 时间戳每次 CONNECT 重建、
  Outbox 最终事件独立预算（K2/K5/K6/K7）。
- `Stdout::lwt_offline` 构造 + Offline `seq=0` 校验；`sekai.set_time` 1s 限频（K8）。
- 协议文档统一内置工具名为 `sekai.workers` / `sekai.workers.set`（K1/D10）。

### 已知限制 / 置后

- P13：ESP32/STM32 **实机**端到端验证仍缺（D1/D2），仅保留验收清单。
- DSH 插件的 Web UI 与 npm 发布仍未做。
- 直连链路保活为真实报文：Worker 帧循环若被同步任务阻塞超过 2×keepalive 会被判死
  （当前唯一同步阻塞是 `pc.exec` 的 10s 上限，不会触发）。
- `clean_start=true` 意味着 Worker 停机期间 broker 不会为其缓存 Stdin（有意取舍）。

## [0.1.0] - 2026-10

- 首个可用版本：V1 帧协议、`no_std` 运行时、Router/PC Worker、MQTT 5 客户端、
  `--serve` JSONL、prompt ingress、DSH 插件、Apache-2.0 许可。
