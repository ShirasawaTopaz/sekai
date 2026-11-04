# sekai-worker

Sekai **PC Worker**：宿主进程形态的 Worker，当前阶段的主力——以 **PC agent** 形态工作：
开箱执行本机命令与文件操作（`pc.*` 工具/任务），通过**直连链路**（stdio 子进程或 TCP，
长度前缀 + V1 帧，**不经 MQTT broker**）与 Router 通信。虚拟 GPIO 降为 `--gpio` opt-in
（嵌入式演示用，见 [docs/sekai-gpio.md](../../docs/sekai-gpio.md)）。

```text
sekai-worker connect 127.0.0.1:7421 --llm dsh
 ├─ 链路（connect/tcp 或 stdio 子进程）：V1 帧，与 Router 的命令面
 ├─ 工具（Tool）：pc.read / pc.write / pc.ls / pc.info（+ 内置 sekai.*）
 ├─ 任务（Task）：pc.exec / pc.job / pc.ask / echo / sleep
 ├─ prompt 通道（--llm，带外 JSONL，不影响 V1 帧）：
 │   ├─ router：Worker 直连 Router prompt ingress（TCP，--prompt-addr）
 │   └─ dsh  ：Worker stdin/stdout JSONL → DSH worker 插件中转 → ingress
 └─ 可选：--gpio esp32|stm32f1|stm32f4 追加 gpio.*（虚拟 GPIO）
```

## 用法

```text
sekai-worker connect <HOST:PORT> [选项]   连接 Router 直连接入端点（tcp 客户端）
sekai-worker [stdio] [选项]               子进程模式（stdin/stdout 即链路；默认）
sekai-worker help                         显示帮助

选项：
  --worker-id ID          Worker id（默认 pc-worker）
  --llm router|dsh|off    prompt 通道：router=直连 Router ingress；dsh=经安装它的
                          DSH 插件中转；off=关闭。默认：connect 模式 router、
                          stdio 模式 off（stdio 的 stdin/stdout 是链路本身，
                          router/dsh 需 connect 模式）
  --prompt-addr HOST:PORT router 模式的 ingress 目标（默认 connect 主机 + 7422；
                          仅 --llm router 时有效）
  --gpio esp32|stm32f1|stm32f4
                          opt-in 虚拟 GPIO 引脚表家族（--family 为兼容别名；
                          默认不挂 GPIO）
  --boot-id N             启动周期 id（默认按时间+pid 生成，跨重启必变）

环境变量：
  SEKAI_PC_EXEC_MS        pc.exec 内部超时（100..=300000，默认 10000）
  SEKAI_PC_ASK_MS         pc.ask 内部硬超时（1000..=300000，默认 300000）
```

兼容旧旗标 `--mode stdio|tcp`、`--router-addr HOST:PORT`（等价子命令）、`--family`
（等价 `--gpio`）。

## pc.* 能力速览

args 与输出都是原始 UTF-8 文本；输出统一截断（`[[truncated]]` / `[[timeout after Nms]]`
标记）。完整参考（含输出形状与错误映射）见
[docs/sekai-pc-agent.md](../../docs/sekai-pc-agent.md)。

| 名称 | 类别 | args | 结果 |
| --- | --- | --- | --- |
| `pc.exec` | 任务 | 命令文本（Windows `cmd /C`，其余 `sh -c`） | `exit=<code>` + 合并输出；≤4096B；默认 10s 内部超时。v0.2 起为 Task（不阻塞帧循环），策略仍按写操作需要 `--allow-write` |
| `pc.read` | 工具 | 首行路径 | 文件内容（≤4000B） |
| `pc.write` | 工具 | 首行路径 + 其余全部为内容 | `written=<字节数>`（覆盖写） |
| `pc.ls` | 工具 | 路径 | `d <name>` / `f <name>` 行，≤200 行 |
| `pc.info` | 工具 | 无 | `os=/host=/cwd=/pid=/llm=/gpio=` 多行 |
| `pc.job` | 任务 | 命令文本 | `exit=<code>` + 输出尾部 ≤4096B；DDL 到期 Failed(Timeout) |
| `pc.ask` | 任务 | prompt 文本 | 经 prompt 通道取 LLM 回答（≤4096B；内部 300s 上限） |
| `echo` / `sleep` | 任务 | 原样 / 毫秒数 | 原样返回 / 到期空载荷 |

## prompt 通道两条路径

`pc.ask` 把 prompt 交给 LLM 取回回答，JSONL 信封三方通用
（请求 `{"id":N,"method":"prompt","params":{"text":"…"}}`，响应 `ok/result` 或
`ok/error{code,message}`；完整协议见
[docs/sekai-pc-agent.md](../../docs/sekai-pc-agent.md) §4）：

```text
--llm router（connect 模式默认）：
  pc.ask → Worker ──TCP JSONL──▶ sekai-router prompt ingress（--prompt-addr）
          ◀────────────────────── 回答

--llm dsh（DSH 插件托管时）：
  pc.ask → Worker ──stdout JSONL──▶ DSH worker 插件（中转）
          ◀──stdin JSONL──────────┘        │
                                           │ TCP JSONL
                                           ▼
                              sekai-router prompt ingress
```

通道失败以错误码回投（`unreachable` / `timeout` / `llm_unavailable` 等）；
通道未启用时 `pc.ask` 直接 `Failed(InvalidArgument)`（消息含 `--llm` 提示）。

## 典型闭环

```bash
# 终端 1                                # 终端 2
sekai-router                            sekai-worker connect 127.0.0.1:7421
sekai> /link tcp                        # stderr 阶段化进度：
sekai-router: 已监听 127.0.0.1:7421     #   TCP 已连接 → Hello 已发布
sekai-router: Worker 已上线：pc-worker
sekai> 列一下当前目录有哪些文件          # → Router 派 pc.ls（只读，默认放行）
```

- `connect` 的连接失败输出**可操作提示**（如「Router 可能未启动，先在另一终端
  运行 sekai-router 并执行 /link tcp…」）；断线按指数退避重连（500ms 起步、
  30s 封顶），重连后重新发布 Hello 并按 `(boot_id, seq)` 重放未确认事件。
- `stdio` 模式供 Router 以 `/link stdio` 直接拉起：stdin 关闭（EOF）即正常退出。
  无参数默认进入本模式。

## 安全注意

- **`pc.exec` / `pc.job` 在 Worker 本机执行任意 shell 命令**，权限面最大：Router 侧
  默认拒绝（`pc.exec` 属写白名单需 `--allow-write`；`pc.job`/`pc.ask`/`echo`/`sleep`
  走 Task 通道需 `--allow-tasks`），放开前确认运行环境可接受任意命令执行，并配合
  `--worker <ID>` 收窄范围。
- `pc.write` 同样需 `--allow-write`；`pc.read`/`pc.ls`/`pc.info` 只读白名单默认放行。
- 经 prompt 通道（ingress / serve `prompt`）发起的 LLM 运行内**禁止再派 `pc.ask`**
  （Router 侧防递归硬闸，`policy_denied`「嵌套 prompt（pc.ask）」）。

策略分类全表见 [docs/sekai-pc-agent.md](../../docs/sekai-pc-agent.md) §5。

## 模块

| 模块 | 内容 |
| --- | --- |
| `link` | 直连链路传输层：`Link` trait、`StdioLink`/`TcpLink`、内存链路（测试）、`u32` 大端长度前缀帧读写、退避重连 |
| `host` | `WorkerHost`：组装 Runtime、注册表与链路读循环 |
| `capability` | 宿主能力声明：`agent_registries*()`（`pc.*` 工具/任务 + prompt 通道）、`host_registries()`（仅测试/路由用：4 个 gpio 工具 + `echo`/`sleep`）、`SystemClock` |
| `llm_channel` | prompt 通道客户端（router 直连 / dsh stdin/stdout JSONL） |

Router 与本 crate 共用 `link` 抽象（`crate/sekai-router` 依赖本 crate 的 lib）。
协议见 [docs/sekai-protocol.md](../../docs/sekai-protocol.md)，PC agent 与 prompt
通道见 [docs/sekai-pc-agent.md](../../docs/sekai-pc-agent.md)，直连链路语义见
[docs/sekai-architecture.md](../../docs/sekai-architecture.md) §2。
