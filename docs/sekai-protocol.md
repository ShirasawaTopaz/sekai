# Sekai 控制平面协议（V1）

本文描述 Sekai Worker 与 Router 之间的控制平面协议 V1：协议版本字节为 `1`，Router 下发命令使用 **Stdin** 帧，Worker 上报状态与命令事件使用 **Stdout** 帧。

- **读者**：固件实现者、上位机/服务端开发者、需要调试 Worker 链路的工程师。
- **实现基线**：`crate/sekai/src/sekai_wire.rs`、`sekai_stdin.rs`、`sekai_stdout.rs`、`sekai_device_list.rs`、`sekai_runtime.rs`、GPIO 用户工具 `crate/sekai-gpio/`，以及启动层 `crate/sekai-worker/`（PC Worker）与 `crate/sekai-embedded/`（嵌入式 Worker，优先级置后）。文中所有常量、编号、字节布局均以这些源码为准。
- **约定**：本文中的“必须/不得”表示协议硬性要求；“应该/建议”表示实现应遵循的约定，违背时可能影响互通；“可以”表示实现自由选择。

---

## 0. 术语

### 0.1 术语与适用范围

本协议 V1 帧格式与主题布局不变。承载方式的适用范围：

- **MQTT 主题承载**仅适用于嵌入式 Worker 链路（后期阶段）。
- **PC Worker 直连链路**（stdio/TCP + 长度前缀帧，见 `crate/sekai-worker`）复用同一套 Stdin/Stdout 帧，不经 MQTT broker。
- Jump/DeviceList 等 MQTT 特有命令仅在有主题路由的链路上有意义。

**prompt 通道（带外，不改变本协议）**：Router 的 prompt ingress（TCP JSONL）与
Worker `pc.ask` 使用的 JSONL 信封是**带外传输层能力**——它只出现在 ingress 连接与
Worker↔DSH 插件的 stdin/stdout 上，不新增帧类型、错误码或 `content_type`，V1
Stdin/Stdout 帧一字不变。全量定义见 [sekai-pc-agent.md](sekai-pc-agent.md) §4。

### 0.2 术语表

| 术语 | 含义 |
| --- | --- |
| 控制平面 | Router 向 Worker 下发命令、Worker 向 Router 上报结果的逻辑通道，与业务数据面无关。 |
| Stdin | Worker 视角的输入流：Router 写成 Stdin 帧，Worker 解码执行。 |
| Stdout | Worker 视角的输出流：Worker 写成 Stdout 帧，Router 消费。 |
| 命令（Command） | 一条 Stdin 帧，对应唯一的 `cmd_id`。命令分三类：Task、ToolCall、Jump。 |
| Task | Worker 端可被多次推进的协作式长任务，有独立 DDL。 |
| ToolCall | Worker 端同步、短小的能力调用（如查询时间、读 GPIO）。 |
| Jump | 把 Worker 的控制链路迁移到另一个 MQTT 端点（另一个 Worker/网关/Router）。 |
| boot_id | Worker 本次启动周期的非零标识，跨重启必须变化。 |
| seq | Worker 本次启动内所有 Stdout 帧的递增序号，用于 Router 去重。 |
| DDL | 命令的执行预算（毫秒）。Task 从开始执行时计时，Tool 为单次调用预算。 |
| LWT | MQTT 遗嘱消息（Last Will and Testament），用于 Worker 异常掉线时上报 Offline。 |
| DeviceList | Worker 内置的跳转目标列表（最多 8 项），Jump 按名称/IP 解析目标。 |

---

## 1. 概念模型

### 1.1 两个逻辑控制流

控制平面只有两个方向的数据流，命名借用了进程模型：

```text
                       Stdin（Router → Worker，in 主题）
   ┌─────────┐  ───────────────────────────────────────▶  ┌─────────┐
   │  Router  │                                            │  Worker  │
   └─────────┘  ◀───────────────────────────────────────  └─────────┘
                       Stdout（Worker → Router）
                       ├─ out 主题：命令事件 Accepted/Started/Completed/Failed
                       └─ state 主题：Hello/Offline（retained）
```

- **Stdin 是逻辑输入流**：任何能向 `{root}/dev/{device_id}/in` 发布合法帧的主体，都在“写 Worker 的标准输入”。
- **Stdout 是逻辑输出流**：Worker 把命令事件发到 `out`，把状态通告发到 `state`。Router 订阅这两个主题来“读 Worker 的标准输出”。
- 两个流都是**帧流**：一条 MQTT PUBLISH 的载荷恰好是一帧，不跨消息分片，也不在一条消息里放多帧。

### 1.2 为什么不是 OS 文件描述符

Stdin/Stdout 只是沿用“控制台输入/输出”的心智模型，**不是**进程的 fd 0/1，也不要求 Worker 上有文件系统：

1. 两端通常在不同机器上，中间隔着 broker；通道由 MQTT 主题而不是内核管道提供。
2. 流的分界由 MQTT 消息边界定义，而不是字节流 EOF；帧内自带长度字段，便于严格解码。
3. MQTT 只是一种载体。协议只依赖“按主题发布有界二进制消息、QoS1 传输、可选 retained、MQTT 5 属性”这些能力；换成其他具备同等语义的可靠消息通道时，帧格式不变。

### 1.3 编码约定

所有多字节整数均为**大端**、无填充、连续排列：

| 类型 | 编码 |
| --- | --- |
| `u8` | 1 字节 |
| `u16` | 2 字节大端 |
| `u32` | 4 字节大端 |
| `u64` | 8 字节大端 |
| 字符串 | `u16` 字节长度 + UTF-8 字节（MQTT 风格，不允许 `U+0000`） |
| 二进制参数 | 原样字节，长度由外层字段或“剩余长度”决定 |

解码必须**恰好消费完整帧**：帧尾出现多余字节、或长度字段给出的范围超出实际数据，均视为非法帧。

### 1.4 协议版本

- 帧首字节固定为 `version = 1`。
- V1 不做版本协商：不为 1 的版本按解码错误处理。
- 新版本必须通过新的主题根或新的 `content_type` 与 V1 隔离，避免旧 Worker 误解析新帧。

---

## 2. 传输层：MQTT 5

控制平面使用 **MQTT 5.0**。V1 依赖以下 MQTT 5 特性：`content_type` 属性、`session_expiry_interval`、`server_reference`。MQTT 3.1.1 broker 无法完整承载本协议。

### 2.1 主题

主题根 `{root}` 由部署配置（参考实现默认 `sekai/v1`），Worker id 为 `{device_id}`。主题均以 Worker 为视角命名：

| 主题 | 方向 | 发布者 | 订阅者 | 载荷 | QoS | retained |
| --- | --- | --- | --- | --- | --- | --- |
| `{root}/dev/{device_id}/in` | Router → Worker | Router | Worker | 一帧 Stdin | 1 | 否 |
| `{root}/dev/{device_id}/out` | Worker → Router | Worker | Router | 一帧命令类 Stdout | 1 | 否 |
| `{root}/dev/{device_id}/state` | Worker → Router | Worker | Router | 一帧 Hello/Offline Stdout | 1 | **是** |

示例（默认根、Worker id `demo`）：

```text
sekai/v1/dev/demo/in
sekai/v1/dev/demo/out
sekai/v1/dev/demo/state
```

服务端最小订阅集合：

```text
{root}/dev/+/out      # 所有 Worker 的命令事件
{root}/dev/+/state    # 所有 Worker 的状态（首帧为 retained 的 Hello 或 Offline）
```

也可以按 Worker 精确订阅。`in` 主题只由 Router 发布，Worker 仅订阅。

### 2.2 QoS 与 retain

- **QoS 1（至少一次）是协议要求**：Router 发布 Stdin、Worker 发布 Stdout 都使用 QoS1。
- 重复投递由协议层兜底：Router 按 `(device_id, boot_id, seq)` 去重 Stdout；Worker 按 `cmd_id` 去重 Stdin（见 §4.4、§7.1）。
- **`state` 主题必须 retained**：新订阅者应立即读到 Worker 最新 Hello 或 Offline。Worker 在 CONNACK 后发布 retained Hello；异常掉线时由 LWT 发布 retained Offline。
- **`in`、`out` 主题不得 retained**：命令是瞬时事件，保留命令会在 Worker 重连/重新订阅时被再次投递，可能造成重复执行（Worker 只能靠有窗口的去重缓存缓解）。参考运行时对 `retain=1` 的入站 Stdin 直接丢弃并计入拒绝计数。
- 若 broker 声明 `retain_available = false`，参考实现会退化为把 Hello/Offline 发布到 `out` 主题且不 retain；这属于降级模式，Router 应修复 broker 配置。
- 若 broker 声明的最大 QoS 小于 1，参考实现会把该发布降级为 QoS0；此时不保证送达与去重，Router 应修复 broker 配置。

### 2.3 `content_type`

| 方向 | 主题 | `content_type` |
| --- | --- | --- |
| Router → Worker | `in` | `application/vnd.sekai.stdin.v1` |
| Worker → Router | `out`、`state`（含 LWT Offline） | `application/vnd.sekai.stdout.v1` |

- Worker**应该**在发布 Stdout 时携带 `content_type`。
- Worker 接收 Stdin 时：`content_type` **缺失则接受**；存在但不等于 `application/vnd.sekai.stdin.v1` 时，回 `Failed(0x0001 InvalidCommand)` 并丢弃该命令。
- Router 可以用 `content_type` 过滤无关消息，但不应要求 Worker 一定携带。

### 2.4 MQTT 会话与连接参数（参考实现默认值）

| 参数 | 默认值 | 说明 |
| --- | --- | --- |
| Client ID | `device_id` | 运行时强制使用 Worker id，ACL 可按此识别 Worker。 |
| Clean Start | `false` | 复用服务端会话。 |
| Session Expiry Interval | `300` 秒 | 普通断线后会话保留 5 分钟。 |
| Keep Alive | `30` 秒 | |
| CONNECT 超时 | 5000 ms | |
| Receive Maximum | `8` | Worker 可同时接收的未确认 QoS>0 消息数。 |
| Maximum Packet Size | `16384` 字节 | 覆盖单帧最大尺寸（见 §9.4）。 |

- Worker 在每次 CONNACK 后**都会**发布一次 retained Hello；只在 `session_present == false` 时重新 SUBSCRIBE `in` 主题。
- Worker 对 `in` 的订阅选项：QoS1、`no_local = true`、`retain_as_published = false`、`retain_handling = 0`。即订阅时立即接收 retained 消息——这也是 `in` 禁止 retained 的原因。

### 2.5 Jump 与 Router 迁移用到的 MQTT 语义

- **Jump**：Worker 在切换前向旧 broker 发送 `DISCONNECT`，原因码 `0x00 Success`，属性 `session_expiry_interval = 0`，使旧 broker 立即丢弃该 Worker 会话，不再向旧链路补发消息；随后关闭 TCP，连接目标端点。
- **Router 迁移**：broker 可向 Worker 下发 `DISCONNECT`，原因码 `0x9C Use Another Server` 或 `0x9D Server Moved`，并携带 `server_reference` 属性；Worker 据此切换到新 Router（见 §7.3.4）。

### 2.6 主题根约束（参考实现）

- 非空、≤128 字节、不以 `/` 开头或结尾、不含 `+`、`#`、`NUL`。
- 建议形如 `sekai/v1`，为多租户/多环境时可在前缀加命名空间，例如 `factory-a/sekai/v1`。

---

## 3. Stdin 帧

### 3.1 帧格式

固定头 **30 字节**，大端：

| 偏移 | 字段 | 类型 | 说明 |
| --- | --- | --- | --- |
| 0 | `version` | `u8` | 必须为 `1`。 |
| 1 | `kind` | `u8` | 命令类型，见 §3.3。 |
| 2 | `id` | `u64` | 命令 id，由 Router 按 Worker 单调分配，**不得为 0**。 |
| 10 | `timestamp` | `u64` | Router Unix 毫秒；`0` 表示未知。 |
| 18 | `delay_ms` | `u32` | 相对 Worker 收到时刻的延迟毫秒；`0` 立即执行。上限 86 400 000（24 小时）。 |
| 22 | `ddl_ms` | `u32` | 执行预算毫秒；`0` 表示使用 Worker 默认值（参考实现 500 ms）。上限 300 000（5 分钟）。 |
| 26 | `payload_len` | `u32` | 载荷字节数，范围 `1..=4096`（Stdin 载荷不得为空）。 |
| 30 | `payload` | `[u8]` | 类型相关载荷，布局见 §5。 |

一帧总长度 = `30 + payload_len`，最大 4126 字节。帧必须恰好占满一条 MQTT PUBLISH 的载荷。

### 3.2 字段语义

- **id**：命令的唯一标识。Router 对同一 Worker 应严格递增且不复用，直到确定旧命令不会重试为止。Worker 用做去重键和事件关联键。
- **timestamp**：Router 当前 Unix 毫秒，用于让 Worker 做命令时效检查。`0` 表示 Router 未提供时间，Worker 跳过时效检查。Router 应该总是填写。
- **delay_ms**：相对于**Worker 收到该帧的单调时刻**的延迟，不是相对于 `timestamp`。延迟期间命令处于等待队列。
- **ddl_ms**：执行预算。`0` 取 Worker 默认值；Task 从 `Started` 时刻开始计时，Tool 为调用预算，Jump 不使用该值（但仍校验上限）。
- **payload**：不能为空。按 `kind` 解释为 TaskBody、ToolBody 或 JumpBody。

### 3.3 Kind 编号

| 值 | 名称 | 含义 | 载荷类型 |
| --- | --- | --- | --- |
| 1 | `Task` | 下发任务，Worker 异步执行为状态机 | TaskBody |
| 2 | `ToolCall` | 同步调用工具，尽快返回结果 | ToolBody |
| 3 | `Jump` | 迁移控制链路到目标端点 | JumpBody |

其他取值均为解码错误。

### 3.4 解码与校验规则

Worker 按顺序执行以下校验，任一失败即拒绝该帧：

1. `version == 1`，否则版本不支持。
2. `kind` 属于上表。
3. `id != 0`。
4. `delay_ms <= 86 400 000`。
5. `ddl_ms <= 300 000`。
6. `payload_len` 不超过 4096，且实际剩余字节足够。
7. 帧尾**没有**多余字节。
8. `payload` 非空，且能按对应 Body 布局完整解码：名称合法、长度自洽；JumpBody 还要求恰好消费完载荷，Task/ToolBody 的剩余字节按 args 处理。

Worker 实际可接受的 `delay_ms`/`ddl_ms` 上限还可以小于协议上限（由 Worker 配置决定），超限时回 `InvalidArgument`。

### 3.5 解码错误如何上报

命令解码失败时，如果头部已经能解析出**非 0 的 `cmd_id`**，Worker 回一帧 `Failed(0x0001 InvalidCommand)`，消息字段为具体错误；否则只能丢弃，Router 表现为“没有响应”。

注意两条因字段顺序导致的行为：

- `version`（偏移 0）和 `kind`（偏移 1）位于 `id`（偏移 2）之前，因此**未知版本或未知 kind 无法得到 `cmd_id`**，Worker 静默丢弃，不会回 `Failed`。Router 排查“无响应”时应先检查这两个字节。
- `id` 已解析但 `payload_len` 截断/超限时，Worker 仍能回 `Failed(0x0001)`。
- 解析阶段产生的 `Failed`（含 Task/Tool/Jump body 解码失败、`content_type` 不匹配）也会记入去重窗口/最终缓存；Router 重试同一帧会得到同一 `Failed`（相同 `seq`），不会产生第二个最终事件。

---

## 4. Stdout 帧

### 4.1 帧格式

固定头 **34 字节**，大端：

| 偏移 | 字段 | 类型 | 说明 |
| --- | --- | --- | --- |
| 0 | `version` | `u8` | 必须为 `1`。 |
| 1 | `phase` | `u8` | 事件阶段，见 §4.2。 |
| 2 | `boot_id` | `u64` | 本启动周期 id，**不得为 0**。 |
| 10 | `cmd_id` | `u64` | 对应 Stdin 的 `id`；Hello/Offline 固定为 `0`。 |
| 18 | `seq` | `u32` | 本 boot 内递增序号；LWT Offline 为 `0`。 |
| 22 | `timestamp` | `u64` | Worker Unix 毫秒；未校准为 `0`。 |
| 30 | `payload_len` | `u32` | 载荷字节数，`0..=4096`。 |
| 34 | `payload` | `[u8]` | 类型相关载荷，布局见 §5。 |

一帧总长度 = `34 + payload_len`，最大 4130 字节。

### 4.2 Phase 编号

| 值 | 名称 | 是否最终 | 载荷 | 用途 |
| --- | --- | --- | --- | --- |
| 1 | `Accepted` | 否 | 空 | 命令已校验并进入等待/执行流程。 |
| 2 | `Started` | 否 | 空 | Task 首次开始推进（仅 Task 会发送）。 |
| 3 | `Progress` | 否 | 未定义 | **V1 保留，不发送**；接收方应忽略。 |
| 4 | `Completed` | **是** | 结果字节或 JumpOutcome | 成功结束。 |
| 5 | `Failed` | **是** | ErrorPayload | 失败结束。 |
| 6 | `Hello` | 不适用 | Hello | 能力/状态通告，发布到 `state` 且 retained。 |
| 7 | `Offline` | 不适用 | 空 | 离线状态，发布到 `state` 且 retained。 |

### 4.3 `cmd_id` 规则

| 帧 | `cmd_id` |
| --- | --- |
| `Accepted`/`Started`/`Completed`/`Failed` | 必须等于对应 Stdin 的 `id`，**不得为 0** |
| `Hello`/`Offline` | 必须为 `0` |

Router 按 `cmd_id` 关联请求与事件。一条命令的 `Accepted`（可选的 `Started`）和最终事件共享同一 `cmd_id`。

### 4.4 `seq` 与去重键

- Worker 每次发布 Stdout 帧都分配 `seq`，从 `1` 开始按本次启动周期递增；`Hello`、正常 `Offline`、以及所有命令事件都消耗一个 `seq`。
- 递增到 `u32::MAX` 后回绕到 `1`（跳过 0）；实际部署中几乎不会触发。
- **LWT Offline 是唯一例外：`seq = 0`**，因为遗嘱消息在 CONNECT 时就要固定载荷。
- 去重键是 **`(device_id, boot_id, seq)`**。`device_id` 不在帧内，Router 从主题或 Hello 获得。
- Router 必须对重复投递做幂等处理：同一去重键只处理一次。重复可能来自 QoS1 重传、Worker 发件箱重放、或同一条事件被多个订阅路径收到。

Router 处理事件时还要注意：

- `seq` 是**全局**递增，不是按 `cmd_id` 递增。不同命令的事件会交错，例如命令 A 的 `Started` 可能出现在命令 B 的 `Accepted` 之后。
- 因此 Router 判断“命令完成”必须以 `cmd_id` 关联为准，不能依赖相邻性。
- `seq == 0` 只允许出现在 `Offline`（LWT）。收到其他阶段的 `seq == 0` 应视为非法帧。

### 4.5 `boot_id` 与时间戳

- `boot_id` 非 0，且**每次重启必须不同**。Router 用 `(device_id, boot_id, seq)` 隔离不同启动周期的事件流；新 `boot_id` 出现表示 Worker 已重启。
- Worker 重启后 `seq` 从 1 重新开始，`out` 中未确认的旧事件随内存丢失。
- `timestamp` 是 Worker 墙上时间（Unix 毫秒）。Worker 未校准时为 `0`；Router 不能把 `0` 当成 1970 年参与业务计算。

### 4.6 解码与校验规则

接收方（Router）解析 Stdout 时应执行：

1. `version == 1`。
2. `phase` 属于 §4.2。
3. `boot_id != 0`。
4. Hello/Offline 的 `cmd_id == 0`；其他阶段的 `cmd_id != 0`。
5. Offline 的 `payload` 必须为空。
6. 除 LWT Offline 外 `seq != 0`。
7. `payload_len <= 4096` 且数据完整、帧尾无多余字节。
8. 按阶段解码载荷；Hello 必须能完整解码为 §5.6 的布局。

### 4.7 最终事件

- 只有 `Completed` 和 `Failed` 是最终事件。Router 不得把 `Accepted` 当作完成。
- 一条命令至多产生一个最终事件（Worker 端有最终事件缓存，重复命令会重放同一个最终帧）。
- Router 应设置合理的等待超时：至少覆盖 `delay_ms + ddl_ms`；Jump 还要覆盖排空与重连时间（见 §8.5）。

---

## 5. 载荷布局

### 5.1 通用约定

- 字符串一律为 `u16` 长度 + UTF-8 字节，且不允许 `U+0000`。
- 长度前缀指**字节数**，不是字符数。
- 参数（args）是“剩余的全部字节”，其内部格式由任务/工具自行解释；协议不解释。
- 所有“列表”都有上限；解析出超限列表即非法帧。

### 5.2 TaskBody / ToolBody

`Kind::Task` 与 `Kind::ToolCall` 共用同一种载荷布局：

| 偏移（相对载荷） | 字段 | 类型 | 说明 |
| --- | --- | --- | --- |
| 0 | `name_len` | `u16` | 名称字节数，`1..=48` |
| 2 | `name` | `[u8]` | UTF-8 名称，规则见 §6.1 |
| 2+name_len | `args` | `[u8]` | 剩余字节，可为空 |

示例：名称 `echo`、参数 `hello`：

```text
00 04 65 63 68 6F 68 65 6C 6C 6F
└─┬─┘ └──── name ────┘ └── args ──┘
 4       echo            hello
```

载荷总长 11 字节。

### 5.3 JumpBody

| 偏移 | 字段 | 类型 | 说明 |
| --- | --- | --- | --- |
| 0 | `target_len` | `u16` | 目标字节数，`1..=96` |
| 2 | `target` | `[u8]` | 目标字符串，规则见 §6.1 |

`target` 可以是 DeviceList 中的 Worker 名、DeviceList 中的 IP/主机名，或在 Worker 允许时是显式的 `host:port`；解析规则见 §5.6.2。

### 5.4 ErrorPayload（Failed 载荷）

| 偏移 | 字段 | 类型 | 说明 |
| --- | --- | --- | --- |
| 0 | `code` | `u16` | 错误码，见 §6.3 |
| 2 | `message_len` | `u16` | 消息字节数，`0..=256` |
| 4 | `message` | `[u8]` | UTF-8 可读描述 |

编码规则：

- 消息超过 256 字节时按 UTF-8 字符边界截断到不超过 256 字节。
- 消息中的 `NUL`（`U+0000`）在编码前替换为空格，保证载荷是合法的 MQTT UTF-8 字符串。
- 错误码必须是 §6.3 中的已知值。

示例：`Timeout`（0x0008）+ 消息“超时”：

```text
00 08 00 06 E8 B6 85 E6 97 B6
└─┬─┘ └─┬─┘ └──── 超时 ────┘
code  len=6
```

### 5.5 JumpOutcome（Jump 成功时的 Completed 载荷）

| 偏移 | 字段 | 类型 | 说明 |
| --- | --- | --- | --- |
| 0 | `host_len` | `u16` | 主机名/地址字节数，`1..=64` |
| 2 | `host` | `[u8]` | 迁移后的目标主机 |
| 2+host_len | `port` | `u16` | 目标端口，`1..=65535` |

示例：`peer-1:1883`：

```text
00 06 70 65 65 72 2D 31 07 5B
└─┬─┘ └──── peer-1 ────┘ └─┬─┘
len=6                     1883
```

### 5.6 Hello 载荷

Hello 由 Worker 在每次 CONNACK 后发布到 `state`（retained），也可通过内置工具 `sekai.info` 单独查询。

编码顺序（变长，无固定偏移）：

| 顺序 | 字段 | 类型 | 说明 |
| --- | --- | --- | --- |
| 1 | `device_id` | 字符串 | Worker id，规则见 §6.1 |
| 2 | `firmware` | 字符串 | 固件版本，≤64 字节 |
| 3 | `uptime_ms` | `u64` | Worker 本次启动以来的毫秒数（单调） |
| 4 | `unix_ms` | `u64` | Worker Unix 毫秒；未校准为 0 |
| 5 | `task_count` | `u16` | 已注册任务数，≤16 |
| 6 | `task_names` | 字符串 × `task_count` | 任务名列表，参考实现按原名排序 |
| 7 | `tool_count` | `u16` | 可用工具数，≤16 |
| 8 | `tool_names` | 字符串 × `tool_count` | 工具名列表，**包含内置工具**，参考实现排序去重 |
| 9 | `workers` | DeviceList | 跳转目标列表（代码字段名 `workers`），见 §5.6.1 |
| 10 | `active_count` | `u16` | 运行中的 Task 数，≤4 |
| 11 | `active_tasks` | ActiveTask × `active_count` | 见 §5.6.1 |

字段约束：

- `device_id` 必须合法；`firmware` 允许为空串但参考实现要求非空，长度 ≤64。
- 每个任务名/工具名必须是合法名称；`task_count <= 16`，`tool_count <= 16`。
- `tools` 必须包含平台内置工具名（`sekai.` 前缀）和用户工具名；用户在 Hello 里看到的工具就是 Worker 实际可调用的集合。
- `active_tasks` 只包含**正在运行**的 Task（不含等待队列、不含 Tool）；最多 4 条，`cmd_id != 0`，`name` 合法。
- `uptime_ms` 用单调时钟计算，不受时间校准影响。
- `unix_ms` 与 Stdout 的 `timestamp` 一样，未校准时为 0。

#### 5.6.1 DeviceList 与 ActiveTask

**DeviceList**（也可作为 `sekai.workers` 工具结果、`sekai.workers.set` 工具参数单独传输）：

| 顺序 | 字段 | 类型 | 说明 |
| --- | --- | --- | --- |
| 1 | `count` | `u16` | 条目数，`0..=8` |
| 2 | 条目 × count | 见下 | 名称在列表内必须唯一 |
| 2.1 | `name` | 字符串 | 逻辑名，规则同 Worker id |
| 2.2 | `ip` | 字符串 | IPv4 地址或主机名，≤64 字节，v1 不支持 IPv6 字面量 |
| 2.3 | `port` | `u16` | TCP 端口，`1..=65535` |

**ActiveTask**：

| 顺序 | 字段 | 类型 | 说明 |
| --- | --- | --- | --- |
| 1 | `cmd_id` | `u64` | 对应 Stdin 的命令 id，非 0 |
| 2 | `name` | 字符串 | 任务名 |
| 3 | `elapsed_ms` | `u64` | 从开始执行到生成 Hello 时的毫秒数 |

#### 5.6.2 Jump 目标解析规则

Worker 执行 Jump 时按以下顺序解析 `JumpBody.target`（参考实现语义）：

1. 与 DeviceList 中的 `name` 精确匹配 → 使用该条目的 `ip:port`。
2. 与 DeviceList 中的 `ip` 精确匹配 → 使用该条目的 `port`。
3. 若 Worker 允许任意目标（参考实现默认**关闭**），按 `host:port` 解析；允许可选 `mqtt://` / `tcp://` 前缀，不接受缺少端口、端口为 0 或 IPv6 字面量（方括号形式）。
4. 以上都不匹配 → `Failed(0x000B JumpInvalidTarget)`。

目标解析结果若与当前链路端点相同，同样回 `JumpInvalidTarget`。

### 5.7 Offline 载荷

Offline 的 `payload` 必须为空（`payload_len = 0`）。它只有帧头，含义由阶段表达。

- **正常 Offline**：Worker 开始 Jump 时主动发布，`seq > 0`、`cmd_id = 0`、retained。
- **LWT Offline**：Worker 异常掉线时由 broker 代发，`seq = 0`、`cmd_id = 0`、retained。载荷在运行时构造/配置 CONNECT 时固定，`timestamp` 取自那一刻的 Worker 时间（未校准为 0）。

### 5.8 各阶段载荷语义汇总

| Phase | `cmd_id` | `seq` | 载荷 | Router 处理 |
| --- | --- | --- | --- | --- |
| Accepted | 命令 id | >0 | 空 | 标记命令已受理；**不是最终结果** |
| Started | 命令 id | >0 | 空 | 可更新“开始执行”状态 |
| Progress | 命令 id | >0 | V1 未定义 | 忽略；V1 Worker 不会发送 |
| Completed | 命令 id | >0 | Task/Tool 结果原始字节；Jump 为 JumpOutcome | 命令成功结束 |
| Failed | 命令 id | >0 | ErrorPayload | 命令失败结束 |
| Hello | 0 | >0 | Hello | 更新 Worker 能力/在线状态（retained） |
| Offline | 0 | 正常 >0；LWT 为 0 | 空 | Worker 离线（retained） |

说明：

- Task/Tool 的 `Completed` 载荷完全由 Worker 侧处理器返回，协议不解释；长度上限 4096 字节，允许为空。
- Jump 的 `Completed` 载荷固定为 JumpOutcome，标识迁移后的端点。
- `Failed` 载荷固定为 ErrorPayload。

---

## 6. 名称、标识与错误码

### 6.1 名称与标识规则

| 对象 | 规则 | 上限 |
| --- | --- | --- |
| 任务名 / 工具名 | `[a-z0-9._-]`（ASCII 小写字母、数字、点、下划线、减号），非空 | 48 字节 |
| Worker id（`device_id`） | `[A-Za-z0-9._-]`，非空 | 64 字节 |
| 固件版本 `firmware` | 任意 UTF-8 | 64 字节 |
| 主机名 / IPv4 `host` | `[A-Za-z0-9._-]`，非空；**不接受 IPv6 字面量** | 64 字节 |
| Jump 目标字符串 | 非空，不含空白、控制字符、NUL | 96 字节 |
| 端口 | `1..=65535` | — |
| Worker 列表（DeviceList） | 名称唯一 | 8 项 |
| `cmd_id` / `boot_id` | 非 0 | — |

**保留前缀**：以 `sekai.` 开头的任务名/工具名保留给平台内置能力。用户注册表会拒绝该前缀，内置工具也不允许被用户覆盖。

### 6.2 编码/解码错误分层

- **帧级**：版本、kind/phase、长度、尾随字节等，对应 `ProtocolError`。
- **字段级**：名称非法、目标含空白、端口为 0、列表超限等，对应 `InvalidField`。
- **发送侧**：字段过长、编码后超过 4096 字节等，对应 `ProtocolEncodeError`。
- 这些错误不会直接出现在线路上；Worker 把它们转换为 `Failed` 事件中的错误码（见 §6.3）或丢弃非法入站帧。

### 6.3 错误码表（Failed 载荷）

| 值 | 名称 | 含义 | Worker 侧典型触发 |
| --- | --- | --- | --- |
| 0x0001 | `InvalidCommand` | 命令帧非法 | Stdin 解码失败但可解析出非 0 id；`content_type` 不匹配；`timestamp` 比 Worker 时间超前超过 300 秒。 |
| 0x0002 | `UnsupportedVersion` | 帧版本不支持 | **协议保留**。当前 Worker 端不产生（版本在 id 之前，解码失败时通常无 id 可回填），Router 应把解码错误本身当作信号。 |
| 0x0003 | `InvalidArgument` | 载荷/名称/参数非法 | TaskBody/ToolBody/JumpBody 解码失败；`delay_ms`/`ddl_ms` 超过 Worker 允许上限；内置工具参数不合法（长度/取值）。 |
| 0x0004 | `Expired` | 命令已过期 | 命令 `timestamp` 非 0、Worker 时钟已校准，且 `now - timestamp > max_command_age_ms`（参考实现默认 300 秒）。 |
| 0x0005 | `Busy` | Worker 忙 | 收件箱满；等待队列满；已有 Jump 进行中（含等待队列中的 Jump）；当前没有可用链路。 |
| 0x0006 | `UnknownTask` | 任务未注册 | Task 名称不在已注册任务表中。 |
| 0x0007 | `UnknownTool` | 工具未注册 | ToolCall 名称既不是内置工具，也不在用户工具表中。 |
| 0x0008 | `Timeout` | 执行超时 | Task 超过 DDL；Tool 单次调用耗时达到 DDL。 |
| 0x0009 | `ResultTooLarge` | 结果过大 | Task/Tool 结果超过 4096 字节。 |
| 0x000A | `Cancelled` | 命令被取消 | 通过内置工具 `sekai.cancel` 取消了等待中或运行中的命令（该错误发给**被取消的命令**）。 |
| 0x000B | `JumpInvalidTarget` | 跳转目标非法 | 目标不在 DeviceList 且不允许任意目标；目标格式非法；目标与当前链路相同。 |
| 0x000C | `JumpUnreachable` | 跳转目标不可达 | 新链路 TCP 连接失败、CONNACK 失败、或切换过程中断链；Worker 已回退到原链路。 |
| 0x000D | `Internal` | 内部错误 | Worker 内部构造 Hello/DeviceList/事件失败等异常情况；不表示协议语义问题。 |
| 0x000E | `Unsupported` | 支持的能力/版本不足 | **协议保留**。当前 Worker 端不产生，供未来扩展使用。 |

补充：

- 错误码是 `u16`，未列出的值在解码时视为非法 ErrorPayload。
- `message` 是给人看的诊断信息，不保证稳定；程序判断必须使用 `code`。
- `Busy`/`Expired`/`InvalidArgument` 等发生在命令被接受之前时，Worker**只发 Failed，不发 Accepted**；Router 必须接受“没有 Accepted 直接 Failed”的事件序列。

---

## 7. 命令生命周期

### 7.1 Task 生命周期

```text
Router                                      Worker
  │  Stdin(kind=Task, id=N)  ───────────────▶  │
  │                                            │ 解码/校验/去重/名称/队列
  │  ◀─────────────  Accepted(cmd_id=N)       │ 立即（接受时）
  │                                            │ 等待 delay_ms
  │  ◀─────────────  Started(cmd_id=N)        │ 开始执行（创建 handler）
  │                                            │ 协作式 poll（受 DDL 约束）
  │  ◀─────────────  Completed(cmd_id=N)      │ 成功（载荷=结果字节）
  │            或  Failed(cmd_id=N)           │ 失败/超时/结果过大
```

关键规则：

1. **接受**：Worker 解析并做基础校验（去重、时效、DDL/延迟上限、任务是否注册、队列是否已满）。通过后立即可发 `Accepted`，然后按 `start_at = 收到时刻 + delay_ms` 放入等待队列。等待队列按 `(start_at, cmd_id)` 排序。
2. **等待**：`delay_ms = 0` 也要走一次调度，但会在同一次推进中启动；`Accepted` 一定先于 `Started`。
3. **开始**：调用任务工厂创建处理器。工厂失败直接 `Failed`；创建成功立即发 `Started`，DDL 从这一刻开始计算。
4. **推进**：处理器在每次 Worker 主循环中被 poll 一次，返回 `Pending{wake_after_ms}` 让出 CPU，运行时把下次 poll 时间截到 DDL 内；返回 `Done(result)` 发 `Completed`（结果超过 4096 字节改为 `Failed(ResultTooLarge)`）；返回 `Failed(error)` 发 `Failed`。
5. **超时**：在 poll 前后都检查 DDL（`start + ddl_ms`），到点未完成则发 `Failed(Timeout)` 并丢弃任务。由于是协作式调度，**处理器不得长时间阻塞**：DDL 检查发生在 poll 返回之后，无法打断一次正在执行的 poll。
6. **并发上限**：参考实现默认最多 4 个 Task 同时运行。达到上限时等待队列中的 Task 不启动，也不会失败，直到有 Task 结束。
7. **取消**：V1 **没有专门的取消命令帧**。使用内置工具 `sekai.cancel`（参数为 8 字节大端命令 id）：
   - 若目标命令在等待队列或运行中：Worker 向目标命令发 `Failed(Cancelled)`，并从队列移除；
   - `sekai.cancel` 本身返回空结果的 `Completed`；
   - 目标不存在或已结束：`sekai.cancel` 返回 `Failed(InvalidArgument)`。
8. **重复与重试**：Router 重试必须发送**逐字节相同的帧**（尤其 `cmd_id` 不变）。Worker 按 `cmd_id` 去重：
   - 命令仍在等待/运行：忽略重复帧，不重复回事件（Router 应继续等待最终事件）；
   - 命令已结束且最终帧仍在缓存：重放**同一个最终帧**（相同 `seq`），不重新执行；
   - 最终缓存已淘汰但 id 仍在去重窗口：静默丢弃；
   - 去重窗口、活跃表、最终缓存都没有该 id：视为新命令，可能重复执行（见 §7.1 队列表）。

   注意：即使 id 已滑出去重窗口，只要命令仍在运行或其最终帧仍在缓存，仍按前两条去重；这比单纯“最近 N 个 id”更强。
9. **结果与事件上限**：Task 结果 ≤4096 字节。通过接受校验的命令，事件序列为 `Accepted` →（Task 可能还有 `Started`）→ 一个最终事件；在接受校验阶段就被拒绝的命令只产生 `Failed`，没有 `Accepted`。

队列与去重窗口（参考实现默认值，均可配置）：

| 项 | 默认值 | 超出时行为 |
| --- | --- | --- |
| 收件箱（已收未处理命令） | 4 | `Failed(Busy, "收件箱已满")` |
| 等待队列（延迟/待启动命令） | 4 | `Failed(Busy, "等待队列已满")` |
| 同时运行 Task | 4 | 等待，不失败 |
| 最近命令 id 去重窗口 | 32 | 窗口外且不在运行/最终缓存 → 视为新命令（可能重复执行） |
| 最终事件缓存 | 8 条 / 4096 字节 | 被淘汰后重试无响应 |

### 7.2 Tool 生命周期

```text
Router                                      Worker
  │  Stdin(kind=ToolCall, id=N) ─────────────▶ │
  │  ◀─────────────  Accepted(cmd_id=N)       │ 立即
  │                                            │ 等待 delay_ms 后同步调用
  │  ◀─────────────  Completed(cmd_id=N)      │ 成功（载荷=调用结果）
  │            或  Failed(cmd_id=N)           │ 失败/超时/结果过大
```

关键规则：

1. Tool 是**同步、短小**的调用，不产生 `Started` 事件。
2. 延迟结束后在 Worker 主循环中**一次性**执行完（同一轮 poll 内），结果立即转成 `Completed`/`Failed`。
3. DDL 为单次调用预算：运行时在调用返回后比较耗时，`elapsed >= ddl_ms` 时回 `Timeout`。**运行时无法抢占阻塞中的工具**，因此工具处理器必须快速返回；耗时工作应实现为 Task。
4. 结果超过 4096 字节 → `Failed(ResultTooLarge)`。
5. 重复命令按 `cmd_id` 去重，规则与 Task 相同。

#### 7.2.1 内置工具

内置工具由运行时直接处理，不进入用户工具注册表，但会在 Hello 的 `tools` 中通告；名称使用保留前缀 `sekai.`。

| 工具名 | 参数 | 成功结果（Completed 载荷） | 失败场景 |
| --- | --- | --- | --- |
| `sekai.time` | 忽略 | 16 字节：`u64` 当前 Unix 毫秒（未校准为 0）+ `u64` 当前单调毫秒 | — |
| `sekai.set_time` | 8 字节 `u64` 大端 Unix 毫秒，范围 `[1_600_000_000_000, 4_000_000_000_000]` | 8 字节 `u64` 大端：校准前的 Worker Unix 毫秒（未知为 0） | 长度不是 8 → `InvalidArgument`；时间超范围 → `InvalidArgument` |
| `sekai.info` | 忽略 | Hello 载荷（同 §5.6） | 内部编码失败 → `Internal` |
| `sekai.workers` | 忽略 | DeviceList 载荷（同 §5.6.1） | 内部编码失败 → `Internal` |
| `sekai.workers.set` | DeviceList 载荷 | 空 | 参数解码失败 → `InvalidArgument`；设置成功后额外发布 retained Hello |
| `sekai.cancel` | 8 字节 `u64` 大端命令 id | 空 | 长度不是 8 或目标命令不存在 → `InvalidArgument` |

补充：

- `sekai.set_time` 是**Worker 唯一的对时入口**：Worker 时钟只在该工具被调用时校准。Router 应使用可信时间源调用它，并注意校准会让 Worker 时间向前或向后跳变。
- `sekai.workers.set` 会替换整个 DeviceList，成功后 Worker 立即用新列表构造 Hello 并发布到 `state`（因此 Router 会先看到一帧 Hello，再看到该工具的 `Completed`）。
- 参考实现中用户工具数量受总上限约束：6 个内置工具 + 用户工具 ≤ 16，即用户最多注册 10 个工具。
- 内置工具名不能被用户工具覆盖；用户注册 `sekai.` 前缀会失败。

### 7.3 Jump 生命周期

Jump 把**Worker 自身的 MQTT 控制链路**迁移到另一个端点，不迁移业务数据，也不把 Task 移交给另一个 Worker。

#### 7.3.1 触发与校验

Router 发送 `Stdin(kind=Jump, payload=JumpBody{target})`。Worker 在接受阶段检查：

- 当前没有进行中的 Jump，等待队列里也没有 Jump —— 否则 `Busy`；
- 当前链路或最近可用的正常链路存在 —— 否则 `Busy`；
- `target` 能按 §5.6.2 解析 —— 否则 `JumpInvalidTarget`；
- 解析出的端点不等于当前端点 —— 否则 `JumpInvalidTarget`。

通过后立即回 `Accepted`，然后按 `delay_ms` 排期（与 Task/Tool 相同）：延迟结束后才进入排空/切换流程。

#### 7.3.2 正常流程（成功）

```text
Worker（旧链路）                                旧 Router
  │  ◀── Stdin(Jump, id=N) ───────────────────────  │
  │  ── Accepted(id=N) ──────────────────────────▶  │
  │  ── Offline(state, retained, seq>0) ────────▶  │ 标记离线
  │    等待发件箱排空（最多 jump_drain_ms，默认 1500ms）
  │  ── DISCONNECT(Success, session_expiry=0) ──▶  │ 旧会话立即失效
  │    关闭 TCP，连接目标端点
Worker（新链路）                                新 Router
  │  ── CONNECT ────────────────────────────────▶  │
  │  ◀──────────────────────────── CONNACK ──────  │
  │  ── SUBSCRIBE in（无会话时）────────────────▶  │
  │  ── Hello(state, retained) ─────────────────▶  │ 新 Router 获得 Worker 状态
  │  ── Completed(id=N, JumpOutcome{target}) ──▶  │ **最终事件出现在新 Router**
```

步骤细节：

1. **排空（Draining）**：Worker 先向旧链路发布 retained `Offline`，然后等待发件箱清空（所有事件已发布并收到 PUBACK）或到达排空上限 `jump_drain_ms`（参考实现默认 1500 ms）。超时则带着未发完的事件直接切换。
2. **断开旧链路**：向旧 broker 发送 `DISCONNECT`，原因码 `0x00 Success`，属性 `session_expiry_interval = 0`；发送后关闭 TCP。这会丢弃旧会话，避免旧 broker 继续缓存并补发消息。
3. **切换链路**：运行时向启动层交付 `LinkAction::Switch { endpoint = 目标, reason = JumpCommand }`；启动层负责关闭旧连接、连接新端点，再调用 `link_up`。
4. **新链路握手**：收到新 Router CONNACK 后，Worker（若无会话）订阅 `in` 主题、发布 retained Hello，然后发布 Jump 的 `Completed`（载荷 = JumpOutcome，`cmd_id` 为 Jump 的 id）。
5. **Router 关联**：旧 Router 可能永远看不到该 `Completed`；新 Router 必须能按 `cmd_id` 关联到之前在旧 Router 上发起、或通过共享状态同步过来的命令。

#### 7.3.3 失败与回退（fallback）

以下情况进入回退流程：

- 连接目标端点失败（TCP 连接失败）；
- 目标返回 CONNACK 失败（连接被拒）；
- 切换过程中链路断开。

回退行为：

1. 运行时向原命令发 `Failed(0x000C JumpUnreachable, "跳转目标不可达，已回退")`。此时链路正断开，事件先进入发件箱。
2. 运行时交付 `LinkAction::Switch { endpoint = 原端点, reason = Fallback }`；启动层重新连接原端点。
3. 重连原端点成功并收到 CONNACK 后，Worker 会（重新）发布 retained Hello，并把排队的 `Failed` 事件送达原 Router。两者都来自发件箱，按 `seq` 排序，Router 不应假设它们的到达先后。
4. 因此 Router 在原链路上可能晚于预期才收到 `Failed`；等待超时应覆盖回退重连时间。

回退后 Worker 继续在原 Router 工作；Router 不应把“Jump 没有立即完成”直接判定为 Worker 失联。

#### 7.3.4 Router 迁移（broker 主动触发）

除了 Router 下发 Jump 帧，broker 也可以通过 MQTT 5 的重定向让 Worker 迁移：

1. 旧 broker 向 Worker 发送 `DISCONNECT`，原因码为 `0x9C Use Another Server` 或 `0x9D Server Moved`，并携带 `server_reference` 属性。
2. Worker 解析 `server_reference`：先按 `host:port` 解析，失败则按 DeviceList 名称/IP 解析（允许显式端点）。
3. 满足以下条件时切换：当前不在 Jump 中；目标端点与当前/最近正常端点不同；连续迁移次数未达到上限（参考实现默认最多连续跟随 3 次，第 4 次起忽略）。
4. 切换动作：`LinkAction::Switch { endpoint = 新端点, reason = ServerMoved }`。**不产生任何命令事件**（迁移不是命令）；新链路 CONNACK 后发布 retained Hello。
5. 连续迁移计数在 Worker 成功接受一条新命令时清零；超过上限后 Worker 忽略后续重定向，按普通断线重连处理。

Router/broker 侧注意：

- 重定向期间 Worker 会短暂离线，Router 应以新端点收到的 Hello 为“上线”信号。
- 若 `server_reference` 缺失、非法或指向当前端点，Worker 会忽略并继续普通重连。
- 不要设计成无限重定向：Worker 有连续迁移上限，达到上限后不再跟随。

#### 7.3.5 Jump 期间的约定

- **Worker 立即拒绝 `in` 主题上的新命令**：回复 `Failed(0x0005 Busy)`（不排队、不静默丢弃），并计入内部拒绝计数。Router 在发送 Jump 后、收到新链路 Hello 前不要下发命令；收到 `Busy` 时应等待迁移结束后重试。
- 已经进入等待队列/运行中的 Task 与已排期的命令**不会被 Jump 自动取消**；它们可能在切换期间继续执行，事件排入发件箱，最终可能在新链路上送达。若业务要求“迁移前清空”，Router 应先用 `sekai.cancel` 取消相关命令并等待最终事件。
- Jump 期间不迁移业务状态；V1 不支持把 Task 移交到另一个 Worker（任务 handoff）。
- Jump 的 `ddl_ms` 不参与超时控制，但仍必须满足协议上限。

#### 7.3.6 运行时观测（参考实现）

运行时的 Jump 阶段（供状态接口/日志观测，不在协议线上传输）：

| 阶段 | 含义 |
| --- | --- |
| `Draining` | 正在排空发件箱、准备断开旧链路 |
| `Switching` | 正在连接新端点并等待 CONNACK |
| `FallingBack` | 正在回退并连接原端点 |

`LinkAction::Switch` 的原因（参考实现）：

| 原因 | 含义 |
| --- | --- |
| `JumpCommand` | 由 Stdin Jump 命令触发 |
| `ServerMoved` | 由 broker 的 0x9C/0x9D 重定向触发 |
| `Fallback` | Jump 失败后回退原端点 |

---

## 8. 服务端契约

本节是**Router 实现必须遵守**的约定；不遵守会导致重复执行、丢事件或误判离线。

### 8.1 命令 id

- 对同一 Worker，`cmd_id` 必须**单调递增且不重复**（推荐从 1 开始、每次加 1 的 `u64`）。
- 在 Worker 可能仍缓存该 id 的窗口内，绝不能用同一 `id` 下发不同命令。
- Worker 重启后不保留去重缓存；Router 可以在确认 Worker 重启（新 `boot_id`）后重新使用旧 id，但仍建议全局单调，避免混乱。

### 8.2 传输

- 必须用 **QoS1** 向 `{root}/dev/{device_id}/in` 发布 Stdin。
- 必须订阅 `out` 与 `state`，同样使用 QoS1。
- 必须部署 MQTT 5 broker，开启 retain；否则 Worker 进入降级模式（Hello/Offline 走 `out` 且不保留、QoS 降为 0）。
- `in` 上的 Stdin 必须 `retain = false`。
- 推荐为每条 Stdin 设置 `content_type = application/vnd.sekai.stdin.v1`。

### 8.3 重试与去重

- 重试必须**逐字节重发同一帧**（相同 `id`、相同 `timestamp`、相同载荷），不得生成新的 `id`。
- Worker 端去重是有界窗口（参考实现 32 个 id）；窗口外且不在运行/最终缓存中的重复会被当成新命令。Router 重试次数与时间窗口应保守。
- Router 对 Stdout 必须按 `(device_id, boot_id, seq)` 幂等去重；`seq` 是全局递增而非按命令递增。
- Router 可以重发同一条命令来“查询结果”，但更推荐等待最终事件；重发在 Worker 端可能被忽略（命令仍在执行时无响应）。

### 8.4 Accepted 不是最终结果

- `Accepted` 只表示命令被受理；`Started` 只表示开始执行。
- 必须等待 `Completed` 或 `Failed` 才能结束命令状态。
- 一条命令至多一个最终事件；最终事件可能因重试而重复（相同 `seq`），Router 去重后应返回同一业务结果。

### 8.5 超时

Router 侧等待超时应覆盖 Worker 侧的最坏延迟：

```text
最小等待时间 ≈ delay_ms
              + ddl_ms（Task/Tool，ddl_ms=0 时取 Worker 默认 500ms）
              + 网络与重传余量
```

Jump 另需覆盖：发件箱排空 `jump_drain_ms`（默认 1500 ms）+ 旧链路断开 + 新链路 TCP/MQTT 建连 + 回退场景下的再次重连。建议为 Jump 单独设置更长超时，并以新链路上的 Hello 作为迁移完成信号；不要仅凭旧链路静默判定失败。

### 8.6 Jump 的最终事件可能出现在新 Router

- Jump 成功后，`Completed`（JumpOutcome）由 Worker 发布到**新 Router 的 `out` 主题**。
- 旧 Router 通常不会收到该最终事件。
- 因此多 Router 部署必须满足其一：
  - Router 集群共享 Worker 命令状态（推荐），新 Router 按 `cmd_id` 关联并完成业务回调；
  - 或由发起 Jump 的上层系统同时监听目标 Router，并将结果汇总。
- Router 必须容忍“Worker 在旧 Router 上 `Accepted`，最终事件只在目标 Router 出现”的跨链路时序。

### 8.7 Jump 期间不要下发命令

- Worker 在 Jump 期间（排空、切换、回退）会忽略 `in` 上的新命令，且**不会回 Failed**。
- 正确做法：发送 Jump 后暂停该 Worker 的新命令；等待新链路上的 Hello（或回退后的 Hello/`Failed`）确认 Worker 回到可服务状态，再继续下发。
- 已下发且被 `Accepted` 的延迟命令可能跨 Jump 继续执行；需要“干净迁移”时先取消并等待。

### 8.8 时间校准（set_time）

- Worker 没有 Battery-backed RTC：重启后只能得到单调时间；只有通过 `sekai.set_time` 才能把墙上时间校准。
- Router 应在 Worker 连接后（以及长时间运行后）用可信 Unix 毫秒调用 `sekai.set_time`；参数必须落在 `[1600000000000, 4000000000000]`。
- 校准前：Hello 的 `unix_ms`、Stdout 的 `timestamp` 可能为 0，命令时效检查（`Expired`）不会生效。
- 校准会让 Worker 时间跳变；Router 不要用 Worker 时间戳的差值做精确间隔测量（间隔应以单调时间/服务端时间为准）。
- `sekai.set_time` 返回校准前的 Worker 时间（8 字节大端），可用于诊断。

### 8.9 尊重 Worker 能力与上限

下发命令前应检查最近一次 Hello：

- `tasks` / `tools` 是否包含目标名称；否则 Worker 会回 `UnknownTask` / `UnknownTool`。
- `devices` 是否包含 Jump 目标（或 Worker 是否允许任意目标）。
- `active_tasks` 反映正在运行的任务数量，避免继续堆积。
- 协议硬上限（见 §9.4）：单帧载荷 ≤4096、名称 ≤48、Worker id ≤64、目标 ≤96、DDL ≤5 分钟、延迟 ≤24 小时、Worker 列表 ≤8 等。
- Worker 收件箱/等待队列都很小（参考实现各 4），Router 应控制“在途命令”数量，而不是一次性 burst 下发。

### 8.10 重启与状态

- Worker 重启后：`boot_id` 变化、`seq` 从 1 重新开始、去重缓存与发件箱清空；Router 必须按 `boot_id` 重置去重状态。
- Router 应把 `state` 主题上的 retained Hello（新 boot_id）视为重新上线；此前 retain 的 Offline 可能仍存在，直到新 Hello 覆盖它。
- Worker 异常掉线时，retained Offline（`seq=0`）由 LWT 发布；Router 应据此把 Worker 标记为离线，而不是等待超时。

### 8.11 其他

- 不要依赖 `Progress`：V1 Worker 不会发送；收到未来的阶段值应作为协议错误记录，不要猜测语义。
- `Failed` 的 `message` 内容不保证稳定，禁止用于程序分支。
- 对未知 `cmd_id` 的事件（例如 Router 重启后收到重放的最终事件）应记录并忽略，或按幂等结果处理，不要崩溃。
- Router 应能处理 `offline` 与随后 `hello` 之间的状态切换，以及同一 `boot_id` 内 `seq` 的乱序到达（MQTT QoS1 不保证跨主题顺序）。

---

## 9. 安全与限制

### 9.1 信任边界：broker ACL

协议本身**不提供**身份认证、加密、签名或权限模型；安全边界完全由 MQTT broker 的认证与 ACL 承担。任何能向 `in` 主题发布合法 Stdin 的主体，都能让 Worker 执行已注册的任务/工具。建议 ACL：

| 主体 | 允许 | 禁止 |
| --- | --- | --- |
| Router/控制端 | 发布 `{root}/dev/+/in`；订阅 `{root}/dev/+/out`、`{root}/dev/+/state` | 发布 `in` 以外 Worker 主题（除 broker 迁移场景） |
| 单个 Worker | 发布 `{root}/dev/{device_id}/out`、`.../state`；订阅 `.../in` | 发布/订阅其他 Worker 的 `out`/`state`/`in` |
| 其他主体 | 无 | 全部 |

- Router 凭据泄露等价于 Worker 被完全控制，必须按 Worker/租户隔离凭据与 ACL。
- Worker 侧只能校验协议字段，无法验证命令来源；不要向不可信 broker 暴露 Worker。
- `sekai.workers.set`、`sekai.set_time`、`sekai.cancel` 是特权能力：`workers.set` 会改变后续 Jump 可达范围，`set_time` 影响 Worker 时间与时效判断，`cancel` 可终止业务任务。ACL 上应与其他命令同等保护。

### 9.2 传输安全

- V1 协议**不加密、不签名**；参考实现的宿主链路是明文 TCP（`crate/sekai-worker` 的 `TcpLink`）。
- 部署必须使用受信任网络，或在 MQTT 传输层加 TLS（由启动层/部署决定，协议帧不感知）。不要通过公网明文暴露 broker。
- 参考实现支持 MQTT 用户名/密码（宿主通过环境变量配置），但凭据保护仍依赖传输层。

### 9.3 无动态代码

- Worker 只执行**预先注册**的 Task/Tool 处理器；协议载荷只是参数，不包含脚本、DSL 或可执行代码。
- Worker 端没有 `eval`/动态加载；未知任务/工具名一律拒绝。
- `sekai.` 前缀保留给平台内置能力，用户注册表拒绝该前缀，避免伪造内置工具。

### 9.4 协议硬上限与参考实现默认值

协议常量（编码/解码必须遵守）：

| 常量 | 值 | 适用 |
| --- | --- | --- |
| `PROTOCOL_VERSION` | 1 | 帧版本 |
| `MAX_PAYLOAD` | 4096 字节 | Stdin/Stdout 载荷 |
| `MAX_NAME_LEN` | 48 字节 | 任务名/工具名 |
| `MAX_DEVICE_ID_LEN` | 64 字节 | Worker id、DeviceList 名称 |
| `MAX_FIRMWARE_LEN` | 64 字节 | 固件版本字符串 |
| `MAX_HOST_LEN` | 64 字节 | Jump 目标主机/IP |
| `MAX_TARGET_LEN` | 96 字节 | Jump 目标字符串 |
| `MAX_TASKS` | 16 | Hello 任务列表、任务注册表 |
| `MAX_TOOLS` | 16 | Hello 工具列表、工具注册表 |
| `MAX_DEVICES` | 8 | DeviceList |
| `MAX_ACTIVE_TASKS` | 4 | Hello 活跃任务列表 |
| `MAX_DELAY_MS` | 86 400 000（24h） | Stdin `delay_ms` |
| `MAX_DDL_MS` | 300 000（5min） | Stdin `ddl_ms` |
| `DEFAULT_DDL_MS` | 500 ms | `ddl_ms = 0` 时使用 |
| `MAX_ERROR_MESSAGE_LEN` | 256 字节 | Failed 消息 |

参考运行时默认值（可配置，Router 应把它们视为“Worker 能力”而非协议保证）：

| 配置 | 默认值 | 说明 |
| --- | --- | --- |
| 主题根 `topic_root` | `sekai/v1` | |
| 命令最大年龄 `max_command_age_ms` | 300 000 ms | 超时回 `Expired` |
| 收件箱 / 等待队列 / 运行中 Task | 4 / 4 / 4 | 见 §7.1 |
| 发件箱 | 8 条 / 16 KiB | 超出会淘汰非最终事件 |
| 去重窗口 | 32 个 id | |
| 最终事件缓存 | 8 条 / 4 KiB | |
| Jump 排空上限 | 1500 ms | |
| 连续 Router 迁移上限 | 3 次 | |
| 允许任意 Jump 目标 | `false` | 只接受 DeviceList 名称/IP |
| LWT Offline | 开启 | |
| Keep Alive / 会话过期 / Receive Maximum / 最大报文 | 30s / 300s / 8 / 16 KiB | 见 §2.4 |

### 9.5 V1 不支持的能力

| 不支持的项 | 说明 / 替代做法 |
| --- | --- |
| IPv6 字面量 | Jump 目标只接受主机名或 IPv4 字面量；带方括号的 IPv6 会被拒绝。 |
| 任意 `host:port` 目标 | 默认关闭，只能跳转到 DeviceList 中的名称/IP；需要时显式启用（有 SSRF 风险）。 |
| 任务 handoff | Jump 只迁移 Worker 自身链路，不把运行中的 Task 移交给另一个 Worker；跨 Worker 接力由业务层自行设计。 |
| 专门的取消帧 | 使用 `sekai.cancel` 工具。 |
| Progress 事件 | V1 不发送，接收方必须容忍缺失。 |
| 帧加密/签名 | 依赖 MQTT TLS + ACL；协议层无内建安全。 |
| 多帧拼接/分片 | 一帧必须完整放在一条 PUBLISH 中，且不超过载荷上限。 |
| 自动重放未确认的业务命令 | Worker 只重放未确认的 Stdout 事件，不重放已执行的命令。 |

### 9.6 资源与拒绝服务

- 入站/等待/发件箱/运行任务/Worker 列表/Hello 列表均有界；超限产生 `Busy`、`ResultTooLarge` 或帧解码错误。
- 发件箱满时优先淘汰非最终、非 Hello/Offline 的事件（`dropped_events` 计数），因此 Router**不能假设中间事件完整**；最终事件缓存有独立上限。
- 错误消息最长 256 字节，避免 Worker 向 Router 放大输出。
- Router 应限速命令下发；大量未知任务/工具、超大载荷、过量 Jump 都会消耗 Worker 有限内存与带宽。
- Worker 处理器必须协作式让出 CPU；阻塞会拖慢 MQTT Keep Alive 处理，最终被 broker 断开并产生误判离线。

---

## 10. 完整十六进制示例

以下示例按当前实现逐字节计算。公共上下文：

| 项 | 值 |
| --- | --- |
| Worker id | `demo` |
| 主题根 | `sekai/v1` |
| `boot_id` | 1 |
| 命令 id | 1 |
| Router 时间戳 | `1700000000000` ms（`0x0000_018B_CFE5_6800`） |
| Task 名称 | `echo` |
| Task 参数 / 结果 | ASCII `hello` |
| `delay_ms` | 0 |
| `ddl_ms` | 500（`0x0000_01F4`） |

### 10.1 Stdin：下发 Task（41 字节）

载荷 TaskBody：`00 04` + `echo` + `hello` = 11 字节：

```text
载荷（11 字节）
00 04 65 63 68 6F 68 65 6C 6C 6F
```

完整帧（发布到 `sekai/v1/dev/demo/in`，QoS1，`content_type = application/vnd.sekai.stdin.v1`）：

```text
偏移  字节                                             说明
0x00  01 01                                            version=1, kind=1(Task)
0x02  00 00 00 00 00 00 00 01                          id=1
0x0A  00 00 01 8B CF E5 68 00                          timestamp=1700000000000
0x12  00 00 00 00                                      delay_ms=0
0x16  00 00 01 F4                                      ddl_ms=500
0x1A  00 00 00 0B                                      payload_len=11
0x1E  00 04 65 63 68 6F 68 65 6C 6C 6F                TaskBody("echo", "hello")
```

单行十六进制（共 41 字节 = 0x29）：

```text
01 01 00 00 00 00 00 00 00 01 00 00 01 8B CF E5 68 00
00 00 00 00 00 00 01 F4 00 00 00 0B
00 04 65 63 68 6F 68 65 6C 6C 6F
```

服务端构造要点：

- `id`、`timestamp`、`delay_ms`、`ddl_ms`、`payload_len` 全部大端。
- `payload_len` 必须等于实际帧剩余字节；帧尾多余字节会被拒绝。TaskBody 的 args 是载荷剩余部分，因此不存在“尾部多余”的独立约束。
- `content_type` 可省略（Worker 接受），但推荐携带。

### 10.2 Stdout：命令事件（Accepted → Started → Completed）

Worker 成功执行 `echo` 后产生的完整事件序列（同一 `boot_id=1`、`cmd_id=1`）。Worker 在 CONNACK 后已发布过一帧 retained Hello（本 boot 的 `seq=1`），所以命令事件从 `seq=2` 开始，这也体现了 `seq` 是 boot 全局递增而非按命令递增：

| 事件 | 主题 | phase | seq | payload |
| --- | --- | --- | --- | --- |
| Accepted | `.../out` | 1 | 2 | 空 |
| Started | `.../out` | 2 | 3 | 空 |
| Completed | `.../out` | 4 | 4 | `hello`（5 字节） |

- `Accepted`（34 字节）：

```text
01 01 00 00 00 00 00 00 00 01 00 00 00 00 00 00 00 01
00 00 00 02 00 00 01 8B CF E5 68 00 00 00 00 00
```

- `Started`（34 字节）：

```text
01 02 00 00 00 00 00 00 00 01 00 00 00 00 00 00 00 01
00 00 00 03 00 00 01 8B CF E5 68 00 00 00 00 00
```

- `Completed`（39 字节 = 0x27）：

```text
偏移  字节                                             说明
0x00  01 04                                            version=1, phase=4(Completed)
0x02  00 00 00 00 00 00 00 01                          boot_id=1
0x0A  00 00 00 00 00 00 00 01                          cmd_id=1
0x12  00 00 00 04                                      seq=4
0x16  00 00 01 8B CF E5 68 00                          timestamp=1700000000000
0x1E  00 00 00 05                                      payload_len=5
0x22  68 65 6C 6C 6F                                   "hello"
```

单行十六进制：

```text
01 04 00 00 00 00 00 00 00 01 00 00 00 00 00 00 00 01
00 00 00 04 00 00 01 8B CF E5 68 00 00 00 00 05
68 65 6C 6C 6F
```

说明：

- 本示例假设三次事件时间戳相同（`1700000000000`）；真实 Worker 为每帧填各自当前时间。
- `Accepted`/`Started` 的载荷为空，所以帧长固定 34 字节。
- Router 按 `(device_id="demo", boot_id=1, seq)` 去重；三条事件 `seq` 分别为 2、3、4。
- 如果 Router 重试同一 Stdin（`id=1`），Worker 会重放 `Completed` 帧，`seq` 仍是 4（同一缓存帧），不会重新执行。

### 10.3 其他常用片段的十六进制

- Failed/Timeout + 消息“超时”的载荷（10 字节）：

```text
00 08 00 06 E8 B6 85 E6 97 B6
```

- Jump 成功时 Completed 的 JumpOutcome 载荷 `peer-1:1883`（10 字节）：

```text
00 06 70 65 65 72 2D 31 07 5B
```

- LWT Offline 帧（34 字节，全零载荷；`seq=0`、`cmd_id=0`）：

```text
01 07 00 00 00 00 00 00 00 01 00 00 00 00 00 00 00 00
00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00
```

（`timestamp` 为建立连接时 Worker 时间；上例中假设为 0。）

---

## 11. 故障排查

| 现象 | 可能原因 | 处理 |
| --- | --- | --- |
| 同一条命令被执行两次 | Worker 重启导致去重缓存丢失；重复帧用了新 `cmd_id`；去重窗口（默认 32）已滑出；最终缓存已淘汰后重试 | 重试必须逐字节相同；Router 记录 `(device_id, boot_id, cmd_id)` 执行结果；不要用同一 `cmd_id` 表示新命令；业务侧做幂等。 |
| 事件时间戳为 0 | Worker 未校准（无 RTC） | 连接后调用 `sekai.set_time` 传入可信 Unix 毫秒；Router 不要用 0 做时间计算；可先用 `sekai.time` 查看 Worker 时间与单调时间。 |
| Router 收不到 Hello/Offline（retain 不支持） | broker `retain_available=false`，Worker 退化为发到 `out` | 修复 broker 配置；Router 同时订阅 `out` 并处理 Hello/Offline；检查 broker 是否为 MQTT 5。 |
| Jump 报目标不可达后 Worker 又回到原 Router | 目标 TCP 不通 / CONNACK 被拒 / 切换中断链，Worker 已回退 | 检查目标地址与端口、`SEKAI_DEVICES`/DeviceList、目标 broker 是否可达；查看原链路收到的 `Failed(0x000C)`；这是预期回退，不应重试风暴。 |
| Worker 重启后重复执行旧命令 | 新 `boot_id`、去重缓存清空、Worker 可能重新订阅到 retained 命令 | 禁止 `in` retained；Router 在 Worker 重启后不要重放旧命令；按 `boot_id` 重置去重与命令状态。 |
| 命令无响应（没有 Accepted/Failed） | 版本/kind 非法导致无 `cmd_id` 可回填；命令仍在执行且被重试忽略；Worker Jump 中忽略新命令；收件箱满但对方未收到事件 | 检查帧首字节与 `kind`；等待最终事件；Jump 期间暂停下发；查看 Worker 状态/日志。 |
| 所有命令都回 `Busy` | Jump 进行中、无可用链路、收件箱/等待队列满 | 等待 Hello/最终事件；降低在途命令数量；检查 Worker 是否刚从断线恢复。 |
| 回 `Expired` | Router `timestamp` 过旧（超过 Worker `max_command_age_ms`，默认 300s）或 Worker 时钟被校准后跳变 | 校准 Worker 时间；缩短命令在途时间；不要把很旧的帧重试给 Worker。 |
| 回 `InvalidCommand` | `content_type` 不符；`timestamp` 超前 Worker 时间 300s 以上；帧结构非法 | 统一 `content_type`；校准双方时间；按 §10 逐字段核对帧。 |
| 回 `UnknownTask` / `UnknownTool` | 名称不在 Worker 能力列表 | 读取最新 Hello 的 `tasks`/`tools`；注意用户工具数受 16 总数约束（参考实现内置 6 个）。 |
| Task 超时但业务实际已完成 | 处理器阻塞、poll 不返回、系统忙导致 DDL 到点 | 改为协作式实现，及时返回 `Pending`；把长工作拆到多次 poll；不要使用阻塞 IO/忙等。 |
| `ResultTooLarge` | 结果超过 4096 字节 | Worker 端压缩/分片业务结果，或改用 Task + 多次查询；协议单帧不可超限。 |
| Jump 后在旧 Router 等不到 Completed | 最终事件发到了新 Router | 见 §8.6：跨 Router 共享命令状态或同时监听目标 Router；旧 Router 不要把它当作失败重试。 |
| Worker 频繁被 broker 重定向 | `server_reference` 指向异常端点，或迁移形成循环 | 检查 broker 集群重定向配置；Worker 默认最多连续跟随 3 次，之后忽略并按普通重连处理。 |
| retained state 一直是 Offline | Worker 未重新连接成功；或新 broker 与旧 broker 是不同实例 | 检查 Worker 连接与 LWT；`state` 是每 broker 本地的 retained 值，跨 broker 需要 Worker 连上后发布新 Hello。 |
| Router 看到重复 `seq` 的事件 | QoS1 重传、发件箱重放、重复订阅 | 按 `(device_id, boot_id, seq)` 去重；不要按载荷内容去重。 |
| 收到 `Progress` | 对端实现了非 V1 扩展 | V1 Worker 不发 Progress；记录并忽略，不要当作完成。 |

---

## 12. 参考实现映射（crate/sekai、crate/sekai-gpio、crate/sekai-worker、crate/sekai-embedded、crate/sekai-router）

> 本节帮助实现者对照代码，不是协议要求；具体 API 以源码为准。

### 12.1 文件职责

| 文件 | 职责 |
| --- | --- |
| `crate/sekai/src/sekai_wire.rs` | 协议版本、常量上限、错误码、名称/Worker id/主机/目标校验。 |
| `crate/sekai/src/sekai_stdin.rs` | Stdin 帧、Kind、TaskBody/ToolBody/JumpBody 编解码。 |
| `crate/sekai/src/sekai_stdout.rs` | Stdout 帧、Phase、ErrorPayload、JumpOutcome、Hello/DeviceList/ActiveTask。 |
| `crate/sekai/src/sekai_device_list.rs` | DeviceList、Endpoint、`host:port` 解析与 Jump 目标解析。 |
| `crate/sekai/src/sekai_clock.rs` | 运行时时间源抽象（单调时间、Unix 时间、校准）。 |
| `crate/sekai/src/sekai_task.rs` / `sekai_tool.rs` | Task/Tool 处理器与注册表（名称上限、保留前缀、处理器契约）。 |
| `crate/sekai/src/sekai_runtime.rs` | 控制平面运行时：收件箱、调度、DDL、去重、发件箱、Hello/LWT、Jump、Router 迁移、内置工具。 |
| `crate/sekai-gpio/` | 家族无关 GPIO 抽象、ESP32/F1/F4 引脚表、虚拟后端与 4 个用户工具（`gpio.*`）；默认构建不依赖 HAL，可选 ESP32/STM32 后端。 |
| `crate/sekai-worker/src/link.rs` | 宿主直连链路：stdio/TCP、长度前缀 V1 帧读写与重连退避（非 MQTT）。 |
| `crate/sekai-embedded/src/framing.rs` | MQTT 连续字节流分帧（no_std，嵌入式 Worker 固件使用）。 |
| `crate/sekai-worker/src/host.rs`、`src/bin/sekai-worker.rs` | 宿主驱动与可执行入口（示例任务 `echo`、`sleep`，默认注册虚拟 GPIO 工具）。 |
| `crate/sekai-embedded/src/driver/esp32/` | 固件驱动：WiFi/DHCP、MQTT TCP 主循环、GPIO 工具注册与运行时接入（`mod.rs`/`net.rs`）。 |
| `crate/sekai-router/` | Router（二进制 `sekai-router`，由原 `crate/sekai-ai` 的 `sekai-agent` CLI 演化而来）：MQTT 5 连接 broker、Worker 注册表与命令生命周期、DeepSeek Chat Completions function tools；host-only、零第三方依赖。设计沿革见 `docs/sekai-ai.md`（历史文档），现行架构见 `docs/sekai-architecture.md`。 |

### 12.2 运行时 API 摘要

| API | 作用 |
| --- | --- |
| `Runtime::new(config, clock, devices, tasks, tools)` | 构造运行时，校验配置、注册表上限，并生成 LWT Offline。 |
| `Runtime::link_up(endpoint)` | TCP 建连后调用，发起 MQTT CONNECT。 |
| `Runtime::feed(packet)` | 喂入一条已解码 MQTT 报文。 |
| `Runtime::poll()` | 推进 MQTT 定时器、收件箱、Task、发件箱、Jump。 |
| `Runtime::next_outgoing()` | 取出待发送 MQTT 报文。 |
| `Runtime::next_deadline()` | 下一次建议 poll 的单调时刻。 |
| `Runtime::take_link_action()` | 取出 `LinkAction::Switch`（Jump/迁移/回退）。 |
| `Runtime::link_closed()` / `link_lost()` / `link_failed(endpoint)` | 通知链路正常关闭 / 异常断开 / 建连失败。 |
| `Runtime::status()` | 运行时观测快照（状态、队列长度、Jump 阶段等）。 |

固件/宿主的主循环建议：

```text
loop {
    if 未连接 { 连接端点；runtime.link_up(endpoint) }
    写出 runtime.next_outgoing() 的全部报文
    读传输层字节 → 分帧 → runtime.feed(packet)
    runtime.poll()
    if let Some(action) = runtime.take_link_action() {
        先 flush 剩余报文（DISCONNECT）→ 关闭旧链路 → runtime.link_closed() → 连接 action.endpoint → runtime.link_up()
    }
    按 runtime.next_deadline() 睡眠（有上限）
}
```

### 12.3 宿主示例的部署参数

> **历史注记**：本节记录的是 2025 重构前的 `sekai-host` 示例（已并入
> `crate/sekai-worker`）。现行 Worker 的命令行、`--gpio` 与 `pc.*` 能力见
> [`crate/sekai-worker/README.md`](../crate/sekai-worker/README.md) 与
> [`docs/sekai-pc-agent.md`](sekai-pc-agent.md) §6；下面的环境变量名与工具数量
> （"6 内置 + echo + 4 gpio = 11"）不再对应当前实现。

`sekai-host`（x86 构建产物）使用环境变量：

| 环境变量 | 必填 | 说明 |
| --- | --- | --- |
| `SEKAI_MQTT_ADDR` | 是 | broker 地址，如 `127.0.0.1:1883` |
| `SEKAI_DEVICE_ID` | 否 | Worker id，默认 `sekai-x86` |
| `SEKAI_MQTT_USERNAME` / `SEKAI_MQTT_PASSWORD` | 否 | MQTT 凭据 |
| `SEKAI_DEVICES` | 否 | 跳转目标列表，格式 `name=host:port[,name=host:port]`，最多 8 项 |
| `SEKAI_GPIO` | 否 | 虚拟 GPIO 开关，默认 `on`；`off` 时不注册任何 `gpio.*` 工具 |
| `SEKAI_GPIO_FAMILY` | 否 | 虚拟 GPIO 引脚表：`esp32`（默认）/ `stm32f1` / `stm32f4` |
| `SEKAI_GPIO_INPUTS` | 否 | 虚拟输入的初始电平，格式 `IO4=1,PA5=0`（可空） |

示例任务：`echo`（返回参数）、`sleep`（参数为 4 字节大端毫秒，默认 1000）。示例工具：`echo`。内置工具无需注册，始终可用。
启用虚拟 GPIO 时（默认），宿主共注册 6 个内置 + `echo` + 4 个 `gpio.*` 工具（11 个，≤16）；细节见 §12.4。

### 12.4 GPIO 用户工具（参考实现）

> 本节是参考实现映射，不是协议要求；GPIO 工具是**用户工具**，不改变帧格式、错误码或 `content_type`。

`crate/sekai-gpio` 注册 4 个工具（按名称排序）：

| 工具 | 文本参数 | 成功结果 | 说明 |
| --- | --- | --- | --- |
| `gpio.info` | 空 / `help` / `PIN` / `port=A`（仅 STM32） | 多行 `key=value`；`help` 为固定英文文本 | 家族摘要、单引脚详情、端口列表、自描述帮助 |
| `gpio.config` | `PIN [in/out] [dir=...] [level=...] [pull=...] [open_drain=...]` | `pin=... mode=... level=... pull=... open_drain=...` | 部分更新；未给方向时沿用当前方向，未配置默认输入 |
| `gpio.read` | `PIN` | `pin=... level=0/1` | 仅允许输入模式 |
| `gpio.write` | `PIN 0/1`（也接受 `low`/`high`/`on`/`off`） | `pin=... level=0/1` | 仅允许输出模式 |

- 参数与结果都是 UTF-8 文本，对协议层不透明：参数上限 **256 字节**，结果上限 **1024 字节**（超过返回 `ResultTooLarge`）。
- 错误码：所有参数/引脚/能力/模式错误都是 `InvalidArgument`；只有共享句柄借用失败或后端异常是 `Internal`。
  协议保留的 `Unsupported`（0x000E）不被 GPIO 工具使用；0x000A 是 `Cancelled`。
- `gpio.info help` 是唯一非 `key=value` 的结果，内容逐字固定；AI 应先调用它获取语法，再调用
  `gpio.info` 摘要与 `gpio.info PIN` 确认引脚能力。
- 工具通过 Hello 的 `tools` 正常通告；工具总数（含 6 个内置工具）不得超过 16。参考实现：
  ESP32 固件注册 4 个 GPIO 工具（6+4=10）；宿主 `sekai-host` 额外注册 `echo`（默认 11 个）。
- 完整语法、引脚表、AI 调用流程与安全说明见 `docs/sekai-gpio.md`；ESP32 固件构建/配网/故障排查见
  `docs/sekai-esp32.md`。

---

## 13. 实现者检查清单

### 13.1 Worker 端

- [ ] 帧解码严格：版本、kind/phase、长度、尾随字节、载荷上限。
- [ ] 所有整数大端；字符串使用 `u16` 长度前缀且拒绝 NUL。
- [ ] Stdin `id != 0`；`delay_ms <= 24h`；`ddl_ms <= 5min`；载荷非空。
- [ ] Stdout `boot_id != 0`；Hello/Offline 的 `cmd_id == 0`；Offline 载荷为空；LWT `seq = 0`。
- [ ] 每次启动生成新的非 0 `boot_id`；`seq` 从 1 递增（回绕跳过 0）。
- [ ] 订阅 `in`（QoS1、no_local、retain_handling=0）；发布 `out`/`state` 使用 QoS1。
- [ ] `state` 使用 retained；CONNACK 后发 Hello；配置 retained LWT Offline。
- [ ] 实现去重：`cmd_id` 窗口 + 最终事件缓存；重试重放相同最终帧。
- [ ] 实现延迟调度、DDL 超时、并发 Task 上限、结果大小检查。
- [ ] Tool 同步执行且有调用预算；内置工具集合完整。
- [ ] Jump 排空 → `DISCONNECT(session_expiry=0)` → 切换 → 新链路 Hello → Completed/JumpOutcome。
- [ ] Jump 失败回退并上报 `JumpUnreachable`；处理 broker 0x9C/0x9D + `server_reference`。
- [ ] Jump 期间忽略新命令；不把协议帧当动态代码执行。
- [ ] 内容/队列/出站缓存有界，错误消息截断到 256 字节。
- [ ] GPIO 用户工具的参数/结果有测试：`gpio.info help` 逐字、结果 `key=value` 格式、
      Stdin 帧十六进制示例由测试固定（参考 `crate/sekai-gpio/tests/`）。
- [ ] 保留引脚/输入专用引脚在 configure/read/write 前被拒绝（不触碰硬件），返回 `InvalidArgument`；
      引脚不存在/能力不足同样在操作硬件前失败。
- [ ] Hello 中的工具数与运行时上限一致：工具总数（含 6 个内置工具）≤ 16；参考实现 ESP32 为 10、
      宿主默认 11。

### 13.2 服务端

- [ ] 使用 MQTT 5、QoS1、retain；正确 ACL。
- [ ] 为每条命令分配单调非 0 的 `cmd_id`；重试逐字节相同。
- [ ] 订阅 `out` 与 `state`；按 `(device_id, boot_id, seq)` 去重。
- [ ] 等待 `Completed`/`Failed`，不把 `Accepted` 当完成。
- [ ] Jump 的超时独立于普通命令，并处理“最终事件出现在新 Router”。
- [ ] Jump 期间暂停下发；以新链路 Hello 作为恢复信号。
- [ ] `in` 不使用 retained。
- [ ] 连接后用 `sekai.set_time` 校准 Worker 时间；用 `sekai.time`/`sekai.info` 做诊断。
- [ ] 读取 Hello 能力列表并在下发前检查；尊重所有上限与在途数量限制。
- [ ] Worker 重启（新 `boot_id`）后重置去重与命令状态；处理 retained Offline。
- [ ] 对 `Progress`、未知 phase、未知 `cmd_id` 做容错记录。
- [ ] 业务幂等：协议只保证“至少一次”，不保证“恰好一次”。

> 参考实现：`crate/sekai-router`（二进制 `sekai-router`，由原 `crate/sekai-ai` 的 `sekai-agent` CLI 演化而来）已按本节实现 Router 侧控制平面，并额外提供 AI 工具调用、发布前策略与脱敏 trace；设计沿革见 `docs/sekai-ai.md`（历史文档），现行架构见 `docs/sekai-architecture.md`。
