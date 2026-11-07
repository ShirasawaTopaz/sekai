# sekai-router

Sekai **Router**：常驻路由器进程。连接/监听 Worker 链路，维护 Worker 能力快照与
生命周期（Hello/Offline、`boot_id`/`seq` 去重），把自然语言目标经 LLM
（DeepSeek Chat Completions function tools）评估、拆解、派工给 Worker，并汇总
结果。与 Worker 的关系见
[docs/sekai-architecture.md](../../docs/sekai-architecture.md)。

> **已知限制**：直连链路（stdio/TCP）同一时刻只支持 **1 个 Worker**（`DirectGateway`
> 持有单个端点，TCP 监听器在连接期间不再 accept）；多 Worker 目前只在 MQTT/嵌入式
> （置后阶段）链路成立。见[缺口审计](../../docs/sekai-gap-audit.md) B1。

```text
sekai-router
 ├─ 链路（--link）
 │   ├─ stdio：拉起 sekai-worker 子进程，stdin/stdout 即链路
 │   ├─ tcp ：监听 127.0.0.1:7421，等 sekai-worker connect 连入
 │   └─ mqtt：订阅 {root}/dev/+/out|state（嵌入式 Worker，置后阶段）
 ├─ Worker 注册表（worker.rs）：在线状态、能力（tools/tasks）、去重窗口
 ├─ 策略（tools.rs::Policy）：默认只读；写/Task/管理分别解锁；发布 Stdin 前强制
 ├─ prompt ingress（prompt_server.rs，--prompt-addr）：TCP JSONL，把 prompt
 │   交给内置 LLM 回路生成回答（Worker pc.ask / DSH 插件 / 任意客户端）
 └─ 大脑
     ├─ agent.rs + llm.rs：自然语言目标 → function tools 调用 → 派工 → 汇总
     └─ --serve：无头 JSONL 网关（DSH 主会话充当大脑；另有 prompt 方法）
```

## 运行形态

| 形态 | 命令 | 说明 |
| --- | --- | --- |
| 批处理 | `sekai-router --goal "读取 IO4" [--link tcp]` | 一个目标后退出；`--json` 输出 JSON 结果 |
| 交互 REPL | `sekai-router`（无 `--goal`） | `/start` 建连向导、`/link`、`/workers`、`/wait`、`/policy`；非 `/` 输入即目标 |
| 全屏 TUI | `sekai-router --tui` | 状态栏 + 滚动输出 + 输入行；非终端时自动回退 REPL |
| 只看链路 | `sekai-router --dry-run` | 连接并列出 Worker，不调用 LLM |
| 无头服务 | `sekai-router --serve` | stdin/stdout JSONL 协议 v1（`link`/`workers`/`tool_call`/`task`/`policy.*`/`prompt`），供 [DSH 插件](../../plugins/sekai-dsh/README.md)驱动 |
| 飞书对话入口 | `sekai-router --feishu` | WebSocket 长连接收 IM 事件、REST 回复原消息；同会话注入元数据与最近 N 轮问答（内存，`SEKAI_FEISHU_CONTEXT_TURNS`）；详见 [docs/sekai-feishu.md](../../docs/sekai-feishu.md) |

## prompt ingress 与 `--prompt-addr`

`--prompt-addr <HOST:PORT|off>`（env `SEKAI_PROMPT_ADDR`）开一个 **TCP JSONL
prompt ingress**：任意客户端（Worker 的 `pc.ask --llm router`、DSH worker 插件中转、
手工 `nc`）连上来发 `{"id":N,"method":"prompt","params":{"text":"…"}}`，Router 用内置
LLM 评估/派工回路生成回答后按 `id` 回投。默认：交互/serve 模式 `127.0.0.1:7422`，
批处理模式 `off`。ingress 只做**策略授权**、**不做调用方认证**（默认仅回环监听）；
经 prompt 通道发起的运行**拒绝再派 `pc.ask`**（防递归）。serve JSONL 同能力以
`prompt` 方法暴露（v1.1 additive）。
协议全量定义（请求/响应/错误码/ping）见
[docs/sekai-pc-agent.md](../../docs/sekai-pc-agent.md) §4。

## 常用选项与环境变量

```text
--goal <TEXT> / --goal-file <PATH>   批处理目标（二选一）
--link <mqtt|stdio|tcp>              链路类型（默认 mqtt）
--listen <HOST:PORT>                 tcp 监听地址（默认 127.0.0.1:7421）
--prompt-addr <HOST:PORT|off>        prompt ingress 监听地址（env SEKAI_PROMPT_ADDR；
                                     交互/serve 默认 127.0.0.1:7422，批处理默认 off）
--worker-bin <PATH>                  stdio 链路拉起的 Worker 程序
--worker <ID>                        只允许操作指定 Worker
--allow-write / --allow-tasks / --allow-admin
                                     解锁 GPIO 写与 pc.exec/pc.write / Task（含
                                     pc.job/pc.ask）/ 管理与其他工具（默认全关）
--model / --base-url / --max-steps / --ddl-ms / --timeout-ms / --wait-worker-ms
--serve / --dry-run / --tui / --json / --trace <PATH> / -h / --version
```

环境变量：`DEEPSEEK_API_KEY`（调 LLM 必需；也接受 `API_KEY`）、
`DEEPSEEK_BASE_URL`（默认 `https://api.deepseek.com`）、`SEKAI_ROUTER_MODEL`
（默认 `deepseek-flash`）、`SEKAI_LINK`、`SEKAI_LISTEN_ADDR`、`SEKAI_PROMPT_ADDR`、
`SEKAI_MQTT_ADDR`、`SEKAI_MQTT_USERNAME/PASSWORD`、`SEKAI_TOPIC_ROOT`
（默认 `sekai/v1`）、`SEKAI_WORKER_BIN`、`SEKAI_ROUTER_TRACE`，以及调参用的
`SEKAI_MAX_STEPS` / `SEKAI_DDL_MS` / `SEKAI_TIMEOUT_MS` / `SEKAI_WAIT_WORKER_MS` /
`SEKAI_HTTP_TIMEOUT_MS`，以及飞书入口的 `SEKAI_FEISHU_*`（凭据/allowlist/
`SEKAI_FEISHU_CONTEXT_TURNS` 等，见 [docs/sekai-feishu.md](../../docs/sekai-feishu.md) §8）。
完整清单以 `sekai-router --help` 为准。

> **agent 步数上限按 Worker 形态区分**：`--max-steps` / `SEKAI_MAX_STEPS` 显式给出时
> 两类链路都用该值；缺省时**直连 PC Worker（`--link stdio/tcp`）不设上限**，只由模型
> 完成 / 取消（`/cancel`、Ctrl-C、serve `cancel`、飞书 `/cancel`）/ LLM 错误终止；
> **非 PC Worker（`--link mqtt`，嵌入式）保留 12**。prompt 请求级 `max_steps`
> （1..=32）与 `SEKAI_FEISHU_MAX_STEPS` 仍是显式上限。
>
> 未显式指定时，**运行期改写链路也会重算**：交互 REPL / TUI 的 `/link tcp|stdio|mqtt`
> 与 serve 的 `link` 请求都会把新链路的默认值同步到会话与 prompt hub（`/link off`
> 后保持原值；prompt 本身会因未建链失败）。显式 `--max-steps` 则固定不变。

## 退出码

| 码 | 含义 |
| --- | --- |
| 0 | 成功 |
| 1 | 未预期的启动/线程/I/O 失败（非命令失败） |
| 2 | 配置错误 |
| 3 | Worker 不可用 |
| 4 | LLM/API 错误 |
| 5 | 达到最大步数 |
| 6 | Worker 命令最终失败 |
| 130 | TUI 被 Ctrl-C 中断 |

> 6 的 `AgentError::Gateway` 分支目前没有构造点（命令失败走 `report_outcome`）；
> 见[缺口审计](../../docs/sekai-gap-audit.md) B8。TUI 内部还有一个 64 的哨兵值，
> 不会作为进程退出码暴露给正常使用。

## 模块地图

`agent.rs`（LLM 回路）· `llm.rs`（API 客户端）· `gateway.rs` /
`direct_gateway.rs` / `direct.rs`（WorkerGateway 与直连接入）· `worker.rs`
（注册表）· `control.rs`（命令下发与超时）· `session.rs`（命令层，REPL 与 TUI
共用）· `repl.rs` / `tui.rs`（前端）· `serve.rs`（JSONL 无头模式）·
`prompt_server.rs`（prompt ingress）· `feishu.rs`（飞书前端）· `ws.rs` /
`pbbp.rs` / `sha1.rs`（飞书长连接协议面）· `tools.rs`（Policy）· `prompt.rs`
（提示词/会话上下文段）· `link_builder.rs` / `host_link.rs` · `topic.rs` ·
`http.rs` · `trace.rs`（脱敏 trace）· `config.rs`。

协议细节见 [docs/sekai-protocol.md](../../docs/sekai-protocol.md)；PC agent 与
prompt 通道见 [docs/sekai-pc-agent.md](../../docs/sekai-pc-agent.md)；serve 协议与
插件见 [docs/sekai-dsh-plugin.md](../../docs/sekai-dsh-plugin.md)。
