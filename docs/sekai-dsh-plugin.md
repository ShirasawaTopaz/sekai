# Sekai × DSH 插件（plugins/sekai-dsh）

> 状态：v1 已实现并安装验证（2026-11）。把 Sekai 的 **Router** 与 **PC Worker** 分别接入
> DeepSeek Harness（DSH）插件体系：DSH 主会话取代 Router 内置 LLM 回路成为「大脑」，
> Router 退化为「链路管理 + Worker 注册表 + 派工网关」，PC Worker 由独立插件托管进程生命周期。

- **读者**：需要在本机 DSH 里驱动 Sekai Worker（GPIO 等）的使用者；插件与 serve 协议的实现者。
- **相关文档**：`docs/sekai-architecture.md`（总体架构）、`docs/sekai-protocol.md`（V1 帧）、
  `plugins/sekai-dsh/README.md`（插件配置全量说明）。

---

## 0. 形态一览

| 角色 | DSH 侧 | Rust 侧 | 通信 |
| --- | --- | --- | --- |
| Router 插件（`@sekai/dsh-plugin/router`，行 id `sekai-router`） | Host 插件：spawn 子进程、注册 4 个 `sekai_*` 工具 | `sekai-router --serve`（无头 JSONL 模式，见 §2） | stdin/stdout JSONL（协议 v1） |
| Worker 插件（`@sekai/dsh-plugin/worker`，行 id `sekai-worker`） | Host 插件：spawn 子进程、退避重启、注册 `sekai_worker_status` 工具 | `sekai-worker connect 127.0.0.1:7421` | stderr 进度行（无 stdout 杂音） |
| 两插件协作 | 只约定地址：router 插件 `listenAddr` = worker 插件 `routerAddr`（默认 `127.0.0.1:7421`） | Router 监听、Worker 自带 500ms→30s 指数退避重连 | 直连 V1 帧（长度前缀，非 MQTT） |

**LLM 职责划分**：DSH 主会话负责自然语言→工具调用的评估；Router 的
`agent.rs`/`llm.rs` 回路仅供独立 CLI 使用，不经插件暴露。安全策略
（`allow_write`/`allow_tasks`/`allow_admin`/`worker_filter`）在 serve 侧
**发布 Stdin 之前**强制执行——DSH 侧无法绕过。

## 1. 安装与配置

```bash
# 1) 构建二进制（插件不内嵌 exe，需先构建）
cargo build --release -p sekai-router -p sekai-worker

# 2) 安装 bundle（工作区绝对目录；或先调整 cordis.patch.yml 里的 routerBin/workerBin）
#    在 DSH 会话中：plugin_manager install_bundle target=D:\Source\seika\sekai\plugins\sekai-dsh
```

默认 config（`plugins/sekai-dsh/cordis.patch.yml`）已把 `routerBin`/`workerBin`
指向本仓库 `target/release` 下的 exe（开发机默认，换机器请覆盖）。行级覆盖写在
profile 的 `cordis.patch.yml`（用户层）：

```yaml
- id: sekai-router
  config:
    link: tcp            # tcp（默认，等 worker 连入）| stdio | mqtt
    listenAddr: 127.0.0.1:7421
    autoLink: true
    policy:              # 转发给 serve 侧强制执行
      allowWrite: false  # GPIO 写（gpio.config/gpio.write）
      allowTasks: false  # Task（echo/sleep 等）
      allowAdmin: false
      workerFilter: null
    requestTimeoutMs: 120000
- id: sekai-worker
  config:
    workerBin: ...       # 同理解析顺序：config > SEKAI_WORKER_BIN > PATH
    routerAddr: 127.0.0.1:7421
    workerId: pc-worker
    family: esp32        # virtual GPIO 家族
```

## 2. serve 模式与 JSONL 协议 v1（冻结）

`sekai-router --serve`：stdin 逐行读请求，stdout 只写 JSONL（人类日志全走 stderr），
不需要 `DEEPSEEK_API_KEY`，与 `--goal/--tui/-i/--dry-run` 互斥。线程模型：主线程读行
→ dispatcher 线程独占网关（串行）；poll 线程 200ms diff Worker 快照发通知。

- 请求：`{"id":N,"method":"...","params":{...}}`；响应：`{"id":N,"ok":true,"result":{...}}`
  或 `{"id":N,"ok":false,"error":{"code","message"}}`。
- 通知（无 id）：`ready` / `link_state{desc,ready}` / `worker_online{worker_id,worker}` /
  `worker_offline{worker_id}`；畸形行以 `invalid_json` 通知回错（取不到 id）。
- 方法：`link`（tcp/stdio/mqtt，tcp 的 result 含实际监听 `addr`）、`link_off`、`workers`、
  `tool_call`、`task`、`policy.get`、`policy.set`、`status`、`shutdown`（EOF 同效，exit 0）。
- 命令结果形状 = `tools.rs::command_result_json`（`ok/worker_id/kind/name/cmd_id/elapsed_ms/
  payload/encoding/truncated?/phases` 或 `error{code,worker_code,...,retryable}`）；
  **策略拒绝与网关失败是信封 `ok:true` + `result.ok:false`**（与 LLM function result 同形），
  信封级错误码：`invalid_arguments`/`unknown_method`/`link_error`。
- 实现位于 `crate/sekai-router/src/serve.rs`；网关装配共享层 `src/link_builder.rs`
  （REPL/serve/批处理三方共用）；测试 `tests/serve.rs`（11 例）。

## 3. DSH 工具面

| 工具 | 形参 | 返回 |
| --- | --- | --- |
| `sekai_status` | — | `{ready, mode, linkDesc, workerCount, policy, routerPid?, version?}` |
| `sekai_list_workers` | — | serve `workers` 响应原样（`{count, workers[]}`，含 tools/tasks/boot_id） |
| `sekai_worker_tool` | `worker_id, tool, args, ddl_ms?` | serve `tool_call` 结果原样（结构化 error：`policy_denied` 等） |
| `sekai_worker_task` | `worker_id, task, args, ddl_ms?` | serve `task` 结果原样 |
| `sekai_worker_status` | — | worker 插件本地状态：`{running, pid?, restarts, uptimeMs, lastStderr[]}` |

工具描述要求先调 `sekai_list_workers`；调用在 serve dispatcher 串行排队；
`exec.signal` 中止仅本地返回（router 侧命令照跑到终态）。

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
- 暴露 Router 自带 LLM agent 回路给 DSH（刻意不做，避免双重大脑）。
