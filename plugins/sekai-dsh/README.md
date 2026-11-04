# @sekai/dsh-plugin

面向 [DSH（DeepSeek Harness）](https://developer.deepseek.com)的 Sekai 双插件 bundle：把
`sekai-router` 与 `sekai-worker` 两个 Rust 二进制接入 DSH Host，让模型直接操作 Sekai 世界里的
Worker（PC agent 的 `pc.*` 命令/文件能力、可选 GPIO）。

- **router 插件**（`@sekai/dsh-plugin/router`，组件 id `sekai-router`）：spawn
  `sekai-router --serve`（无头 JSONL over stdio 模式），并向 DSH 注册 4 个 `sekai_*` 工具。
- **worker 插件**（`@sekai/dsh-plugin/worker`，组件 id `sekai-worker`）：spawn
  `sekai-worker connect <addr> --llm dsh` 管理本机 Worker（PC agent）的生命周期，注册
  `sekai_worker_status` 状态工具；默认通道 `dsh` 下还把 Worker 的 `pc.ask` prompt
  中转到 Router 的 prompt ingress（见下文「prompt 中转」）。

纯 ESM JavaScript，无构建步骤、无运行时 npm 依赖（只用 `node:child_process` / `node:net`
/ `node:readline` 等内置模块）；`@deepseek-ai/cordis` 与 `@deepseek-ai/schemastery` 为 optional peer 依赖。

```
DSH Host
 ├─ sekai-router 插件 ──spawn── sekai-router --serve --link tcp   (JSONL over stdio)
 │                          ├─ 请求: {"id":n,"method":"tool_call"|"task"|"prompt"|...,"params":{...}}
 │                          └─ 通知: worker_online / link_state / ready
 └─ sekai-worker 插件 ──spawn── sekai-worker connect 127.0.0.1:7421 --worker-id pc-worker --llm dsh
        │                    └─ stderr: 阶段化中文进度行
        └─ prompt 中转（llm:'dsh'）：worker stdout 的 prompt JSONL ──TCP──▶ Router prompt ingress
                                                       回答按 id 回投 worker stdin
```

## 安装

由 DSH 管理者执行（本仓库不代办）：

```
# dsh 插件管理器（本地绝对目录安装；<repo> = 本仓库的克隆目录）
dsh plugin add <repo>/plugins/sekai-dsh
```

或手动在 profile 的 bundles 配置中列出 `@sekai/dsh-plugin`。bundle 自带的
`cordis.patch.yml` 会插入两行组件（默认 config），无需手工编辑 patch：

```yaml
- insert:
    - id: sekai-router
      name: '@sekai/dsh-plugin/router'
      config: { enabled: true }
    - id: sekai-worker
      name: '@sekai/dsh-plugin/worker'
      config: { enabled: true }
```

已保存的 profile 行覆盖优先于补丁默认值生效。

## 构建二进制

两个插件分别驱动仓库里的 Rust 二进制：

```
cargo build --release -p sekai-router -p sekai-worker
# 产物：target/release/sekai-router(.exe) 与 target/release/sekai-worker(.exe)
```

二进制解析顺序（两个插件一致）：**config 显式路径 > 环境变量 > PATH**。

- router：`routerBin` config 键 > `SEKAI_ROUTER_BIN` > PATH 上的 `sekai-router`
- worker：`workerBin` config 键 > `SEKAI_WORKER_BIN` > PATH 上的 `sekai-worker`

config 值与默认二进制同名时视为「未显式配置」（此时环境变量优先生效）。最省事的配法是把
`routerBin` / `workerBin` 设为 `target\release` 下 exe 的绝对路径。

## 配置

### sekai-router

| 键 | 类型 | 默认 | 说明 |
| --- | --- | --- | --- |
| `enabled` | boolean | `true` | `false` 时不 spawn、不注册工具 |
| `link` | `'tcp'\|'stdio'\|'mqtt'` | `'tcp'` | 链路模式，同时作为 `--link` 旗标与 `link` 请求参数 |
| `listenAddr` | string | `'127.0.0.1:7421'` | tcp 模式作 `listen_addr`、mqtt 模式作 `mqtt_addr` 传给 `link` |
| `routerBin` | string | `'sekai-router'` | serve 二进制（解析顺序见上） |
| `extraArgs` | string[] | `[]` | 追加在 `--serve --link <mode>` 之后的额外 CLI 参数 |
| `autoLink` | boolean | `true` | ready 后自动发 `link` 请求与 `policy.set`（每次重启重放） |
| `policy.allowWrite` | boolean | `false` | GPIO 写（`gpio.write` / `gpio.config`） |
| `policy.allowTasks` | boolean | `false` | Task 下发 |
| `policy.allowAdmin` | boolean | `false` | 管理/其他用户工具 |
| `policy.workerFilter` | string | 空 | 只允许操作该 worker id；空 = 不限制 |
| `requestTimeoutMs` | integer ≥1000 | `120000` | 单个 JSONL 请求的本地超时 |

### sekai-worker

| 键 | 类型 | 默认 | 说明 |
| --- | --- | --- |
| `enabled` | boolean | `true` | `false` 时不 spawn、不注册工具 |
| `workerBin` | string | `'sekai-worker'` | worker 二进制（解析顺序见上） |
| `routerAddr` | string | `'127.0.0.1:7421'` | `connect` 的目标地址 |
| `workerId` | string | `'pc-worker'` | `--worker-id` |
| `llm` | `'dsh'\|'router'\|'off'` | `'dsh'` | prompt 通道（spawn 时映射 `--llm`）：`dsh`=插件中转（默认）；`router`=worker 直连 ingress；`off`=关闭 |
| `promptAddr` | string | `'127.0.0.1:7422'` | Router prompt ingress 地址：`router` 模式作 `--prompt-addr` 传给 worker；`dsh` 模式为中转目标 |
| `promptTimeoutMs` | integer ≥1000 | `300000` | dsh 中转等待响应的本地超时；超时回 `timeout` 错误信封 |
| `gpio` | string\|null | `null` | 虚拟 GPIO 家族（`esp32\|stm32f1\|stm32f4`），非 null 时传 `--gpio <family>`；默认关闭（PC agent 形态）。旧键 `family` 兼容映射到本键 |
| `restartBackoffMaxMs` | integer ≥500 | `30000` | 重启退避上限 |
| `publishStatusTool` | boolean | `true` | 是否注册 `sekai_worker_status` |

## 工具

模型侧描述为英文；5 个工具都经插件串行转发（serve 端 dispatcher 本身串行执行，
长命令会阻塞后续调用）。

| 工具 | 参数 | 返回 |
| --- | --- | --- |
| `sekai_status` | 无 | `{ready, mode, linkDesc, workerCount, policy, routerPid?, version?}` |
| `sekai_list_workers` | 无 | workers 响应原样：`{count, workers[]}`（worker_id/online/boot_id/firmware/uptime_ms/tools/tasks/active_tasks…） |
| `sekai_worker_tool` | `{worker_id, tool, args, ddl_ms?}` | tool_call 响应原样（command_result 形状） |
| `sekai_worker_task` | `{worker_id, task, args, ddl_ms?}` | task 响应原样（command_result 形状） |
| `sekai_worker_status` | 无 | `{running, pid?, restarts, uptimeMs, lastStderr[], llm}` |

约定（镜像 router `tools.rs` 的语义）：**先调 `sekai_list_workers`，worker id、工具名、任务名
必须来自其结果**。`args` 是原始 UTF-8 文本（如 `gpio.info help`）；`ddl_ms` ∈ [1, 300000]。

错误一律以 `{error:{code, message, hint?}}` 信封返回、不向 DSH 抛异常：

- `spawn_error`：二进制缺失/不可执行（hint 指引 `cargo build --release` + 设绝对路径/环境变量）；
- `not_ready`：serve 进程不在（崩溃重启中）；
- `timeout`：本地等待超时（命令可能仍在执行）；
- `aborted`：DSH 侧取消了本次调用——**只是本地停止等待，router/worker 侧命令照跑**；
- 其余（`policy_denied` / `invalid_arguments` / `unknown_method` / `link_error` …）：router
  协议错误的 code 原样透传。

## JSONL 协议 v1（冻结）

router 插件与 `sekai-router --serve` 之间的协议（与 Rust 侧任务冻结一致，见 task「Router
--serve」）：

- 请求（stdin 每行）：`{"id":<整数>,"method":"<名>","params":{...}}`
- 响应（stdout 每行）：`{"id":..,"ok":true,"result":{...}}` 或 `{"id":..,"ok":false,"error":{"code":"...","message":"..."}}`
- 通知（无 id）：`{"event":"ready"}`、`{"event":"link_state","desc":"..","ready":bool}`、
  `{"event":"worker_online","worker_id":"..","worker":{...}}`、`{"event":"worker_offline","worker_id":".."}`
- 方法：`link{mode,listen_addr?,worker_bin?,worker_args?,mqtt_addr?}`、`link_off`、`workers`、
  `tool_call{worker_id,tool,args,ddl_ms?}`、`task{worker_id,task,args,ddl_ms?}`、`policy.get`、
  `policy.set{allow_write?,allow_tasks?,allow_admin?,worker_filter?}`、`status`、`shutdown`、
  `prompt{text,max_steps?}`（**v1.1 追加，additive**：把 prompt 交给 Router 内置 LLM 回路，
  响应 `result:{answer,status,steps,usage}`，需 API key）
- 客户端对无法匹配 id、也无 event 的行丢弃并 `logger.warn`；人类日志走 stderr。

## prompt 中转（worker 插件，`llm:'dsh'`）

worker 以 `--llm dsh` 启动时，`pc.ask` 任务的 prompt 由 worker 插件中转到 Router 的
prompt ingress（`--prompt-addr`，默认 `127.0.0.1:7422`；协议全量定义见
[../../docs/sekai-pc-agent.md](../../docs/sekai-pc-agent.md) §4）：

```text
worker stdout ──▶ 插件逐行识别 {"id":N,"method":"prompt","params":{...}} 请求行
                     │ net.connect(promptAddr)：每条 prompt 一条独立 TCP 连接
                     ▼
              Router prompt ingress（127.0.0.1:7422）
                     │ 响应行按 id 匹配（不匹配的行丢弃告警）
                     ▼
worker stdin ◀── 原样回投响应（{"id":N,"ok":true,"result":{...}} 或错误信封）
```

时序与清理：

- **并发**：每条 prompt 独立连接、id 各自匹配，互不占用；
- **本地超时**：等待 `promptTimeoutMs`（默认 300000，≥1000）后回
  `{"id":N,"ok":false,"error":{"code":"timeout","message":...}}`；
- **dispose**：插件停止时挂起请求回 `aborted`（本地码）并销毁连接；worker 进程
  意外退出时挂起请求只断开、不回投（stdin 已亡）。

错误码表（回投 worker stdin 的 `error.code`）：

| code | 来源 | 含义 |
| --- | --- | --- |
| `unreachable` | 插件本地 | ingress 连接失败/提前断开，或 `promptAddr` 非法 |
| `timeout` | 插件本地 | 等待响应超过 `promptTimeoutMs` |
| `aborted` | 插件本地 | 插件 dispose，请求被清理 |
| `llm_unavailable` / `not_linked` / `no_workers` / `invalid_arguments` / `agent_error` | ingress 透传 | Router 侧生成回答失败（无 API key / 无链路 / 无 Worker / 参数非法 / Agent 循环失败） |

`llm:'router'`（worker 自连 ingress）与 `llm:'off'` 时插件不中转：worker stdout 的
JSON 行与非 JSON 行一样只记宿主日志。

## 生命周期与容错

- **router**：ready 通知 → autoLink（`link` + `policy.set`）；进程意外退出按 500ms→30s
  指数退避重启并在 ready 后重放 link/policy、退避复位；worker 计数由
  `worker_online`/`worker_offline` 通知与 `workers` 响应维护，进程死亡时清零重建。
  dispose 顺序：`shutdown` 请求（短超时）→ 宽限等待 → kill → 强杀。
- **worker**：stderr 逐行进 40 行环形缓冲并写宿主日志；意外退出按 500ms 起步指数退避重启
  （封顶 `restartBackoffMaxMs`），稳定运行 ≥5s 后的退出把退避复位；dispose 直接 kill
  （connect 模式无优雅协议），挂起的 prompt 中转请求回 `aborted` 并销毁连接。

## 本地冒烟（无 Rust）

```
node --check lib/index.js && node --check lib/shared.js && node --check lib/router.js \
  && node --check lib/worker.js && node --check scripts/smoke.mjs   # 语法
node scripts/smoke.mjs                                              # 冒烟
# 或：npm run check / npm run smoke
```

`scripts/smoke.mjs` 用 node 写的假 serve（按协议 v1 回放通知与响应）验证 router 侧桥接
（分帧、请求-响应、超时、本地中止、dispose、崩溃重启、autoLink 重放、四工具、错误信封、
schema 卫生），用假 worker 脚本验证 worker 侧重启与状态工具；另有假 prompt worker +
假 ingress（node `net` server 回放响应）验证 dsh 通道 prompt 中转（成功回投、并发独立
连接、不可达/超时错误信封、`llm:'off'` 不转发、argv 旗标、dispose 清理）。全部断言
通过 exit 0。

## 设计取舍

- **schemastery 延迟加载**：任务范式是 `import z from '@deepseek-ai/schemastery'`（peer）；
  本插件改为 `createRequire` 守卫式加载，宿主内行为相同（`static Config = z.object({...})`），
  仓库冒烟（无 node_modules）下回退内部默认值兜底，插件不崩。
- **工具 output schema 刻意放宽**（`additionalProperties: true`、无 required）：工具值是冻结
  协议响应的**原样透传**，Rust 侧 v1 内新增字段不应被宿主判为 invalid output；输入
  `parameters` 才是严格契约（`additionalProperties: false`、type 只用单字符串）。
- **「无值」用省略不用 null**（`routerPid`/`version`/`pid`）：宿主的 JSON Schema 校验拒绝
  type 数组，可空字段以省略表达（对齐 dsh-ops 的约定）。
- **`sekai_status` 以本地状态视图为主、`status` 响应尽力合并**：Rust 侧 `status` 的键名
  未在协议里逐字冻结（`link描述`/`worker数` 等是语义描述），防御式映射多种常见键名，保证
  工具返回卡面要求的七个字段。
- **worker 不走 JsonlProcess**：connect 模式没有请求-响应式 JSONL 协议（stderr 是人类
  进度行），只需要 spawn + stderr 环形缓冲 + 退避重启，复用 shared 的
  `RingBuffer`/`nextBackoffMs`。dsh 通道的 prompt 中转是**单向识别**：只把形如
  `{"id","method":"prompt","params"}` 的 stdout 行转发，其余行照旧记日志——语义与
  JsonlProcess 的「任意方法请求-响应」不同，故独立实现（stdin 由此从 `ignore` 改为
  `pipe`，承载响应回投）。
- **worker 插件不 `static inject ['tools']`**：宿主没有 tools 服务时也能加载（只跑生命周期），
  状态工具在注册表可用时才发布，缺 registry 只 warn。
- **插件类不用 `#private`**：Cordis 会把插件方法派发到派生接收者上执行，`#private` 品牌检查
  在这种接收者上会误抛（dsh-dcp 的符号键教训）；统一用普通属性 + 闭包。

## English quick reference

Two host plugins bridging the Sekai Rust binaries into DSH: the **router** plugin spawns
`sekai-router --serve` (frozen JSONL-over-stdio protocol v1, plus the additive v1.1
`prompt` method) and registers `sekai_status` / `sekai_list_workers` / `sekai_worker_tool`
/ `sekai_worker_task` (serially forwarded; aborting a call only stops waiting locally —
the command keeps running); the **worker** plugin manages `sekai-worker connect <addr>
--llm <channel>` — a PC agent with a 40-line stderr ring buffer, 500ms→cap exponential
backoff restarts, and (with the default `llm:'dsh'`) prompt relaying: lines shaped like
`{"id","method":"prompt","params"}` on worker stdout are forwarded over independent TCP
connections to the router prompt ingress (`promptAddr`, default `127.0.0.1:7422`) and the
id-matched response is written back to worker stdin verbatim; local failures return
`unreachable` / `timeout` envelopes and disposal returns `aborted`. Virtual GPIO is
opt-in via the `gpio` config key (legacy `family` maps to it). It exposes
`sekai_worker_status` (now including the `llm` channel). Binaries resolve as config path >
`SEKAI_ROUTER_BIN` / `SEKAI_WORKER_BIN` > PATH; build them with
`cargo build --release -p sekai-router -p sekai-worker`. No build step, no runtime npm
deps; schemastery is an optional peer. All tool failures return a structured
`{error:{code,message,hint}}` envelope (`spawn_error` / `not_ready` / `timeout` /
`aborted` or the router-side protocol code) and never throw at DSH. Run
`node scripts/smoke.mjs` for the Rust-free smoke suite.
