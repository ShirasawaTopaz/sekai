# Sekai × DSH 插件（plugins/sekai-dsh）

> 状态：v1 已实现并安装验证（2026-11）；v1.1 追加 serve `prompt` 方法与 worker 插件
> 的 prompt 中转（PC agent 形态）；v1.3 追加 worker 插件 `autoConnect` 自动连接开关
> （恢复上次连接的 Router，状态文件按 `workerId` 持久化）与 `sekai_worker_connect` /
> `sekai_worker_disconnect` 手动控制工具。把 Sekai 的 **Router** 与 **PC Worker** 分别接入
> DeepSeek Harness（DSH）插件体系：DSH 主会话取代 Router 内置 LLM 回路成为「大脑」，
> Router 退化为「链路管理 + Worker 注册表 + 派工网关」，PC Worker 由独立插件托管进程
> 生命周期（并可中转其 `pc.ask` prompt）。

- **读者**：需要在本机 DSH 里驱动 Sekai Worker（`pc.*` 命令/文件能力、可选 GPIO）的
  使用者；插件与 serve 协议的实现者。
- **相关文档**：`docs/sekai-architecture.md`（总体架构）、`docs/sekai-pc-agent.md`
  （PC agent 与 prompt 通道全参考）、`docs/sekai-protocol.md`（V1 帧）、
  `plugins/sekai-dsh/README.md`（插件配置全量说明）。

---

## 0. 形态一览

| 角色 | DSH 侧 | Rust 侧 | 通信 |
| --- | --- | --- | --- |
| Router 插件（`@sekai/dsh-plugin/router`，行 id `sekai-router`） | Host 插件：spawn 子进程、注册 4 个 `sekai_*` 工具 | `sekai-router --serve`（无头 JSONL 模式，见 §2） | stdin/stdout JSONL（协议 v1，v1.1 追加 `prompt`） |
| Worker 插件（`@sekai/dsh-plugin/worker`，行 id `sekai-worker`） | Host 插件：spawn 子进程、退避重启、注册 `sekai_worker_status` 工具、（`llm:'dsh'` 时）中转 prompt | `sekai-worker connect 127.0.0.1:7421 --llm dsh`（PC agent） | stderr 进度行 + stdin/stdout prompt JSONL（中转通道） |
| 两插件协作 | 只约定地址：router 插件 `listenAddr` = worker 插件 `routerAddr`（默认 `127.0.0.1:7421`） | Router 监听、Worker 自带 500ms→30s 指数退避重连 | 直连 V1 帧（长度前缀，非 MQTT） |

**LLM 职责划分**：DSH 主会话负责自然语言→工具调用的评估；Router 的
`agent.rs`/`llm.rs` 回路仅供独立 CLI 与 prompt 通道（`pc.ask` / serve `prompt` /
ingress）使用。安全策略（`allow_write`/`allow_tasks`/`allow_admin`/`worker_filter`）
在 serve 侧**发布 Stdin 之前**强制执行——DSH 侧无法绕过。

## 1. 安装与配置

### 一键安装（推荐）

在仓库根目录、**普通终端**执行一条命令（可选 `--profile <name>` 指定 DSH profile）：

```bash
node scripts/install-dsh-plugin.mjs
```

脚本做四件事：

1. **构建**：`cargo build --release -p sekai-router -p sekai-worker`
   （`--no-build` 跳过，改用 `target/release` 现有产物；`--bin-dir <dir>` 改用该目录里
   已构建的两个二进制，隐含不构建）。
2. **部署二进制**：把两个二进制原子复制（临时文件 + rename）到 `<DSH_HOME>/sekai/bin/`
   （`DSH_HOME` 未设置时 = `~/.dsh`）。插件从这里**零配置**发现，不需要改 PATH、
   环境变量或 profile YAML。
3. **安装插件**：`dsh plugin --profile <p> add file:<repo>/plugins/sekai-dsh`
   （`file:` = pnpm 真实拷贝，peer 依赖可从 profile 解析；DSH 会自动把包名追加进
   profile `package.json` 的 `dsh.profile.bundles`，重复执行幂等）。所有 `dsh`
   子进程都注入 `DSH_HOME`。
4. **自检**：部署文件存在且非空；profile 的 `dependencies` 与 `dsh.profile.bundles`
   已登记；从 profile 解析并 import 已安装插件、调用其 `resolveBin`，**必须命中刚部署
   的两个绝对路径**（零配置闭环）；`dsh --profile <p> --dump-config` 输出含
   `sekai-router` / `sekai-worker` 两行（默认只告警，`--strict` 时失败）。

装完**重启 DSH 会话**（重启桌面端或重新运行 `dsh web`）——运行中的会话不会热加载
bundle；重启后会话里出现 `sekai_*` 工具。默认 router 监听 `127.0.0.1:7421`，
worker `autoConnect` 自动连接。

| 选项 | 作用 |
| --- | --- |
| `--profile <name>` / `-p` | 指定 profile（默认规则见下） |
| `--dsh-home <dir>` | 指定 `DSH_HOME`（默认 `$DSH_HOME` > `~/.dsh`） |
| `--dsh <path>` | 指定 dsh CLI（默认 PATH；Windows 回落桌面端内置 `dsh.cmd`） |
| `--no-build` | 跳过 cargo build，使用 `target/release` 现有二进制 |
| `--bin-dir <dir>` | 用该目录里已构建的两个二进制（隐含 `--no-build`） |
| `--dry-run` | 只看计划：打印将执行的命令与写入位置，不构建、不写盘、不调用 dsh |
| `--uninstall` | 卸载插件（删 profile 依赖与 bundles 条目；保留 `<DSH_HOME>/sekai/bin` 与 worker 状态文件） |
| `--strict` | `--dump-config` 组合校验失败也视为失败（默认只告警） |

**profile 自动选择**：`--profile` > `$DSH_PROFILE` > `profiles/` 下唯一的 profile >
存在的 `desktop`（DSH 桌面端默认）> 报错并列出可用 profile；profile 必须已存在
（先启动一次 DSH 桌面端 / `dsh web` 创建）。`--dsh-home` 用于指定非默认 DSH home
（测试/多实例等）——注意 `$DSH_PROFILE` 的优先级高于「唯一 profile / desktop」，
用非默认 home 时建议同时显式传 `--profile`；`--dsh` 用于指定 dsh CLI 路径。
`--help` 列出全部参数。

> **运行环境提示**：脚本要调用 `cargo` / `dsh` 子进程并写 `<DSH_HOME>/sekai`，请在
> 普通系统终端运行。在 DSH 受限沙箱会话（workspace-write）里 Node 子进程会被拒绝
> （`spawn EPERM`）；若确实要在 DSH 会话里执行，需要给该命令申请更宽的沙箱权限
> （会放弃该次命令的会话隔离），否则请到终端跑。升级插件 = `git pull` 后重跑同一条
> 命令；重启 DSH 才会加载新 bundle（运行中的会话不热加载）。

### 二进制解析顺序（零配置）

两个插件一致（`plugins/sekai-dsh/lib/shared.js::resolveBin`）：

| 优先级 | 来源 | 说明 |
| --- | --- | --- |
| 1 | 显式 config：`routerBin` / `workerBin` | 非空且不等于默认名时生效；最高优先级，用于覆盖 |
| 2 | 环境变量：`SEKAI_ROUTER_BIN` / `SEKAI_WORKER_BIN` | 非空即生效 |
| 3 | `$DSH_HOME/sekai/bin/<name>(.exe)` | **一键脚本的部署位置**，默认零配置走这里 |
| 4 | `<repo>/target/release/<name>(.exe)` | 从仓库 checkout 加载插件时本地开发免配置 |
| 5 | PATH 上的默认名 | 最后回退（`sekai-router` / `sekai-worker`） |

即：**config > 环境变量 > `$DSH_HOME/sekai/bin` > `<repo>/target/release` > PATH**。
第 3 项由一键安装脚本负责写入，所以装完默认零配置；第 4 项让 `link:` 安装的源码
checkout 场景不做任何配置也能直接用 `cargo build` 产物。

### 备选：不用安装脚本

功能等价，只是二进制来源不同：`dsh plugin --profile <p> add <仓库目录>` 是 `link:`
安装（模块 realpath 指回仓库），`cargo build` 后插件会**自动命中**
`<repo>/target/release`（解析顺序第 4 项），通常仍然零配置；只有用 `file:` 拷贝安装、
或二进制不在仓库内时，才需要手动放二进制/设路径：

```bash
# 1) 构建
cargo build --release -p sekai-router -p sekai-worker

# 2) 安装 bundle（二选一）
#    DSH 会话内：plugin_manager install_bundle target=<repo>/plugins/sekai-dsh
#    或 CLI：dsh plugin --profile <p> add <repo>/plugins/sekai-dsh

# 3) 仅当二进制不在 <repo>/target/release 时（任选其一）
#    - 复制到 $DSH_HOME/sekai/bin/（与一键脚本的部署位置相同）；
#    - 把二进制目录加入 PATH；
#    - 设 SEKAI_ROUTER_BIN / SEKAI_WORKER_BIN 为绝对路径；
#    - 在 profile 行级覆盖里写 routerBin / workerBin。
```

### 高级：行级覆盖（config）

默认 config（`plugins/sekai-dsh/cordis.patch.yml`）不写死二进制路径。需要改监听地址/
策略/自动连接等时，在 profile 的 `cordis.patch.yml`（用户层）按行覆盖（未写的键沿用
bundle 默认值）：

```yaml
- id: sekai-router
  config:
    link: tcp            # tcp（默认，等 worker 连入）| stdio | mqtt
    listenAddr: 127.0.0.1:7421
    autoLink: true
    policy:              # 转发给 serve 侧强制执行
      allowWrite: false  # pc.exec/pc.write 与 GPIO 写（gpio.config/gpio.write）
      allowTasks: false  # Task（pc.job/pc.ask/echo/sleep 等）
      allowAdmin: false
      workerFilter: null
    requestTimeoutMs: 120000
    # routerBin: D:\path\to\sekai-router.exe   # 可选：解析顺序 config > SEKAI_ROUTER_BIN > $DSH_HOME/sekai/bin > <repo>/target/release > PATH
- id: sekai-worker
  config:
    # workerBin: D:\path\to\sekai-worker.exe   # 同上；默认由一键脚本部署后自动发现
    routerAddr: 127.0.0.1:7421
    workerId: pc-worker
    llm: dsh             # dsh（默认，插件中转 pc.ask）| router（worker 直连 ingress）| off
    promptAddr: 127.0.0.1:7422   # router 模式作 --prompt-addr；dsh 模式为中转目标
    promptTimeoutMs: 300000      # 中转等待响应的本地超时（≥1000）
    gpio: null           # 虚拟 GPIO 家族 esp32|stm32f1|stm32f4；null=关闭（默认；
                         # 旧键 family 兼容映射到本键）
    autoConnect: true    # 自动连接开关：DSH 启动且 enabled 时是否自动连上次连接的
                         # Router（无记录回退 routerAddr）；false 时用
                         # sekai_worker_connect / sekai_worker_disconnect 手动控制
    publishControlTools: true    # 是否发布上述两个手动控制工具
    stateFile: ''        # 「上次连接的 Router」状态文件；空 = $DSH_HOME/sekai/worker-state.json
```

> **自动连接与「上次连接的 Router」**：worker 插件在 `autoConnect:true`（默认）时于 DSH
> 启动后自动 `connect` **上次成功连接的 Router**——地址来自 worker stderr 的
> `sekai-worker: TCP 已连接 <addr>` 与 `sekai-worker: 切换到新端点 <addr>`（jump 迁移）
> 进度行，按 `workerId` 持久化到状态文件；没有记录时回退 `routerAddr`。`autoConnect:false`
> 时启动不 spawn，改由 `sekai_worker_connect`（省略地址同样优先上次连接）/
> `sekai_worker_disconnect` 控制；手动断开后不再自动退避重启，直到再次 connect。

## 2. serve 模式与 JSONL 协议 v1（冻结；v1.1 `prompt`、v1.2 异步/取消/管理）

`sekai-router --serve`：stdin 逐行读请求，stdout 只写 JSONL（人类日志全走 stderr），
链路操作不需要 `DEEPSEEK_API_KEY`，与 `--goal/--tui/-i/--dry-run` 互斥。线程模型：主线程读行
→ dispatcher 线程独占网关（只解析与分发，**不阻塞在长命令/LLM 调用上**；长方法拆到工作
线程，结果经写队列按 id 回写，JSONL 行完整、允许乱序）；poll 线程 200ms diff Worker 快照
发通知，并在链路 ready 由 true→false / false→true 时补发 `link_state`。

- 请求：`{"id":N,"method":"...","params":{...}}`；响应：`{"id":N,"ok":true,"result":{...}}`
  或 `{"id":N,"ok":false,"error":{"code","message"}}`。
- 通知（无 id）：`ready` / `link_state{desc,ready}` / `worker_online{worker_id,worker}` /
  `worker_offline{worker_id}`；畸形行以 `invalid_json` 通知回错（取不到 id）。
- 方法：`link`（tcp/stdio/mqtt，tcp 的 result 含实际监听 `addr`、mqtt 含 `mqtt_addr`）、
  `link_off`、`workers`、`tool_call`、`task`、`policy.get`、`policy.set`、`status`、
  `shutdown`（EOF 同效，exit 0）；
  **v1.1 追加（additive）**：`prompt{text,max_steps?}`——与 TCP prompt ingress 共用同一
  核心，把 prompt 交给 Router 内置 LLM 回路生成回答（需 API key；响应
  `result:{answer,status,steps,usage}`；错误码与防递归见
  [sekai-pc-agent.md](sekai-pc-agent.md) §4–§5）。未传 `max_steps` 时随 Router 全局：
  **直连 PC Worker（默认 `link:'tcp'`）不设上限**，MQTT/嵌入式仍为 12。
  **v1.2 追加（additive）**：
  - `tool_call` / `task` 支持 `async:true`：立即回
    `result:{accepted:true,cmd_id,worker_id,kind,name,async:true}`，完成后发无 id 通知
    `{"method":"command_result","params":{...同一 command_result_json...}}`；
  - `cancel {cmd_id}` → 转发 `sekai.cancel`，回 `{cmd_id,cancelled,known}`；
  - `commands` → `{count,commands[]}`（在途账本）；
  - `jump {worker_id,target}`（需 allow_admin）；
  - `admin {op:set_time|cancel|workers.set,...}`（需 allow_admin；set_time 为 8 字节大端）；
  - `status` 追加 `link_info{state,workers,last_error}`、`inflight[]`、
    `counters{late_results,dropped_events}`、`clock_synced`、`late_results[]`。
- Router 另以 `--prompt-addr <HOST:PORT|off>`（env `SEKAI_PROMPT_ADDR`；serve/交互默认
  `127.0.0.1:7422`，批处理默认 `off`）开 **TCP prompt ingress**，协议与 serve
  `prompt` 一致（另有 `ping` 探活）；默认仅回环，远程需
  `SEKAI_INGRESS_ALLOW_REMOTE=1` + `SEKAI_INGRESS_TOKEN`（见
  [sekai-pc-agent.md](sekai-pc-agent.md) §4.1）。
- 命令结果形状 = `tools.rs::command_result_json`（`ok/worker_id/kind/name/cmd_id/elapsed_ms/
  payload/encoding/truncated?/phases` 或 `error{code,worker_code,...,retryable}`）；
  **策略拒绝与网关失败是信封 `ok:true` + `result.ok:false`**（与 LLM function result 同形），
  信封级错误码：`invalid_arguments`/`unknown_method`/`link_error`。
- 实现位于 `crate/sekai-router/src/serve.rs` 与 `prompt_server.rs`（ingress）；网关装配
  共享层 `src/link_builder.rs`（REPL/serve/批处理三方共用）；测试 `tests/serve.rs`。

## 3. DSH 工具面

| 工具 | 形参 | 返回 |
| --- | --- | --- |
| `sekai_status` | — | `{ready, mode, linkDesc, workerCount, policy, routerPid?, version?}` |
| `sekai_list_workers` | — | serve `workers` 响应原样（`{count, workers[]}`，含 tools/tasks/boot_id） |
| `sekai_worker_tool` | `worker_id, tool, args, ddl_ms?` | serve `tool_call` 结果原样（结构化 error：`policy_denied` 等） |
| `sekai_worker_task` | `worker_id, task, args, ddl_ms?` | serve `task` 结果原样 |
| `sekai_worker_status` | — | worker 插件本地状态：`{running, pid?, restarts, uptimeMs, lastStderr[], llm, autoConnect, routerAddr, lastRouterAddr?}` |
| `sekai_worker_connect` | `router_addr?` | 启动/切换本机 worker 连接：`{started, running, disconnected:false, routerAddr, lastRouterAddr?}`；省略地址 = 上次连接 / `routerAddr` |
| `sekai_worker_disconnect` | — | 断开并停止本机 worker：`{disconnected, running:false}`；手动断开后抑制自动重启直到再次 connect |

工具描述要求先调 `sekai_list_workers`；调用在 serve dispatcher 串行排队；
`exec.signal` 中止仅本地返回（router 侧命令照跑到终态）。`sekai_worker_status` /
`sekai_worker_connect` / `sekai_worker_disconnect` 是 worker 插件本地工具，不走 serve。

## 3.1 worker 插件的 prompt 中转（`llm:'dsh'`）

Worker 以 `--llm dsh` 启动时，其 `pc.ask` prompt 由 worker 插件中转：

```text
worker stdout ──▶ 插件识别 {"id":N,"method":"prompt",params} 行
                     │ net.connect(promptAddr)（每条 prompt 独立连接）
                     ▼
              Router prompt ingress（127.0.0.1:7422）
                     │ 响应行（按 id 匹配；其余行丢弃告警）
                     ▼
worker stdin ◀── 原样回投（ok/result 或 ok/error 信封）
```

- 非请求行（含非 prompt 的 JSON）维持现状：记宿主日志。
- 失败回投错误信封：ingress 连不上 → `unreachable`；等待超时（`promptTimeoutMs`，
  默认 300000ms）→ `timeout`；插件 dispose 时挂起请求 → `aborted`（本地码）。
- worker 进程退出时挂起请求只断开不回投；ingress 侧错误码
  （`llm_unavailable`/`not_linked`/`no_workers`/`invalid_arguments`/`agent_error` 等）
  原样透传。完整协议见 [sekai-pc-agent.md](sekai-pc-agent.md) §4。

## 3.2 自动连接开关与「上次连接的 Router」

```
DSH 启动 + worker 插件 enabled
 ├─ autoConnect: true  ──▶ spawn sekai-worker connect <上次连接地址>
 │                          （状态文件无记录 → config routerAddr）
 └─ autoConnect: false ──▶ 不 spawn；等 sekai_worker_connect 手动连接
                            （省略 router_addr 同样优先「上次连接」）
```

- **开关**：config 键 `autoConnect`（boolean，默认 `true`）。它只控制「DSH 启动 + worker
  启用」时的自动连接行为，不影响 `enabled`；关掉后插件生命周期与
  `sekai_worker_status` 仍正常，连接由 `sekai_worker_connect` 发起、
  `sekai_worker_disconnect` 断开。
- **「上次连接」来源**：worker connect 模式的 stderr 阶段化进度行
  （`crate/sekai-worker/src/bin/sekai-worker.rs` 的 `connected_line` /
  `切换到新端点`）——插件逐行识别 `sekai-worker: TCP 已连接 <addr>` 与
  `sekai-worker: 切换到新端点 <addr>`，按 `workerId` 写入状态文件
  （`$DSH_HOME/sekai/worker-state.json`，可被 config `stateFile` 覆盖）：

  ```json
  {
    "version": 1,
    "workers": {
      "pc-win": { "lastRouterAddr": "127.0.0.1:7421", "updatedAt": 1794112472780 }
    }
  }
  ```

  与当前记录相同时跳过写盘；写盘异步原子（tmp + rename）、失败只 warn，不影响链路。
  删除该文件即回到「首次运行」：自动连接使用 `routerAddr`。
- **手动断开语义**：`disconnect` 先摘除子进程引用并 kill，之后 worker 进程退出事件不再计
  `restarts`、不再排退避定时器；再次 `connect` 恢复并复位退避。运行中 `connect` 到另一个
  地址会先断开旧连接再连新目标（`routerAddr` 随之更新，状态工具可见）。
- **错误面**：`sekai_worker_connect` 的 `router_addr` 非法 → `invalid_arguments` 信封；
  插件已 dispose → `not_running` 信封；其余成功值见 §3 工具表。

## 4. 关键实现事实（排障必读）

- **Cordis 类插件必须默认导出**：`export default SekaiRouterPlugin`（对齐 dsh-dcp）。
  仅命名导出会报误导性的「failed to import」（fiber 从未创建，导入本身成功）。
- **未经 `static inject` 声明的服务只能用 `ctx.get('tools')` 访问**；`ctx.tools`
  直接访问会抛 "cannot get property without inject"（worker 插件曾因此崩溃，已改防御式）。
- 工具 `output.schema` 有意放宽（透传协议响应，容忍 serve v1 加字段）；输入
  `parameters` 保持严格（`additionalProperties:false`、禁 type 数组）。
- schemastery 以 `createRequire` 守卫式加载：宿主内正常生效，无 node_modules
  环境回退内置默认值（仓库冒烟可直跑）。
- 插件崩溃退避重启后自动重放 link/policy；dispose 顺序 shutdown→宽限→kill。
- **worker 子进程 stdin 现在是管道**（`['pipe','pipe','pipe']`）：承载 prompt 响应
  回投通道；非 dsh 通道时闲置不用。
- **「上次连接的 Router」是插件侧状态，不在 worker/Router 协议里**：靠 stderr 稳定进度行
  （Rust 单测冻结文案）识别连接成功 / jump 新端点，持久化到 `$DSH_HOME/sekai/worker-state.json`
  （按 workerId 隔离）；状态文件损坏/缺失/非法地址一律回退 `routerAddr`，不阻塞启动。
- **手动 disconnect 与崩溃重启是两条路径**：`disconnectWorker` 先把 `child` 置空再 kill，
  迟到 exit 事件被当作旧进程忽略、不计 `restarts`；`stopped`（dispose）与 `manualStopped`
  分别抑制「插件生命周期结束」与「手动断开」两类重启。
- **readline 会把输入流错误转发成 Interface 自身的 `error` 事件**（Node 新行为）：
  中转 socket 上的 readline 必须挂 `error` 监听，否则 ingress 不可达时未处理错误
  事件会把宿主进程抛崩。
- serve `link_state.ready` = 链路建立（tcp 已监听），Worker 上线以 `worker_online` 为准。

## 5. 端到端验收记录（本机，2026-11-04）

- Rust 层（`.build/lead-serve-smoke.mjs`，真实双进程）：ready/link/link_state →
  worker_online（能力快照 10 工具）→ workers → sekai.info → 默认策略拒 gpio.config
  （`policy_denied`）→ `policy.set allow_write` → config/write/info 显示 `level=1` →
  status → shutdown exit 0。**14/14 断言通过**。
- 插件层（安装后）：`set_bundle` 应用结果 `applied`、无告警；router 监听 7421、
  worker 建立连接；DSH 子会话可见 5 个 `sekai_*` 工具并完成只读 + 拒写路径
  （详见 §验收清单）；kill worker 后插件按退避自动重启。
- 工作区测试：`cargo test --workspace` 全绿；bundle 冒烟 `node scripts/smoke.mjs` 14/14。

## 6. 非目标与后续

- Web UI 状态面板（Client Slots）——下轮。
- bundle 内嵌预编译二进制 / npm 发布；嵌入式 Worker（MQTT 链路仅保留 `link mqtt` 通道）。
- ~~暴露 Router 自带 LLM agent 回路给 DSH（刻意不做，避免双重大脑）~~ → v1.1 起以
  **受限问答形态**暴露：serve `prompt` 方法 / ingress 只返回单轮回答（带防递归闸），
  DSH 主会话仍是唯一的控制大脑。
- 已核实的插件/serve 缺口（link 地址、断开通知、长任务 DDL 提示等）见
  [sekai-gap-audit.md](sekai-gap-audit.md) B11。
