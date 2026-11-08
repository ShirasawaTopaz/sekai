# @sekai/dsh-plugin

面向 [DSH（DeepSeek Harness）](https://developer.deepseek.com)的 Sekai 双插件 bundle：把
`sekai-router` 与 `sekai-worker` 两个 Rust 二进制接入 DSH Host，让模型直接操作 Sekai 世界里的
Worker（PC agent 的 `pc.*` 命令/文件能力、可选 GPIO）。

- **router 插件**（`@sekai/dsh-plugin/router`，组件 id `sekai-router`）：spawn
  `sekai-router --serve`（无头 JSONL over stdio 模式），并向 DSH 注册 4 个 `sekai_*` 工具。
- **worker 插件**（`@sekai/dsh-plugin/worker`，组件 id `sekai-worker`）：spawn
  `sekai-worker connect <addr> --llm dsh` 管理本机 Worker（PC agent）的生命周期，注册
  `sekai_worker_status` 状态工具；**`autoConnect` 开关**决定 DSH 启动且 worker 启用后是否
  自动连接**上次连接的 Router**（持久化在 `$DSH_HOME/sekai/worker-state.json`，无记录回退
  `routerAddr`），关闭时用 `sekai_worker_connect` / `sekai_worker_disconnect` 手动控制；默认通道
  `dsh` 下还把 Worker 的 `pc.ask` prompt 中转到 Router 的 prompt ingress（见下文「prompt 中转」）。

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

在仓库根目录、**普通终端**用一条命令完成构建、部署二进制、安装 bundle 与自检：

```bash
node scripts/install-dsh-plugin.mjs [--profile <name>]
```

脚本做四件事：`cargo build --release -p sekai-router -p sekai-worker`（`--no-build`
跳过；`--bin-dir <dir>` 用已有构建）→ 把两个二进制原子部署到 `<DSH_HOME>/sekai/bin/`
（`DSH_HOME` 未设置 = `~/.dsh`）→ `dsh plugin --profile <p> add
file:<repo>/plugins/sekai-dsh`（DSH 自动把包名写进 profile 的 `dsh.profile.bundles`）→
自检（部署文件 / 依赖与 bundles / 从 profile import 并 `resolveBin` 闭环 /
`--dump-config` 两行）。

**重启 DSH 会话**后出现 `sekai_*` 工具（运行中的会话不会热加载 bundle）；升级 =
`git pull` 后重跑同一条命令。其他选项：`--dry-run` 只看计划、`--uninstall` 卸载
（保留二进制与 worker 状态文件）、`--strict` 让 `--dump-config` 校验失败也算失败；
profile 自动选择 `--profile` > `$DSH_PROFILE` > 唯一 profile > `desktop` > 报错列出，
`--help` 查看全部。完整说明见 [docs/sekai-dsh-plugin.md §1](../../docs/sekai-dsh-plugin.md)。

> 脚本要调用 `cargo` / `dsh` 子进程：在 DSH 受限沙箱会话（workspace-write）里 Node
> 子进程会被拒（`spawn EPERM`），请在普通终端运行，或让 DSH 以提权模式运行。

bundle 自带的 `cordis.patch.yml` 会插入两行组件（默认 config），无需手工编辑 patch：

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

### 二进制解析顺序（零配置）

两个插件一致（`lib/shared.js::resolveBin`）：**config `routerBin`/`workerBin` >
`SEKAI_ROUTER_BIN`/`SEKAI_WORKER_BIN` > `$DSH_HOME/sekai/bin/<name>(.exe)` >
`<repo>/target/release/<name>(.exe)` > PATH**。

- router：`routerBin` config 键 > `SEKAI_ROUTER_BIN` > `$DSH_HOME/sekai/bin/sekai-router(.exe)`
  > `<repo>/target/release/sekai-router(.exe)` > PATH 上的 `sekai-router`
- worker：同理 `workerBin` > `SEKAI_WORKER_BIN` > `$DSH_HOME/sekai/bin/sekai-worker(.exe)`
  > `<repo>/target/release/sekai-worker(.exe)` > PATH 上的 `sekai-worker`

第 3 项由一键安装脚本负责部署，所以安装后默认零配置。config 值与默认二进制同名时视为
「未显式配置」（此时环境变量优先）；需要覆盖时可在 profile 行级 config 写
`routerBin` / `workerBin`。

### 备选：不用安装脚本

`dsh plugin --profile <p> add <仓库目录>` 是 `link:` 安装，`cargo build` 后插件自动命中
`<repo>/target/release`（解析顺序第 4 项），通常仍零配置；只有 `file:` 拷贝安装或二进制
不在仓库内时，才需要手动放二进制/设路径：

```bash
# 构建
cargo build --release -p sekai-router -p sekai-worker
# 安装（二选一）：DSH 会话内 plugin_manager install_bundle target=<repo>/plugins/sekai-dsh
#               或 CLI：dsh plugin --profile <p> add <repo>/plugins/sekai-dsh
# 仅当二进制不在 <repo>/target/release 时（任选其一）：复制到 $DSH_HOME/sekai/bin；
# 把二进制目录加入 PATH；设 SEKAI_ROUTER_BIN / SEKAI_WORKER_BIN；行级覆盖 routerBin / workerBin。
```

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

> **agent 步数**：默认 `link:'tcp'` 即直连 **PC Worker**，Router 的 agent 默认**不设
> 步数上限**（只由完成 / 取消 / LLM 错误终止）；`link:'mqtt'`（嵌入式）仍为 12。
> 需要收敛时在 `extraArgs` 里加 `--max-steps <N>`（1..=64），或给 `prompt` 请求传
> `max_steps`（1..=32）。详见 Router README 与 `docs/sekai-pc-agent.md` §4。

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
| `autoConnect` | boolean | `true` | **自动连接开关**：DSH 启动且 `enabled` 时，是否自动 spawn worker 连接上次连接的 Router（无记录回退 `routerAddr`）；`false` 时不自动连接，改用 `sekai_worker_connect` 工具 |
| `publishControlTools` | boolean | `true` | 是否注册 `sekai_worker_connect` / `sekai_worker_disconnect` 手动控制工具 |
| `stateFile` | string | 空 | 「上次连接的 Router」状态文件路径；空 = `$DSH_HOME/sekai/worker-state.json`（`DSH_HOME` 未设置时 `~/.dsh/...`） |

### 自动连接开关与「上次连接的 Router」

```
DSH 启动 + worker 插件 enabled
 ├─ autoConnect: true  ──▶ spawn sekai-worker connect <上次连接>  (无记录 → routerAddr)
 └─ autoConnect: false ──▶ 不 spawn；等 sekai_worker_connect（省略地址同样优先「上次连接」）
                            断开用 sekai_worker_disconnect（抑制自动重启，直到再次 connect）
```

- **「上次连接的 Router」怎么来**：插件逐行观察 worker stderr 的稳定进度行——
  `sekai-worker: TCP 已连接 <addr>`（连接成功）与 `sekai-worker: 切换到新端点 <addr>`
  （jump 迁移）。识别到即按 `workerId` 隔离写入状态文件；与当前记录相同则跳过写盘。
  写盘异步、原子（`tmp` + `rename`），失败只 warn，不影响链路。
- **状态文件格式**（可手工查看/清理，删除后回退 `routerAddr`）：

  ```json
  {
    "version": 1,
    "workers": {
      "pc-worker": { "lastRouterAddr": "198.51.100.7:7421", "updatedAt": 1794112472780 }
    }
  }
  ```

- **profile 行级覆盖**（关掉自动连接，改为按需手动连）：在 profile 已有的
  `- id: sekai-worker` 行 `config` 里加一行即可（其余键沿用 bundle 默认/profile 原值）：

  ```yaml
  - id: sekai-worker
    config:
      autoConnect: false
  ```

- 手动 `disconnect` 后 worker 进程退出不再触发退避重启（状态工具里 `restarts` 不增长）；
  `sekai_worker_connect` 恢复连接并清零退避。运行中 `connect` 到另一个地址会先断旧连接再连新目标。

## 工具

模型侧描述为英文；经 router 的 4 个工具串行转发（serve 端 dispatcher 本身串行执行，
长命令会阻塞后续调用）；`sekai_worker_status` 与 connect/disconnect 是 worker 插件本地
工具，不走 serve。`sekai_worker_connect` / `sekai_worker_disconnect` 由 `autoConnect:false`
或运行期切换 Router 时使用（`publishControlTools:false` 可关闭注册）。

| 工具 | 参数 | 返回 |
| --- | --- | --- |
| `sekai_status` | 无 | `{ready, mode, linkDesc, workerCount, policy, routerPid?, version?}` |
| `sekai_list_workers` | 无 | workers 响应原样：`{count, workers[]}`（worker_id/online/boot_id/firmware/uptime_ms/tools/tasks/active_tasks…） |
| `sekai_worker_tool` | `{worker_id, tool, args, ddl_ms?}` | tool_call 响应原样（command_result 形状） |
| `sekai_worker_task` | `{worker_id, task, args, ddl_ms?}` | task 响应原样（command_result 形状） |
| `sekai_worker_status` | 无 | `{running, pid?, restarts, uptimeMs, lastStderr[], llm, autoConnect, routerAddr, lastRouterAddr?}` |
| `sekai_worker_connect` | `{router_addr?}` | `{started, running, disconnected:false, routerAddr, lastRouterAddr?}`；省略 `router_addr` = 上次连接 / `routerAddr`；运行中连另一地址会断旧连新；非法地址回 `invalid_arguments` 信封 |
| `sekai_worker_disconnect` | 无 | `{disconnected, running:false, started:false, routerAddr}`；抑制自动重启直到再次 connect |

约定（镜像 router `tools.rs` 的语义）：**先调 `sekai_list_workers`，worker id、工具名、任务名
必须来自其结果**。`args` 是原始 UTF-8 文本（如 `gpio.info help`）；`ddl_ms` ∈ [1, 300000]。
长任务（`pc.job`、`pc.ask`）请显式传 `ddl_ms`（建议 ≥60000）：不传时 Router 默认预算
只有 1s（见[缺口审计](../../docs/sekai-gap-audit.md) B4）。`sekai_worker_task` 与
`ddl_ms` 字段的模型侧描述已写明这一点（`LONG_TASK_DDL_HINT_MS`）。

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
- `link` 响应的实际地址在 `result.addr`（插件优先读取，兼容旧版 `listen_addr`/`mqtt_addr`
  字段）；`link_state` 只在 `link`/`link_off` 时由 serve 主动发。
- 为覆盖「TCP 对端断开但 serve 未补发 `link_state`」，router 插件每 3s 用 `status`
  做兜底探测：连续两次看到未就绪才把 `ready` 修正为 false（幂等：同类状态不重复告警）。

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
- **worker**：插件加载时按 `autoConnect` 决定是否立即连接（目标是上次连接的 Router，
  无记录回退 `routerAddr`）；stderr 逐行进 40 行环形缓冲并写宿主日志，同时观察
  `TCP 已连接` / `切换到新端点` 行更新「上次连接的 Router」状态文件；意外退出按 500ms 起步
  指数退避重启（封顶 `restartBackoffMaxMs`），稳定运行 ≥5s 后的退出把退避复位；手动
  `disconnect` 后的退出不再重启（直到再次 `connect`）；dispose 直接 kill（connect 模式无
  优雅协议），挂起的 prompt 中转请求回 `aborted` 并销毁连接。

## 本地冒烟（无 Rust）

```
node --check lib/index.js && node --check lib/shared.js && node --check lib/router.js \
  && node --check lib/worker.js && node --check scripts/smoke.mjs   # 语法
node scripts/smoke.mjs                                              # 冒烟
# 或：npm run check / npm run smoke
```

`scripts/smoke.mjs` 用 node 写的假 serve（按协议 v1 回放通知与响应）验证 router 侧桥接
（分帧、请求-响应、超时、本地中止、dispose、崩溃重启、autoLink 重放、四工具、错误信封、
schema 卫生），用假 worker 脚本验证 worker 侧重启与状态工具；另有假 worker 的成功连接 /
jump 切换行 + 临时状态文件验证 autoConnect 开关与「上次连接的 Router」持久化（自动连上次
地址、关闭开关不自动连接、手动 connect/disconnect 与目标切换、无效地址信封、跨插件实例
复用）；另有假 prompt worker + 假 ingress（node `net` server 回放响应）验证 dsh 通道
prompt 中转（成功回投、并发独立连接、不可达/超时错误信封、`llm:'off'` 不转发、argv 旗标、
dispose 清理）。全部断言通过 exit 0。

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
- **「上次连接的 Router」用 stderr 观察而不是改 worker 协议**：worker 的 connect 模式本来
  就用 stderr 输出阶段化进度行（`TCP 已连接 <addr>` / `切换到新端点 <addr>` 有 Rust 单测
  冻结文案），插件据此落盘即可，无需在 V1 帧/CLI 增加状态查询接口；状态文件异步原子写、
  与当前值相同则跳过写盘，链路失败时也不会因为状态文件出错而崩。
- **手动控制工具与 `autoConnect` 开关分离**：`publishControlTools` 默认开——关掉自动连接后
  模型仍能按需 `sekai_worker_connect`；不想要这两个工具（例如严格只读会话）可单独关闭，
  不影响 `sekai_worker_status`。
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
opt-in via the `gpio` config key (legacy `family` maps to it). The worker plugin's
`autoConnect` switch (default `true`) decides whether, after DSH starts with the worker
plugin enabled, it auto-connects to the **last successfully connected router**, remembered
per `workerId` in `$DSH_HOME/sekai/worker-state.json` (from the worker's stderr lines
`TCP 已连接 <addr>` and `切换到新端点 <addr>`; falls back to `routerAddr`); with
`autoConnect:false` nothing is spawned at startup and the new `sekai_worker_connect` /
`sekai_worker_disconnect` tools drive the connection on demand (a manual disconnect
suppresses backoff restarts until the next connect; the state file path is overridable
via `stateFile`). It exposes `sekai_worker_status` (including the `llm` channel,
`autoConnect`, current `routerAddr` and the remembered `lastRouterAddr`). Install
everything from the repo root with `node scripts/install-dsh-plugin.mjs` — it builds both
binaries, atomically deploys them
to `$DSH_HOME/sekai/bin`, installs the bundle via
`dsh plugin --profile <p> add file:<repo>/plugins/sekai-dsh`, and self-checks the result
(including that `resolveBin` resolves the deployed paths). Restart the DSH session
afterwards (`--dry-run` / `--uninstall` / profile auto-selection are documented in
[docs/sekai-dsh-plugin.md §1](../../docs/sekai-dsh-plugin.md)). Binaries resolve as
explicit `routerBin`/`workerBin` config > `SEKAI_ROUTER_BIN` / `SEKAI_WORKER_BIN` >
`$DSH_HOME/sekai/bin` > `<repo>/target/release` > PATH, so a default install needs no
PATH, env-var or profile edits. No build step, no runtime npm
deps; schemastery is an optional peer. All tool failures return a structured
`{error:{code,message,hint}}` envelope (`spawn_error` / `not_ready` / `timeout` /
`aborted` or the router-side protocol code) and never throw at DSH. Run
`node scripts/smoke.mjs` for the Rust-free smoke suite.
