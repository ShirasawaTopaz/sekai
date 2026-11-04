# @sekai/dsh-plugin

面向 [DSH（DeepSeek Harness）](https://developer.deepseek.com)的 Sekai 双插件 bundle：把
`sekai-router` 与 `sekai-worker` 两个 Rust 二进制接入 DSH Host，让模型直接操作 Sekai 世界里的
Worker（ESP32 等设备的 GPIO/任务面）。

- **router 插件**（`@sekai/dsh-plugin/router`，组件 id `sekai-router`）：spawn
  `sekai-router --serve`（无头 JSONL over stdio 模式），并向 DSH 注册 4 个 `sekai_*` 工具。
- **worker 插件**（`@sekai/dsh-plugin/worker`，组件 id `sekai-worker`）：spawn
  `sekai-worker connect <addr>` 管理本机 Worker 的生命周期，注册 `sekai_worker_status` 状态工具。

纯 ESM JavaScript，无构建步骤、无运行时 npm 依赖（只用 `node:child_process` / `node:readline`
等内置模块）；`@deepseek-ai/cordis` 与 `@deepseek-ai/schemastery` 为 optional peer 依赖。

```
DSH Host
 ├─ sekai-router 插件 ──spawn── sekai-router --serve --link tcp   (JSONL over stdio)
 │                          ├─ 请求: {"id":n,"method":"tool_call","params":{...}}
 │                          └─ 通知: worker_online / link_state / ready
 └─ sekai-worker 插件 ──spawn── sekai-worker connect 127.0.0.1:7421 --worker-id pc-worker
```

## 安装

由 DSH 管理者执行（本仓库不代办）：

```
# dsh 插件管理器（本地绝对目录安装）
dsh plugin add D:\Source\seika\sekai\plugins\sekai-dsh
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
| `family` | string | `'esp32'` | `--family` |
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
| `sekai_worker_status` | 无 | `{running, pid?, restarts, uptimeMs, lastStderr[]}` |

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
  `policy.set{allow_write?,allow_tasks?,allow_admin?,worker_filter?}`、`status`、`shutdown`
- 客户端对无法匹配 id、也无 event 的行丢弃并 `logger.warn`；人类日志走 stderr。

## 生命周期与容错

- **router**：ready 通知 → autoLink（`link` + `policy.set`）；进程意外退出按 500ms→30s
  指数退避重启并在 ready 后重放 link/policy、退避复位；worker 计数由
  `worker_online`/`worker_offline` 通知与 `workers` 响应维护，进程死亡时清零重建。
  dispose 顺序：`shutdown` 请求（短超时）→ 宽限等待 → kill → 强杀。
- **worker**：stderr 逐行进 40 行环形缓冲并写宿主日志；意外退出按 500ms 起步指数退避重启
  （封顶 `restartBackoffMaxMs`），稳定运行 ≥5s 后的退出把退避复位；dispose 直接 kill
  （connect 模式无优雅协议）。

## 本地冒烟（无 Rust）

```
node --check lib/index.js && node --check lib/shared.js && node --check lib/router.js \
  && node --check lib/worker.js && node --check scripts/smoke.mjs   # 语法
node scripts/smoke.mjs                                              # 冒烟
# 或：npm run check / npm run smoke
```

`scripts/smoke.mjs` 用 node 写的假 serve（按协议 v1 回放通知与响应）验证 router 侧桥接
（分帧、请求-响应、超时、本地中止、dispose、崩溃重启、autoLink 重放、四工具、错误信封、
schema 卫生），用假 worker 脚本验证 worker 侧重启与状态工具；全部断言通过 exit 0。

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
- **worker 不走 JsonlProcess**：connect 模式没有 JSONL 协议（stdout 静默、stderr 是人类进度
  行），只需要 spawn + stderr 环形缓冲 + 退避重启，复用 shared 的 `RingBuffer`/`nextBackoffMs`。
- **worker 插件不 `static inject ['tools']`**：宿主没有 tools 服务时也能加载（只跑生命周期），
  状态工具在注册表可用时才发布，缺 registry 只 warn。
- **插件类不用 `#private`**：Cordis 会把插件方法派发到派生接收者上执行，`#private` 品牌检查
  在这种接收者上会误抛（dsh-dcp 的符号键教训）；统一用普通属性 + 闭包。

## English quick reference

Two host plugins bridging the Sekai Rust binaries into DSH: the **router** plugin spawns
`sekai-router --serve` (frozen JSONL-over-stdio protocol v1) and registers
`sekai_status` / `sekai_list_workers` / `sekai_worker_tool` / `sekai_worker_task`
(serially forwarded; aborting a call only stops waiting locally — the command keeps
running); the **worker** plugin manages `sekai-worker connect <addr>` with a 40-line
stderr ring buffer and 500ms→cap exponential backoff restarts, exposing
`sekai_worker_status`. Binaries resolve as config path > `SEKAI_ROUTER_BIN` /
`SEKAI_WORKER_BIN` > PATH; build them with `cargo build --release -p sekai-router
-p sekai-worker`. No build step, no runtime npm deps; schemastery is an optional peer.
All tool failures return a structured `{error:{code,message,hint}}` envelope
(`spawn_error` / `not_ready` / `timeout` / `aborted` or the router-side protocol code)
and never throw at DSH. Run `node scripts/smoke.mjs` for the Rust-free smoke suite.
