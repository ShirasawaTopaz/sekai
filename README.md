# Sekai

中文 | [English](README.en.md)

**请不要用于真实生产环境**

**把自然语言目标变成真实机器操作的最小控制平面。**

Sekai 是一个 Rust 编写的控制平面框架：常驻的 **Router** 用 LLM（DeepSeek
function tools）把自然语言目标评估、拆解成 Task / ToolCall，派给一个或多个
**Worker**（PC 进程或 ESP32 固件）执行，再把结果汇总成自然语言回答。第一个能跑通
的完整闭环是「*列一下当前目录有哪些文件* → Router → PC Worker 的 `pc.ls` → 目录
清单确实回来了」——PC Worker 即 **PC agent**：开箱执行本机命令与文件操作。

```text
   自然语言目标            Task / ToolCall（V1 二进制帧）
你 ──────────────▶ sekai-router ─────────────────────────▶ sekai-worker（PC Worker）
                    · LLM 评估/拆解/派工      ◀───────────   · Runtime 执行命令
                    · Worker 注册表/去重        Stdout 事件    · pc.* 工具（命令/文件）
                    · 安全策略（默认只读）                      │
                                                              │ 嵌入式阶段（置后）
                              同一套 V1 帧，改经 MQTT 主题承载 ▼
                                                    ESP32 固件（WiFi · 真实 GPIO）
```

- **协议极简**：控制平面只有两个方向——Router 下发 Stdin 帧、Worker 回 Stdout 帧；
  帧格式 [docs/sekai-protocol.md](docs/sekai-protocol.md) 一文说尽（V1 已冻结）。
- **承载无关**：PC Worker 走**直连链路**（stdio 子进程 / TCP，长度前缀 + V1 帧，
  不经 broker）；嵌入式 Worker 走 MQTT 主题（`{root}/dev/{id}/in|out|state`）。
  同一套帧，换载体不换协议。
- **多 Worker**：TCP 监听常驻 accept，可按 Worker id 同时路由多个直连 Worker；
  同名新连接顶替旧连接。
- **可取消、可异步**：serve 的 `tool_call`/`task` 支持 `async:true`（立即返回
  `cmd_id`，完成后发通知），配合 `cancel {cmd_id}`；REPL `/cancel`、TUI `Ctrl-C`
  可中止在途长任务。
- **默认只读**：`pc.exec`/`pc.write` 等写操作、Task 下发、管理工具分别由开关解锁；
  策略在 Router 侧发布 Stdin **之前**强制执行。
- **prompt 通道（带外）**：Worker 的 `pc.ask` 任务可把问题经 Router prompt ingress
  （直连或经 DSH 插件中转）交给 LLM 取回回答；JSONL 带外传输，不改变 V1 帧。
- **内置 HTTPS**：Router 默认用 `native-tls`（Windows schannel / 系统 OpenSSL）
  直连 DeepSeek API，不再需要 node 或 curl（`SEKAI_HTTPS_MODE=helper` 可回退）。
- **`no_std` 核心**：`crate/sekai` 只依赖 `core` + `alloc`，同一份协议代码同时
  编译进宿主测试与 ESP32 固件。

## 仓库布局

| 路径 | 形态 | 职责 |
| --- | --- | --- |
| [crate/sekai](crate/sekai) | lib（no_std） | 核心协议库：V1 帧编解码、Worker Runtime、DeviceList、时钟抽象、MQTT 5 客户端状态机。零 IO 依赖，所有角色共用。 |
| [crate/sekai-router](crate/sekai-router) | 常驻进程 | 连接/监听 Worker 链路，维护 Worker 生命周期（Hello/Offline/boot_id/seq 去重），LLM 评估派工，汇总回答；另开 prompt ingress（`--prompt-addr`）。 |
| [crate/sekai-worker](crate/sekai-worker) | 宿主进程 | PC Worker（**PC agent**）：驱动 Runtime 跑 Task/ToolCall，经直连链路（stdio/TCP）与 Router 通信，提供 `pc.*` 命令/文件能力与 `pc.ask` prompt 通道；虚拟 GPIO 为 `--gpio` 可选。 |
| [crate/sekai-gpio](crate/sekai-gpio) | lib（no_std） | 家族无关 GPIO 抽象 + `gpio.info/config/read/write` 用户工具；ESP32/STM32F1/F4 引脚表、真实后端与宿主虚拟后端（`--gpio` opt-in 后的嵌入式演示）。 |
| [crate/sekai-embedded](crate/sekai-embedded) | 固件（esp32 feature 门控） | 嵌入式 Worker 启动层（ESP32）：MQTT 传输、硬件时间、真实 GPIO。**当前阶段置后**。 |
| [crate/sekai-build](crate/sekai-build) | bin | 固件构建/烧录编排（`build` / `flash`），服务嵌入式 Worker。 |
| [plugins/sekai-dsh](plugins/sekai-dsh) | DSH 插件 bundle | 把 `sekai-router --serve` 与 `sekai-worker connect` 接入 DeepSeek Harness，让 DSH 主会话直接操作 Worker；安装见 [docs/sekai-dsh-plugin.md](docs/sekai-dsh-plugin.md) §1（一键脚本）。worker 插件带 `autoConnect` 开关（自动连接上次连接的 Router）与 `sekai_worker_connect`/`sekai_worker_disconnect` 手动控制。 |
| [docs/](docs/) | 文档 | 协议、架构、GPIO、ESP32、DSH 插件的完整文档，见[文档导航](#文档导航)。 |

## 快速开始

### 前置要求

- Rust **1.88+**（workspace 为 edition 2024；代码使用 1.88 才稳定的 let-chains）；`cargo` 在 PATH 上。
- `DEEPSEEK_API_KEY`：凡是真正调用 LLM 的形态都需要（批处理启动时校验；
  交互会话启动时不校验、输入目标时用到；`pc.ask` 与 prompt ingress 同样需要；
  `--dry-run` 与 `--serve` 的链路操作不需要）。
- 无需任何硬件：PC Worker 即 **PC agent**，开箱执行本机命令/文件操作；虚拟 GPIO
  为 `--gpio esp32|stm32f1|stm32f4` 可选（嵌入式演示）。

### 构建

```bash
cargo build --release -p sekai-router -p sekai-worker
# 产物：target/release/sekai-router(.exe) 与 target/release/sekai-worker(.exe)
```

### 在 DSH 里用插件（可选）

构建之后，一条命令把两个二进制接入 DSH（构建 → 部署二进制 → 安装 bundle → 自检）：

```bash
node scripts/install-dsh-plugin.mjs [--profile <name>]
```

重启 DSH 会话后出现 `sekai_*` 工具；`--dry-run`/`--uninstall`、profile 自动选择与
二进制解析顺序见 [docs/sekai-dsh-plugin.md](docs/sekai-dsh-plugin.md) §1。

### 两个终端跑通闭环

```bash
# 终端 1：启动 Router（进入交互 REPL）
export DEEPSEEK_API_KEY=sk-...        # Windows PowerShell: $env:DEEPSEEK_API_KEY="sk-..."
./target/release/sekai-router

sekai> /start        # 引导式建连向导：选 2（tcp），回车用默认 127.0.0.1:7421
                    # 向导会提示可直接复制的另一端命令

# 终端 2：Worker 主动连入
./target/release/sekai-worker connect 127.0.0.1:7421

# 终端 1 会异步通知：Worker 已上线：pc-worker（tools=… tasks=…）
sekai> 看一下这台机器的系统信息        # → pc.info（只读，默认放行）
sekai> 列一下当前目录有哪些文件        # → pc.ls（只读，默认放行）
```

进阶（按需解锁，`--allow-write`/`--allow-tasks` 启动旗标或 REPL `/policy`）：

```bash
sekai> 跑一下 cargo --version         # → pc.exec（需 allow-write；shell 执行）
sekai> 帮我问一下 LLM：Rust 的 Deref 是什么   # → pc.ask（需 allow-tasks；经 Router
                                       #   prompt ingress 取回回答，防递归）
```

`pc.*` 工具/任务全表与环境变量（`SEKAI_PC_EXEC_MS`/`SEKAI_PC_ASK_MS`）见
[docs/sekai-pc-agent.md](docs/sekai-pc-agent.md)。

REPL 常用命令：`/help`、`/link <mqtt|stdio|tcp>`（建/换链路）、`/workers`
（观察注册表）、`/wait`（等 Worker 上线）、`/policy`（查看/调整策略）、`/exit`。
非 `/` 开头的输入一律作为自然语言目标；同一会话可连续下发多个目标（不保留跨目标上下文）。

### 其他运行形态

| 形态 | 命令 | 说明 |
| --- | --- | --- |
| 批处理 | `sekai-router --link tcp --goal "列出当前目录的文件"` | 执行一个目标后退出，适合脚本化；`--json` 输出单个 JSON 结果 |
| 拉起子进程 | `sekai-router --link stdio`（REPL 内 `/link stdio`） | Router 直接 spawn `sekai-worker`，stdin/stdout 即链路，单进程演示 |
| 全屏 TUI | `sekai-router --tui` | ratatui 界面：状态栏 + 滚动输出 + 输入行；非终端管道下自动回退行式 REPL |
| 无头服务 | `sekai-router --serve` | stdin/stdout 走 JSONL 协议（链路操作不调 LLM、无需 API key；`prompt` 方法经内置回路调 LLM），供 [DSH 插件](plugins/sekai-dsh)驱动 |
| 飞书对话入口 | `sekai-router --feishu` | WebSocket 长连接收 IM 事件、REST 回复原消息；同会话注入会话元数据与最近 N 轮问答（内存，`SEKAI_FEISHU_CONTEXT_TURNS`，默认 8）。需 `SEKAI_FEISHU_APP_ID`/`_APP_SECRET`；链路默认 tcp；详见 [docs/sekai-feishu.md](docs/sekai-feishu.md) 与 [建连 skill](skills/feishu-setup/SKILL.md) |
| MQTT 链路 | `SEKAI_MQTT_ADDR=127.0.0.1:1883 sekai-router --goal "…"` | 嵌入式链路沿用（默认 `--link mqtt`），需要 MQTT 5 broker |
| 只看链路 | `sekai-router --dry-run` | 连接并列出 Worker，不调用 LLM，无需 API key |

## 环境变量（常用）

| 变量 | 含义 | 默认 |
| --- | --- | --- |
| `DEEPSEEK_API_KEY`（或 `API_KEY`） | DeepSeek API key | 无（调 LLM 时必需） |
| `DEEPSEEK_BASE_URL`（或 `BASE_URL`） | OpenAI 兼容 base URL | `https://api.deepseek.com` |
| `SEKAI_ROUTER_MODEL` | 模型名 | `deepseek-flash` |
| `SEKAI_ROUTER_REASONING_EFFORT` | 思考强度：设置后启用 thinking 并传 `reasoning_effort`（`minimal`/`low`/`medium`/`high`/`xhigh`/`max`/`ultra`；缺省关闭 thinking） | 无（关闭） |
| `SEKAI_LINK` | 链路类型 `mqtt`/`stdio`/`tcp` | `mqtt` |
| `SEKAI_LISTEN_ADDR` | tcp 链路监听地址 | `127.0.0.1:7421` |
| `SEKAI_PROMPT_ADDR` | Router prompt ingress 地址（`HOST:PORT`/`off`；交互/serve 默认 `127.0.0.1:7422`，批处理默认 `off`） | 见左 |
| `SEKAI_INGRESS_TOKEN` | prompt ingress 认证 token（远程连接必需；回环可省略） | 无 |
| `SEKAI_INGRESS_ALLOW_REMOTE` | 允许 ingress 绑定非回环地址（`1` 显式开启） | 关闭 |
| `SEKAI_HTTPS_MODE` | HTTPS 后端：`native`（默认，进程内 TLS）/ `helper`（node/curl 回退） | `native` |
| `SEKAI_FEISHU_APP_ID` / `SEKAI_FEISHU_APP_SECRET` / `SEKAI_FEISHU_ALLOW_USERS` / `SEKAI_FEISHU_ALLOW_CHATS` / `SEKAI_FEISHU_CONTEXT_TURNS` | 飞书入口凭据、allowlist 与会话上下文轮数（默认全拒、禁 `*`；`--feishu` 必需凭据；`CONTEXT_TURNS` 默认 8、0 关闭多轮记忆；其余 `SEKAI_FEISHU_*` 见 [docs/sekai-feishu.md](docs/sekai-feishu.md) §8） | 无 / 空 / 8 |
| `SEKAI_PC_EXEC_MS` | Worker `pc.exec` 内部超时（100..=300000） | `10000` |
| `SEKAI_PC_ASK_MS` | Worker `pc.ask` 内部硬超时（1000..=300000） | `300000` |
| `SEKAI_MQTT_ADDR` | MQTT broker 地址（mqtt 链路） | 无 |
| `SEKAI_WORKER_BIN` | stdio 链路拉起的 Worker 程序 | `sekai-worker` |
| `SEKAI_TOPIC_ROOT` | MQTT 主题根 | `sekai/v1` |
| `SEKAI_MAX_STEPS` / `SEKAI_DDL_MS` / `SEKAI_TIMEOUT_MS` / `SEKAI_WAIT_WORKER_MS` | agent 步数 / 执行预算 / 命令超时 / 等待上线（步数缺省：直连 PC Worker 不设上限，MQTT/嵌入式 `12`） | 见左 / 10000ms / — / 5000ms |
| `SEKAI_ROUTER_TRACE` | 脱敏 JSONL trace 路径 | 关闭 |

完整清单见 `sekai-router --help`。

## 安全模型

- **默认只读**：`pc.exec` / `pc.write` 与 `gpio.config` / `gpio.write` 等写操作、
  Task 下发（`pc.job` / `pc.ask` / `echo` / `sleep`）、管理与其他工具，分别需要
  `--allow-write` / `--allow-tasks` / `--allow-admin`（或 REPL `/policy`、serve 侧
  `policy.set`）显式解锁；`pc.read` / `pc.ls` / `pc.info` 只读白名单默认放行。
- **拒绝即不发布**：策略校验发生在 Router 组帧发布 Stdin 之前，被拒命令
  根本不会到达 Worker。`pc.exec`/`pc.job` 在 Worker 本机执行任意 shell 命令，
  权限面最大，仅在受控环境放开。
- **prompt 防递归**：经 prompt ingress / serve `prompt` 到达的 LLM 运行内再派
  `pc.ask` 一律拒绝（`policy_denied`「嵌套 prompt（pc.ask）」），与开关状态无关。
- **ingress 访问控制**：prompt ingress 默认只监听回环；绑定非回环地址必须显式
  `SEKAI_INGRESS_ALLOW_REMOTE=1`，且必须配置 `SEKAI_INGRESS_TOKEN`（连接第一行
  需 `hello` 握手）。另有限制：并发连接默认 4、每连接 60 请求/分钟，每个请求都会
  记录 peer/method/outcome 审计行。
- **`--worker <ID>`**（或 `policy.worker_filter`）：把可操作范围锁定到单个 Worker。
- 凭据只从环境变量或显式 CLI 参数读取，Debug 输出脱敏，不落盘。

## 文档导航

| 文档 | 内容 | 读者 |
| --- | --- | --- |
| [docs/sekai-architecture.md](docs/sekai-architecture.md) | Router/Worker 总体架构、通信矩阵、PC agent 与 prompt 通道、阶段计划 | 所有贡献者，**先读这篇** |
| [docs/sekai-gap-audit.md](docs/sekai-gap-audit.md) | 缺口审计：未实现/未闭环清单、证据、优先级与建议工作包 | 评估范围、排期与验收的人 |
| [docs/sekai-protocol.md](docs/sekai-protocol.md) | V1 控制平面协议：帧格式、主题、QoS/retain、Hello、错误码 | 协议实现者、调试链路的工程师 |
| [docs/sekai-pc-agent.md](docs/sekai-pc-agent.md) | PC agent：`pc.*` 工具/任务全表、prompt JSONL 协议、三方配置与安全 | PC Worker 使用者、插件/客户端实现者 |
| [docs/sekai-gpio.md](docs/sekai-gpio.md) | GPIO 抽象、引脚表、`gpio.*` 工具语法与 AI 调用流程（`--gpio` opt-in 后的嵌入式演示） | 板级开发者、GPIO 集成者 |
| [docs/sekai-esp32.md](docs/sekai-esp32.md) | ESP32 固件：工具链、构建烧录、WiFi/MQTT 链路、排障 | 固件开发者 |
| [docs/sekai-dsh-plugin.md](docs/sekai-dsh-plugin.md) | Sekai × DSH 插件形态与 serve 协议概览（v1.1 `prompt`、v1.2 异步/取消/管理） | DSH 使用者、插件实现者 |
| [docs/sekai-feishu.md](docs/sekai-feishu.md) | 飞书对话入口（`--feishu`）：长连接协议、消息回路、安全模型、建连 skill | 飞书接入使用者、Router 实现者 |
| [CHANGELOG.md](CHANGELOG.md) | 版本变更记录（0.2.0：B/K/P 缺口闭环、serve v1.2、ingress 安全、进程内 HTTPS） | 所有使用者 |
| [plugins/sekai-dsh/README.md](plugins/sekai-dsh/README.md) | DSH 插件配置全量说明（安装、config 键、JSONL 协议） | 插件使用者 |
| [docs/sekai-ai.md](docs/sekai-ai.md) | 历史文档：AI 控制端设计记录（角色现由 Router 承担） | 了解演进史 |
| [docs/sekai-ai-plan.md](docs/sekai-ai-plan.md) / [docs/sekai-gpio-plan.md](docs/sekai-gpio-plan.md) | 历史计划文档 | 按需查阅 |

各 crate 的简介见其目录下 README（[sekai](crate/sekai)、[sekai-router](crate/sekai-router)、
[sekai-worker](crate/sekai-worker)、[sekai-gpio](crate/sekai-gpio)、
[sekai-embedded](crate/sekai-embedded)、[sekai-build](crate/sekai-build)）。

## 项目状态

当前以 **PC Worker** 为主线；**嵌入式 Worker（ESP32）优先级置后**（协议与代码
路径保留，不阻塞主线）。阶段进度（详见
[docs/sekai-architecture.md](docs/sekai-architecture.md) §3）：

| 阶段 | 内容 | 状态 |
| --- | --- | --- |
| P0–P1.7 | 术语重构、直连链路（stdio/TCP）、REPL/TUI、连接向导 | 完成 |
| P2 | LLM 评估/派工回路吃到直连 Worker；PC agent 形态（`pc.*` 能力、`--gpio` opt-in、prompt 通道）端到端演示 | **B1–B11/K/P 已闭环（2026-11-04，见[缺口审计](docs/sekai-gap-audit.md) §0.4）；P13 实机验证置后** |
| P3 | 嵌入式 Worker：esp32 固件构建、MQTT 链路回归 | 置后 |

P2 的验收结果与仍置后项见 [docs/sekai-gap-audit.md](docs/sekai-gap-audit.md) §0.4；
变更记录见 [CHANGELOG.md](CHANGELOG.md)。

## 开发

```bash
cargo test --workspace        # 宿主全量测试（无需硬件、无需网络、无需 broker）
cargo build --workspace       # 全 workspace 构建（自动跳过 esp32 门控的固件镜像）
```

- 嵌入式固件构建（需要 Xtensa 工具链）见 [crate/sekai-embedded/README.md](crate/sekai-embedded/README.md)
  与 [docs/sekai-esp32.md](docs/sekai-esp32.md)。
- DSH 插件的本地冒烟（无 Rust 依赖）见 [plugins/sekai-dsh/README.md](plugins/sekai-dsh/README.md)。

## 许可证

Apache License 2.0，见 [LICENSE](LICENSE)。各 crate 经 workspace 清单继承
`license = "Apache-2.0"`；DSH 插件在其 `package.json` 中声明。
