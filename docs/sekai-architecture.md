# Sekai 架构（Router / Worker 重构）

本文描述 2025 架构重构后的 Sekai 总体形态。重构参考了 DSH superagent 架构文档的 Router/Worker 分工：**常驻路由器负责评估与派工，Worker 负责执行与汇报**。原「server」改称 **Router**，原「设备」改称 **Worker**。

- **读者**：所有 sekai 贡献者；协议细节见 `docs/sekai-protocol.md`。
- **阶段声明**：第一阶段以 **PC Worker** 为主；**嵌入式 Worker 优先级置后**（保留协议与代码路径，不阻塞主线）。

---

## 1. 角色与 Crate 划分

| 角色 | Crate | 形态 | 职责 |
| --- | --- | --- | --- |
| 核心协议库 | `crate/sekai` | lib（no_std 兼容） | V1 帧编解码（Stdin/Stdout）、Worker 运行时（`sekai_runtime`）、DeviceList、时钟抽象。所有角色共用，不含 IO。 |
| Router | `crate/sekai-router` | 常驻进程 `sekai-router` | 连接/监听 Worker 链路，维护 Worker 能力快照与生命周期（Hello/Offline/boot_id/seq 去重），把自然语言目标经 LLM（function tools）评估、拆解、派工给 Worker，汇总结果。 |
| PC Worker | `crate/sekai-worker` | 宿主进程 `sekai-worker` | 第一阶段主角，以 **PC agent** 形态工作：驱动 `Runtime` 跑 Task/ToolCall，提供 `pc.*` 命令/文件工具与 `pc.ask` prompt 通道，通过**直连链路**（非 MQTT）与 Router 通信；`--gpio` opt-in 挂 `sekai-gpio` virtual 后端。 |
| 嵌入式 Worker | `crate/sekai-embedded` | 固件（esp32 feature 门控） | 置后阶段。ESP32 固件：真实 GPIO、WiFi、MQTT 主题链路。宿主构建（默认无 feature）只编译共享的 no_std 模块。 |
| GPIO 工具 | `crate/sekai-gpio` | lib | `gpio.*` 工具族：PC 经 `--gpio` opt-in 用 virtual 后端，esp32 用真实后端，stm32 预留。 |
| 构建工具 | `crate/sekai-build` | bin | 固件构建/烧录编排（服务嵌入式 Worker）。 |

**原则**：`crate/sekai` 不依赖任何 IO/执行器；Router 与两类 Worker 是对等消费者。PC Worker 与嵌入式 Worker **彻底拆分**：不共享启动层，只共享协议库与工具库。

## 2. 通信矩阵

| 链路 | 载体 | 帧格式 | 适用 |
| --- | --- | --- | --- |
| Router ↔ PC Worker | **直连**：stdio 子进程 或 TCP | 长度前缀（u32 大端）+ V1 Stdin/Stdout 帧，**不经 MQTT broker**，无主题路由 | 第一阶段 |
| Router ↔ 嵌入式 Worker | MQTT 主题（`{root}/dev/{id}/in|out|state`，QoS1，state retained + LWT） | MQTT PUBLISH 载荷 = 一帧 V1 Stdin/Stdout | 置后阶段 |
| Router ↔ LLM API | HTTPS（默认进程内 TLS；`SEKAI_HTTPS_MODE=helper` 可回退 node/curl） | JSON | Router 内部 |
| prompt ingress | TCP（`--prompt-addr`，默认 `127.0.0.1:7422`；批处理默认 `off`） | **JSONL（带外）**：`{"id":N,"method":"prompt",...}` 请求 / `ok+result` 或 `ok+error` 响应 | Worker `pc.ask`、DSH 插件中转、任意客户端 |

**为什么 PC Worker 不用 MQTT**（用户决策）：点对点链路不需要主题路由、遗嘱、跨 broker 跳转这些 MQTT 语义；直连把链路降到「有界二进制帧 + 完整投递」的最小契约，进程内内存链路即可测试，部署少一个 broker 依赖。V1 帧格式两条链路完全一致（协议 §1.2 本就声明 MQTT 只是载体）：

- MQTT 语义（retain、LWT、Jump、按主题订阅）**仅对嵌入式链路有意义**；直连链路上 Hello/Offline 就是普通帧，掉线由 TCP/管道关闭语义 + boot_id 变化表达。
- 去重语义两条链路一致：Router 按 `(worker_id, boot_id, seq)` 去重 Stdout。
- **直连链路保持活性**：PINGREQ/PINGRESP 真实跨链路（Router 与 PC Worker 双向），两端以
  1.5×keepalive（默认 45s）读空闲判死；对端静默死亡会走既有退避重连。因此 Router 与
  PC Worker 需**同版本配套**（旧 Worker 会吞掉对端 PINGREQ）。
- **多 Worker**：TCP 监听常驻 accept，每条连接一个独立 ControlPlane，按 Hello 的
  `worker_id` 路由；同一 id 的新连接顶替旧连接。

### 2.1 直连链路的命令行用法

Router 与 Worker 两侧都已提供面向使用者的入口（2026-11 起）：

```text
# 形态一：Router 监听，Worker 主动连入（两终端）
$ sekai-router                  # 无 --goal 进入交互会话（REPL）
sekai> /start                   # 引导式建连向导（别名 /connect）：菜单选链路、
                                # 回车用默认值，自动建链 + /wait + /workers
  … 选 2（tcp）→ 回车用默认 127.0.0.1:7421
sekai-router: 已监听 127.0.0.1:7421，等待 Worker 连入…
sekai-router: 在另一终端运行：sekai-worker connect 127.0.0.1:7421   ← 可直接复制
$ sekai-worker connect 127.0.0.1:7421
sekai-worker: TCP 已连接 127.0.0.1:7421
sekai-worker: Hello 已发布，链路已就绪
sekai-router: Worker 已上线：pc-worker（tools=… tasks=…）           ← poll 异步通知
sekai> 列一下当前目录有哪些文件      # 自然语言目标（pc.ls，只读默认放行）；同一会话可继续下发目标

# 形态二：Router 拉起 Worker 子进程（/start 选 1，或直接 /link stdio）
sekai> /link stdio              # 默认拉起 sekai-worker，stdin/stdout 即链路

# 形态三：全屏 TUI（ratatui；非终端管道输入时自动回退行式 REPL）
$ sekai-router --tui            # 状态栏（链路/就绪/Worker 数）+ 滚动输出窗格
                                # + 输入行；PgUp/PgDn 滚动，F10/Ctrl-Q 退出

# 形态四：批处理（一次性目标后退出，--link 选择链路）
$ sekai-router --link tcp --goal "列出 host-1 当前目录的文件"
$ sekai-router --link stdio --worker-bin ./sekai-worker --goal "…"

# 形态五：MQTT broker（嵌入式链路沿用）
$ SEKAI_MQTT_ADDR=127.0.0.1:1883 sekai-router --goal "…"     # 默认 --link mqtt
```

Worker 侧 `connect` 的连接失败有可操作提示（如「连接被拒绝：Router 可能未启动。
先在另一终端运行 sekai-router 并执行 /link tcp…」），断线按指数退避重连并显示
第 N 次重连与等待秒数。

命令层（`crate/sekai-router/src/session.rs`，含 `/start` 向导状态机与
`Session::poll` 异步通知）与前端（行式 `repl.rs` / 全屏 `tui.rs`）分离，
两个前端复用同一命令层。


## 3. 阶段计划

| 阶段 | 内容 | 状态 |
| --- | --- | --- |
| P0 | 术语与代码重命名（server→Router，device→Worker），sekai-start 拆分为 sekai-worker / sekai-embedded | 完成 |
| P1 | PC Worker 直连链路（stdio/TCP + 长度前缀帧）；Router 侧直连接入端点；router 测试全绿 | 完成 |
| P1.5 | 直连网关接入 Router CLI：`DirectGateway`（stdio/tcp）实现 WorkerGateway；`--link` 旗标；交互 REPL（/link 建链路 + 多轮 Agent 对话）；`sekai-worker connect <ADDR>` 子命令 | 完成 |
| P1.7 | 连接体验：`/start` 引导式建连向导 + 真实地址提示；`Session::poll` Worker 上线异步通知；`sekai-worker connect` 阶段化进度与可操作错误提示；ratatui 全屏 TUI（`--tui`，非 TTY 回退 REPL） | 完成 |
| P2 | Router 的 LLM 评估/派工回路吃到直连 Worker；PC agent 形态（`pc.*` 能力、`--gpio` opt-in、prompt 通道）端到端演示 | **单 Worker 闭环完成；剩余项见[缺口审计](sekai-gap-audit.md)** |
| P3 | 嵌入式 Worker：xtensa 工具链恢复、esp32 固件构建、MQTT 链路回归 | 置后 |

P2 的退出条件与未闭环清单以 [sekai-gap-audit.md](sekai-gap-audit.md) 为准（§1 闭环判定、
§9 工作包）。在审计的 B1–B11 关闭前，P2 不算完成。

## 4. 与 DSH superagent 架构的对照

| DSH 概念 | Sekai 对应 | 备注 |
| --- | --- | --- |
| orchestrator/主会话（评估、工单、槽位） | Router | Sekai 暂无工单/槽位系统，Router 直接维护 Worker 生命周期并派工 |
| 执行器会话（worker shell，工作目录，工具白名单） | Worker（PC/嵌入式） | PC Worker 是同机进程；嵌入式是远程固件 |
| Channel/Source | 无 | Sekai 的入口是 Router CLI 目标，不是消息源 |
| 预设/策略（router/worker 预设） | Worker 能力声明（Hello capabilities） | 协议层快照，Router 派工前校验 |

刻意**不引入**的 DSH 概念：工单（Ticket）、Eval 批处理、Outbox——Sekai 的控制平面是命令式的（cmd_id 生命周期），等出现多源触发需求再评估。

## 5. PC agent 能力与 prompt 通道

PC Worker 以 **PC agent** 形态提供本机能力：工具 `pc.exec`/`pc.read`/`pc.write`/
`pc.ls`/`pc.info`，任务 `pc.job`/`pc.ask`/`echo`/`sleep`（全表与输出格式见
[sekai-pc-agent.md](sekai-pc-agent.md)）；虚拟 GPIO 降为 `--gpio` opt-in。
`pc.ask` 把 prompt 交给 LLM 取回回答，路径有两条（`--llm` 旗标选择）：

```text
--llm router（connect 模式默认；Worker 直连 ingress）：

  pc.ask ──▶ Worker ──TCP JSONL──▶ Router prompt ingress（--prompt-addr，7422）
                                     │ Router 内置 LLM 评估/派工回路
  回答 ◀─────────────────────────────┘（按 id 回投）

--llm dsh（DSH worker 插件托管时；插件中转）：

  pc.ask ──▶ Worker ──stdout JSONL──▶ DSH worker 插件 ──TCP──▶ ingress
  回答 ◀─── Worker ◀──stdin JSONL────┘（每条 prompt 独立连接，id 匹配）
```

- **协议**：三方通用 JSONL（请求 `{"id":N,"method":"prompt","params":{"text":"…"}}`，
  成功 `{"id":N,"ok":true,"result":{answer,status,steps,usage}}`，失败
  `{"id":N,"ok":false,"error":{code,message}}`；`ping` 探活）。全量定义与错误码表见
  [sekai-pc-agent.md](sekai-pc-agent.md) §4。
- **防递归**：经 ingress（TCP）或 serve `prompt` 方法到达的 prompt 会在 Router 内
  跑一次 LLM 回路；该运行置内部位 `deny_llm_relay`，其内再派 `pc.ask` 一律
  `policy_denied`（「嵌套 prompt（pc.ask）」），与 allow-tasks/allow-admin 无关。
- **与 V1 帧协议的关系**：prompt 通道是**带外传输层能力**——JSONL 只出现在
  ingress 连接与 Worker↔插件的 stdin/stdout 上，V1 Stdin/Stdout 帧一字不变
  （见 [sekai-protocol.md](sekai-protocol.md) §0.1）。
- **策略**：`pc.read`/`pc.ls`/`pc.info` 只读白名单默认放行；`pc.write`/`pc.exec`
  需 allow-write；Task 通道（`pc.job`/`pc.ask`/`echo`/`sleep`）需 allow-tasks；
  ingress 做**策略授权**（`allow_write`/`allow_tasks`/`allow_admin`/`worker_filter`）。
- **访问控制**：ingress 默认只监听回环；绑定非回环必须显式
  `SEKAI_INGRESS_ALLOW_REMOTE=1` 且配置 `SEKAI_INGRESS_TOKEN`（连接首行 `hello` 握手），
  另有连接数（默认 4）与每连接 60 req/min 限制，请求记录 peer/method/outcome 审计行。
  分类细节见 [sekai-pc-agent.md](sekai-pc-agent.md) §5、§4.1。
