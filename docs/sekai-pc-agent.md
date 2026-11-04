# Sekai PC Agent（pc.* 工具与 prompt 通道）

本文描述 PC Worker 的 **PC agent 形态**：开箱即执行本机命令与文件操作的 `pc.*` 工具/任务，
以及把 prompt 交给 LLM 取回回答的 **prompt 通道**（带外 JSONL，不改变 V1 帧）。

- **读者**：驱动 PC Worker 的使用者（REPL / DSH 插件）、prompt ingress 的客户端实现者、
  需要 audit `pc.*` 权限面的安全评审者。
- **实现基线**：`crate/sekai-worker`（`pc.*` 注册、`--llm`/`--prompt-addr`/`--gpio`、prompt 通道客户端）、
  `crate/sekai-router`（prompt ingress、serve `prompt` 方法、策略分类）、
  `plugins/sekai-dsh`（dsh 中转）。文中工具语法、旗标、协议字段以这些源码为准。
- **相关文档**：`docs/sekai-protocol.md`（V1 帧；prompt 通道是带外能力，见其 §0.1）、
  `docs/sekai-architecture.md` §5（PC agent 能力与 prompt 通道）、
  `docs/sekai-gpio.md`（`--gpio` 可选的虚拟 GPIO）、
  `plugins/sekai-dsh/README.md`（插件 config 键）。

```text
              自然语言目标 / DSH 工具调用
                        │
                 sekai-router（LLM 评估/派工/汇总）
                        │  V1 帧（直连 stdio/TCP；嵌入式 MQTT）
                 sekai-worker（PC agent）
                  ├─ 工具：pc.exec / pc.read / pc.write / pc.ls / pc.info
                  ├─ 任务：pc.job / pc.ask / echo / sleep
                  └─ （可选）--gpio esp32|stm32f1|stm32f4 → 追加 gpio.*（虚拟 GPIO）
                        │
                        │ prompt 通道（带外 JSONL，帧协议不变）
                  ├─ router：Worker 直连 Router prompt ingress（TCP）
                  └─ dsh  ：Worker stdin/stdout JSONL → DSH worker 插件中转 → ingress
```

---

## 1. 形态与默认能力

PC Worker 是**正常 agent**：默认注册的本机能力不再是虚拟 GPIO，而是命令执行与文件操作。
GPIO 降级为 `--gpio` opt-in（嵌入式演示用）。

默认注册（无 `--gpio`）：

| 类别 | 名称 |
| --- | --- |
| 工具（Tool） | `pc.exec`、`pc.read`、`pc.write`、`pc.ls`、`pc.info` + 内置 `sekai.*` |
| 任务（Task） | `pc.job`、`pc.ask`、`echo`、`sleep` |

加 `--gpio esp32|stm32f1|stm32f4`（`--family` 为兼容别名）后**追加**
`gpio.info` / `gpio.config` / `gpio.read` / `gpio.write`（虚拟 GPIO，见 `docs/sekai-gpio.md`）。

## 2. 工具（Tool）参考

args 与输出均为**原始 UTF-8 文本**（无 key=value 前缀；与 `gpio.*` 的文本参数风格一致）。
输出统一截断：超出上限时按 UTF-8 字符边界截断并追加 `[[truncated]]` 标记行；
`pc.exec` 超时追加 `[[timeout after <ms>ms]]` 标记行。

> 注意：上表是 **Worker 侧**输出上限。Router 回填给模型的 payload 还有一层
> `PAYLOAD_TRUNCATE_BYTES = 3584` 字节的展示截断（`crate/sekai-router/src/tools.rs`），
> 因此模型看到的字节数可能少于 Worker 文档上限；需要完整内容时应分块读取或写文件。

### 2.1 `pc.exec` —— 执行命令（Task）

> **v0.2 起 `pc.exec` 是 Task 而不是 Tool**：Worker 不再在帧循环里同步等命令结束，
> 执行期间仍能响应 `sekai.time`、`gpio.read` 等命令。事件序列为
> `Accepted → Started → Completed/Failed`；Router 侧仍可用 `worker_tool` 按名字调用
> （按 Worker 通告的 tasks 列表自动按 Task 下发），策略分类不变（写操作，
> 需 `--allow-write`）。

| 项 | 值 |
| --- | --- |
| args | 命令文本整体（去首尾空白；允许换行） |
| shell | Windows 走 `cmd /C`，其余走 `sh -c` |
| 输出 | 首行 `exit=<code>`，随后为 stdout+stderr 合并输出（先后顺序不保证） |
| 编码 | 输出统一 UTF-8（非法字节替换为 U+FFFD；结果 JSON 带 `encoding` 字段） |
| 截断 | 总输出 ≤4096 字节，按 UTF-8 字符边界截断，追加 `[[truncated]]` |
| 超时 | 内部默认 **10000ms**（env `SEKAI_PC_EXEC_MS`，范围 100..=300000）；超时杀**整棵进程树**、保留已收输出并追加 `[[timeout after <ms>ms]]` |
| 进程树 | Unix 用独立进程组（`process_group(0)` + `kill -KILL -pgid`）；Windows 先 `taskkill /T /F /PID` 再 kill 父进程，孙进程不会成孤儿 |
| stdin | 子进程 stdin 为 `null`，不会抢 Worker 的 V1 帧流 / prompt JSONL |
| 策略 | 需 `--allow-write`（Router 侧默认拒绝） |

### 2.2 `pc.read` —— 读文件（工具）

| 项 | 值 |
| --- | --- |
| args | 首行是路径（其余行忽略） |
| 输出 | 文件内容 |
| 截断 | ≤4000 字节；超出截断到 4000 并追加换行 + `[[truncated]]` 行 |
| 失败 | 路径为空/不可读 → `Failed(InvalidArgument)` |
| 策略 | 只读，默认放行 |

### 2.3 `pc.write` —— 写文件（工具）

| 项 | 值 |
| --- | --- |
| args | 首行是路径，**其余全部**（含后续换行）为写入内容 |
| 语义 | 覆盖写；不创建父目录 |
| 输出 | `written=<字节数>` |
| 策略 | 需 `--allow-write` |

### 2.4 `pc.ls` —— 列目录（工具）

| 项 | 值 |
| --- | --- |
| args | 路径（整体去首尾空白） |
| 输出 | 每行 `d <name>`（目录）或 `f <name>`（文件），按名称排序 |
| 截断 | 双预算：上限 200 行 **且** ≤3500 字节（给 4096 字节 V1 帧留余量），超出追加 `[[truncated]]` |
| 策略 | 只读，默认放行 |

> 结果不会因条目多/名字长而触发 `ResultTooLarge`：字节预算会先截断并标记
> （B4.5 已修）。需要完整列表时建议分批读取或改用 `pc.exec` 重定向到文件。

### 2.5 `pc.info` —— 宿主信息（工具）

无 args。多行输出：

```text
os=<windows|linux|…>
host=<COMPUTERNAME / HOSTNAME，缺省 unknown>
cwd=<当前目录>
pid=<进程 id>
llm=<router|dsh|off>
gpio=<esp32|stm32f1|stm32f4|none>
```

## 3. 任务（Task）参考

> **预算提醒**：Router 侧默认执行预算 `SEKAI_DDL_MS=10000`（B4.1 起；旧值为 1000）。
> 超过 10s 的 `pc.exec`、以及 `pc.job`/`pc.ask` 这类长任务应在调用时显式给
> `ddl_ms`（建议 ≥60000，`pc.ask` 上限 300000），否则会在 Worker 侧
> `Failed(Timeout)`。
>
> **并发**：同一个 Worker 最多 4 条在途命令（`max_inflight_per_worker`，B5 起；
> 旧值为 1）。因此 `pc.ask` 的内层 agent 可以在任务运行期间继续调用同一 Worker 的
> 工具（`pc.read`、`gpio.read` 等）；超过 4 条时 Router 回 `worker_busy`。

### 3.1 `pc.job` —— 长命令（任务）

| 项 | 值 |
| --- | --- |
| args | 命令文本（去首尾空白，非空） |
| 推进 | 协作式 poll（Task 生命周期，见 `docs/sekai-protocol.md` §7.1） |
| 预算 | DDL 到期杀进程并 `Failed(Timeout)` |
| Done | `exit=<code>` 行 + 输出**尾部** ≤4096 字节（累积超限只保留尾部；截断追加 `[[truncated]]` 行） |
| 策略 | Task 通道：需 `--allow-tasks`（见 §5.1） |

### 3.2 `pc.ask` —— 问 LLM（任务）

| 项 | 值 |
| --- | --- |
| args | prompt 文本（去首尾空白，非空） |
| 通道 | `--llm router`：直连 Router prompt ingress（TCP JSONL，默认 `HOST:7422`）；`--llm dsh`：进程 stdin/stdout JSONL 经 DSH worker 插件中转到 ingress |
| Done | 回答文本（≤4096，截断追加 `[[truncated]]`） |
| 通道未启用 | `Failed(InvalidArgument)`，错误消息含 `--llm` 提示 |
| 通道报错 | 透传：`unreachable` / `llm_unavailable` 等 → `InvalidArgument`；`timeout` → `Timeout` |
| 硬超时 | 内部默认 **300000ms**（env `SEKAI_PC_ASK_MS`，范围 1000..=300000） |
| 策略 | 作为 Task 派发需 `--allow-tasks`（见 §5.1）；经 ingress/serve 到达的 prompt 运行内**一律拒绝再派 `pc.ask`**（防递归，见 §5.2） |

### 3.3 `echo` / `sleep`（保留任务）

- `echo`：args 为任意字节，Done 原样返回。
- `sleep`：args 为毫秒数文本（≤3600000），到期 Done 空载荷。

## 4. prompt JSONL 协议（三方通用）

同一条 JSONL 约定用于三段链路：**Worker ↔ DSH 插件**（stdin/stdout）、
**DSH 插件 ↔ ingress**（TCP）、**任意客户端 ↔ ingress**（TCP）。每行一个 JSON 对象。

- 请求（Worker stdout / 客户端 → ingress）：

```json
{"id":42,"method":"prompt","params":{"text":"..."}}
```

`params.text` 必填（1..=8192 字节）；可选 `params.max_steps`（1..=32）限定该次评估的
agent 步数上限。

> **注意**：8192 字节是 ingress / HTTP 类客户端的协议上限。经 Worker `pc.ask` 发出的
> prompt 会作为 V1 Task 载荷传输，受 `MAX_PAYLOAD = 4096` 字节限制（见
> `docs/sekai-protocol.md` §9.4）。

- 成功响应（回投 Worker stdin / 客户端连接）：

```json
{"id":42,"ok":true,"result":{"answer":"...","status":"completed","steps":3,"usage":{"prompt_tokens":10,"completion_tokens":5,"total_tokens":15}}}
```

`status` ∈ `completed` | `max_steps`；`steps` 为该次评估的 agent 步数；
`usage` 为 token 用量计数。

- 失败响应：

```json
{"id":42,"ok":false,"error":{"code":"unreachable","message":"..."}}
```

`code` 取值（冻结）与语义：

| code | 语义 |
| --- | --- |
| `llm_unavailable` | Router 未配置 API key 等，LLM 不可用 |
| `not_linked` | 无 Worker 链路 |
| `no_workers` | 无已发现的 Worker |
| `invalid_arguments` | `text` 为空/超 8192 字节，或 `max_steps` 越界（1..=32） |
| `agent_error` | Router 内 Agent 循环失败 |
| `timeout` | 等待回答超时 |
| `unreachable` | ingress 连接失败（DSH 插件中转本地产生） |
| `unauthorized` | ingress 认证失败：未 hello 握手或 token 不匹配 |
| `rate_limited` | 每连接每分钟请求数超限 |
| `too_many_connections` | ingress 并发连接数超限 |
| `busy` | Router 内 prompt 运行槽位已满（默认 4 路并发） |
| `unknown_method` | 未知方法名 |

- ingress 另支持探活：`{"id":1,"method":"ping"}` → `{"pong":true}`。

`id` 为整数，由请求方分配；响应按 `id` 匹配。一条连接上可以只承载一对请求-响应
（DSH 插件中转即为每条 prompt 独立建连）。

ingress 与 serve `prompt` 方法共用同一核心，只做**策略授权**
（`allow_write`/`allow_tasks`/`allow_admin`/`worker_filter` 原样继承，不新增开关）。

### 4.1 访问控制（B9 起）

| 项 | 行为 |
| --- | --- |
| 默认绑定 | 仅回环地址（`127.0.0.1`/`::1`） |
| 非回环绑定 | 必须显式 `SEKAI_INGRESS_ALLOW_REMOTE=1`，且必须配置 token，否则启动失败 |
| token 来源 | `SEKAI_INGRESS_TOKEN`，或 `SEKAI_INGRESS_TOKEN_FILE`（取文件首行） |
| 握手 | 配置 token 后，每条连接**第一行**必须是 `{"id":N,"method":"hello","params":{"token":"…"}}`；成功回 `{"id":N,"ok":true,"result":{"authenticated":true}}`，失败回 `unauthorized` 并断开 |
| 并发 | 连接数上限默认 4（`SEKAI_INGRESS_MAX_CONNECTIONS`），超出直接回 `too_many_connections` 并关闭 |
| 限流 | 每连接每分钟 60 请求（`SEKAI_INGRESS_RATE`），超出回 `rate_limited` 并关闭 |
| 审计 | 每个请求在 Router stderr 记一行 `peer=… method=… outcome=ok|denied`（不记 prompt 正文） |
| 并发执行 | 同时最多 4 个 prompt 运行；满员时短等后回 `busy`（`PromptCore::run_gate`） |

回环 + 未配置 token 的旧用法保持兼容（本地 DSH 插件不需要改）。

## 5. 安全模型与防递归

### 5.1 策略分类（Router 侧，发布 Stdin 前强制）

Router 按「**调用通道 + 工具名**」分类（`tools.rs::check_policy`），工具通道与任务通道
门槛不同：

| 通道 | 名称 | 默认 | 解锁开关 |
| --- | --- | --- | --- |
| `worker_tool` | `pc.read` / `pc.ls` / `pc.info` | 放行 | —（只读白名单） |
| `worker_tool` | `pc.write` / `pc.exec` | 拒绝 | `--allow-write`（写白名单） |
| `worker_tool` | 其余一切名字（admin 桶） | 拒绝 | `--allow-admin` |
| `worker_task` | 一切任务名：`pc.job` / `pc.ask` / `echo` / `sleep` | 拒绝 | `--allow-tasks`（不按名字分类） |

即：Worker 把 `pc.job`/`pc.ask` 广告为 Task，实际门槛是 `--allow-tasks`；「admin 桶
（`--allow-admin`）」只作用于 `worker_tool` 通道里不在两个白名单内的名字。
策略可经 REPL `/policy` 或 serve `policy.set` 调整；`--worker <ID>`/
`worker_filter` 可把可操作范围锁定到单个 Worker。

> **`pc.exec` 的线类型与策略分离**：`pc.exec` 在 Worker 侧实现为 Task（不阻塞帧循环），
> 但 Router **按名字**把它归为写操作，门槛仍是 `--allow-write`，不是 `--allow-tasks`。
> 同样地，`worker_tool` 命中 Worker 通告的 tasks 列表时会自动按 Task 下发（D5）。

`pc.exec`/`pc.job` 在 Worker 本机以 shell 执行任意命令，权限面最大：默认拒绝之外，
还应仅在受控环境放开。

### 5.2 防递归（deny_llm_relay）

经 prompt ingress（TCP）或 serve `prompt` 方法到达的 prompt，会在 Router 内跑一次 LLM
评估/派工回路生成回答；该运行置内部位 `deny_llm_relay=true`，此时**两个通道的
`pc.ask` 一律拒绝**（`policy_denied`，what=「嵌套 prompt（pc.ask）」）——即使
`--allow-tasks`/`--allow-admin` 全开也拒，其余工具不受影响。否则 LLM 回答 LLM 的
问题可以无限自我嵌套。`deny_llm_relay` 是内部位：不出现在 CLI 与
`policy.get`/`policy.set` 的四键里，策略摘要仅在生效时追加「嵌套 prompt：拒绝」。

## 6. 三方配置清单

### 6.1 Router（`sekai-router`）

| 项 | 值 |
| --- | --- |
| `--prompt-addr HOST:PORT\|off` | prompt ingress 监听地址；`off` 关闭。env `SEKAI_PROMPT_ADDR` 同名覆盖 |
| 默认 | 交互/serve 模式 `127.0.0.1:7422`；批处理模式 `off` |
| env `SEKAI_INGRESS_TOKEN` / `SEKAI_INGRESS_TOKEN_FILE` | ingress 认证 token（配置后连接需 hello 握手） |
| env `SEKAI_INGRESS_ALLOW_REMOTE` / `SEKAI_INGRESS_MAX_CONNECTIONS` / `SEKAI_INGRESS_RATE` | 允许非回环绑定 / 连接上限（默认 4） / 每连接每分钟请求上限（默认 60） |
| env `SEKAI_HTTPS_MODE` | `native`（默认，进程内 TLS）或 `helper`（node/curl 回退） |
| serve 方法 | JSONL 新增 `prompt`（v1.1）、`cancel`/`commands`/`admin`/`jump` 与 `async:true`（v1.2，见 `docs/sekai-dsh-plugin.md` §2） |

### 6.2 Worker（`sekai-worker`）

| 旗标 | 值 |
| --- | --- |
| `--llm router\|dsh\|off` | prompt 通道：router=直连 ingress；dsh=经 DSH 插件中转；off=关闭。默认：connect 模式 `router`，stdio 模式 `off`（stdio 的 stdin/stdout 是链路本身，router/dsh 需 connect 模式） |
| `--prompt-addr HOST:PORT` | router 模式的 ingress 目标；默认 connect 主机 + 端口 `7422`；仅 `--llm router` 时有效 |
| `--gpio esp32\|stm32f1\|stm32f4` | opt-in 虚拟 GPIO（`--family` 兼容别名） |
| `--gpio-inputs IO4=1,PA5=0` | 给虚拟 GPIO 注入初始输入电平（`0/1/low/high`；需同时 `--gpio`；P12） |
| env `SEKAI_PC_EXEC_MS` | `pc.exec` 内部超时（100..=300000，默认 10000） |
| env `SEKAI_PC_ASK_MS` | `pc.ask` 内部硬超时（1000..=300000，默认 300000） |

### 6.3 DSH worker 插件（`@sekai/dsh-plugin/worker`）

| config 键 | 类型 | 默认 | 说明 |
| --- | --- | --- | --- |
| `llm` | `'dsh'\|'router'\|'off'` | `'dsh'` | spawn 时映射为 `--llm`；`dsh` 由插件中转 prompt |
| `promptAddr` | string | `'127.0.0.1:7422'` | `router` 模式传 `--prompt-addr`；`dsh` 模式为中转目标 |
| `promptTimeoutMs` | integer ≥1000 | `300000` | 中转等待响应的本地超时，超时回 `timeout` 信封 |
| `gpio` | string\|null | `null` | 非 null 时传 `--gpio <family>`；旧键 `family` 兼容映射到本键 |

全量键表与中转时序见 `plugins/sekai-dsh/README.md`。
