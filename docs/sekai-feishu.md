# Sekai × 飞书（Feishu）对话入口设计

| | |
| --- | --- |
| **读者** | 想把飞书/Lark 作为 sekai-router 对话入口的使用者；实现本功能的 Router 工程师 |
| **状态** | **现行**（实现已合入；与设计的差异见 §14「实现基线校准」；本文仍是协议与行为的权威参考，源码为实现基线） |
| **实现基线** | 新增 `crate/sekai-router/src/{ws.rs, sha1.rs, pbbp.rs, feishu.rs}`；修改 `config.rs`、`http.rs`、`agent.rs`、`prompt.rs`、`prompt_server.rs`、`bin/sekai-router.rs`；集成测试 `crate/sekai-router/tests/{feishu_mock.rs, agent_loop.rs}`。长连接、事件泵、会话上下文与回复**全部在 Router**；Worker 不参与飞书（`crate/sekai-worker` 无飞书依赖） |
| **相关文档** | [sekai-architecture.md](sekai-architecture.md)（前端形态）、[sekai-protocol.md](sekai-protocol.md) §0.1（带外通道清单）、[sekai-pc-agent.md](sekai-pc-agent.md)（`pc.*` 与 prompt 通道）、[根 README](../README.md)；建连引导见 [skills/feishu-setup/SKILL.md](../skills/feishu-setup/SKILL.md) |

写作约定沿用 [docs/README.md](README.md)：「必须/不得」为硬性要求；
「应该/建议」为约定；「可以」为实现自由。

---

## 0. 飞书侧协议事实与出处

本设计的全部飞书协议常量来自 **2026-11 时点的官方 SDK 行为**与开源参考实现
（`feishu-ws`，MIT/Apache-2.0，对官方 Go/Node SDK 长连接协议的独立复刻）。
飞书长连接协议**没有正式的公开规范文档**，以下事实以官方 SDK 演进为准；
实现期必须与官方 SDK 对拍（见 §10、§13），对拍不一致时以官方为准并回改本文。

## 1. 定位与总览

飞书机器人是 Router 的**第 4 个前端**（与 REPL / TUI / serve 并列）：飞书用户在
IM 里发消息，经飞书开放平台推送（WebSocket 长连接）到 Router，Router 复用
prompt 通道的共享核心执行，把答案回复到原消息。

```text
飞书用户                飞书开放平台                     sekai-router --feishu
   │  IM 消息                │                                  │
   │ ─────────────────────▶ │                                  │
   │                     事件推送（WS 长连接，pbbp2 帧）           │
   │                         │ ──────────────────────────────▶ │ feishu.rs 事件泵
   │                         │ ◀──────── ack（{"code":200}）── │   （授权/去重/限流）
   │                         │                                  │ 会话元数据 + 多轮记忆快照
   │                         │                                  │ PromptCore::run_prompt_with_context
   │                         │                                  │   （deny_llm_relay、RunGate=4）
   │                         │                                  │   （策略/网关快照）
   │                         │                                  │ Agent → Worker 链路（V1 帧，不变）
   │  IM 回复             REST im/v1                           │
   │ ◀───────────────────────│ ◀────────────────────────────── │ 答案 reply 到原消息
```

与 prompt 通道的关系：**同为「带外前端」**。飞书消息最终进入
[`PromptCore::run_prompt_with_context`](../crate/sekai-router/src/prompt_server.rs)
（无上下文语义与 `run_prompt(&self, text: &str, max_steps: Option<u32>)` 相同），
天然继承其全部护栏：`deny_llm_relay` 防递归、`RunGate` 全局 4 路并发、策略/
网关快照、错误码（`llm_unavailable`/`not_linked`/`no_workers`/`agent_error`/`busy`）。
V1 二进制帧协议**一字不动**（[sekai-protocol.md](sekai-protocol.md) §0.1
带外清单不改）。

V1 范围：**企业自建应用**机器人、**仅文本消息**、**每条消息独立目标**；同一
会话（§5.5）的会话元数据与最近 N 轮问答作为上下文注入 LLM，由 Router 在
内存中维护（**不跨重启持久化**，不改变 Worker 与帧协议）。

## 2. 形态与运行方式

| 项 | 设计 |
| --- | --- |
| 启动 | `sekai-router --feishu`，常驻无头（无 REPL/TUI，stdin 不读） |
| 旗标互斥 | `--feishu` 与 `--serve`、`--tui`、`--goal`、`--goal-file`、`--dry-run`、`-i` 互斥，冲突时 `Conflict` 报错、退出码 2（沿用 config 现有冲突路径） |
| 链路 | 未显式指定 `--link`/`SEKAI_LINK` 时，feishu 形态**默认 `tcp`**（常驻服务等 Worker 连入最自然；其余形态默认不变） |
| prompt ingress | feishu 形态默认 `off`（同批处理）；`--prompt-addr` 可显式开启，语义不变 |
| 退出码 | 沿用现有约定：**2** 配置（缺 `SEKAI_FEISHU_APP_ID`/`_APP_SECRET`、allowlist 含 `*`、旗标冲突）；**3** 飞书链路（endpoint API 不可达、握手 403/514 永久拒绝，见 §3.6）；**4** LLM（HTTPS helper 缺失或运行期 `llm_unavailable` 致命场景）。运行期 Worker 链路断开**不退出**：回用户 `not_linked` 提示，等待重连 |
| 生命周期 | Ctrl-C / SIGTERM 由**操作系统默认终止**（本 crate 禁 `unsafe`，不注册信号处理器）：进程直接退出，不保证发 close 帧或在途运行收尾；链路永久失败时退出码 3。**不读 stdin**：服务管理器常把 stdin 接 `/dev/null`，若以 stdin EOF 作退出信号会误退（§14 基线校准） |

启动时 stderr 横幅（风格同批处理「建立链路」行）：
`sekai-router: 飞书入口 domain=<domain> allow_users=<n> allow_chats=<n>`；
连接成功后 `飞书长连接已建立`；token 刷新不打常规日志（见 §4）。

## 3. 飞书长连接协议契约（承接上游）

### 3.1 获取连接端点

`POST {domain}/callback/ws/endpoint`，body 为 PascalCase：

```json
{"AppID": "cli_xxx", "AppSecret": "xxx"}
```

成功响应：

```json
{"code": 0, "data": {"URL": "wss://...",
  "ClientConfig": {"ReconnectCount": -1, "ReconnectInterval": 120,
                   "ReconnectNonce": 30, "PingInterval": 120}}}
```

| ClientConfig 字段 | 含义 | Router 用法 |
| --- | --- | --- |
| `ReconnectCount` | 重连次数上限，`-1` = 无限 | ≤0 视为无限（默认 -1 → 永不因次数放弃） |
| `ReconnectInterval` | 重连间隔（秒） | 断线后每该间隔重试 |
| `ReconnectNonce` | 抖动上限（秒） | 首次重连前随机等待 `rand(0..Nonce)` 秒 |
| `PingInterval` | 心跳间隔（秒） | 按 §3.4 发 ping |

- `code` 为 `1` 或 `1000040343`：**暂不可用**，按重连节奏重试（不退出）。
- 其余非 0 code：配置类错误（如 Secret 错），stderr 中文错误 + 退出码 3。
- 响应里的 `URL` 为 `wss://`，**query 含凭据与 `service_id`/`device_id`**：
  不得写入日志、trace 或错误消息（§6）。

### 3.2 WebSocket 帧（RFC 6455 客户端子集）

连接 `URL`（GET + `Upgrade: websocket`，校验
`Sec-WebSocket-Accept == base64(sha1(Sec-WebSocket-Key + GUID))`）。
**仅实现客户端必需子集**：

| 能力 | 必须 | 说明 |
| --- | --- | --- |
| 握手 + Accept 校验 | 是 | 校验失败视为握手失败（进入 §3.6 重连） |
| 二进制帧（opcode 0x2）收发 | 是 | 每条 WS 消息 = 一个 pbbp2.Frame（§3.3） |
| 客户端帧 mask | 是 | RFC 6455 强制；发送侧必须 mask，接收侧帧不 mask |
| 帧长度 7/16/64 位 | 是 | 含边界单测（§10） |
| ping(0x9)/pong(0xA)/close(0x8) | 是 | 协议层 ping/pong 自动应答；close 帧触发重连 |
| continuation 分片 | 接收侧必须 | 大 payload 事件可能分片发送 |
| permessage-deflate 等扩展 | 不协商 | 握手头不带扩展，收到扩展响应视为失败 |

### 3.3 pbbp2.Frame（protobuf）

WS 二进制消息即一条 protobuf 消息，schema（字段号即 wire tag）：

| 字段 | tag | 类型 | 说明 |
| --- | --- | --- | --- |
| seq_id | 1 | varint | 连接内单调递增（服务端不校验，按 1 递增即可） |
| log_id | 2 | varint | 旧日志 id（可 0） |
| service | 3 | varint | 服务标识（事件通道为常量值；按官方 SDK 对拍确定） |
| method | 4 | varint | **0 = CONTROL（ping/pong/设置），1 = DATA（事件）** |
| headers | 5 | repeated message | 键值对 `{key=1, value=2}`（string） |
| payload_encoding | 6 | string | 如 `json` |
| payload_type | 7 | string | 如 `event/v2.0` |
| payload | 8 | bytes | JSON 字节 |
| log_id_new | 9 | string | 新日志 id |

未知字段号**必须跳过不报错**（protobuf 向前兼容）。

事件 DATA 帧的 headers 约定：`type=event`、`message_id`、`trace_id`、
`sum`/`seq`（分片：`seq` 从 0 起，`sum` 为总数）。payload 为事件 JSON：

```json
{"schema": "2.0", "header": {"event_id": "...", "event_type": "...", ...},
 "event": {...}}
```

### 3.4 心跳（应用层 ping/pong）

按 `PingInterval` 发 CONTROL 帧：headers `{type: "ping"}`，空 payload。
服务端回 `{type: "pong"}`，**pong 的 headers 可以携带新的 ClientConfig**
（如调整后的 PingInterval）——必须解析并即时生效。

### 3.5 事件 ack 与分片

- 收到 `type=event` 的 **DATA 帧即回 ack**（分片事件按**每一片**回；否则该片 3 秒后
  会被重投，无法只靠 event_id 去重止血）：同 seq/method/headers 结构回一帧，
  附加 header `biz_rt`（处理耗时毫秒），payload `{"code": 200}`（处理失败
  也先 ack 200，业务错误走 REST 回复；`{"code": 500}` 仅用于「本进程无法
  处理且希望重投」的场景，V1 不使用）。
- 服务端 **3 秒**未收到 ack 会重投同一事件 → 应用层按 `header.event_id`
  去重（§6.4）。
- 分片：按 `message_id` 聚合 `seq`/`sum`，**TTL 5s**（超时丢弃未齐分片并
  审计）；凑齐后重组 payload 再进 §5 回路。
- headers `type != "event"`（如 card 回调）：**忽略、不 ack**（V1 无卡片）。

### 3.6 重连与错误分类

| 场景 | 分类 | 行为 |
| --- | --- | --- |
| TCP 断开 / close 帧 / ping 超时（2×PingInterval 无 pong） | 暂时 | 重新走 §3.1 取新 URL → 随机 jitter(0..ReconnectNonce) 秒 → 连接；失败则每 ReconnectInterval 秒重试，次数按 ReconnectCount（-1 无限） |
| endpoint `code=1`/`1000040343` | 暂时 | 同上节奏重试 |
| WS 握手被拒且 HTTP 状态 **403** | 永久 | stderr 中文错误，退出码 3（凭据/权限问题） |
| WS 握手被拒且 HTTP 状态 **514**（含 `autherrcode=1000040350`） | 永久 | 同上（连接数超限：单 app ≤50 条，本进程只持 1 条，正常不会触发） |
| 其余握手失败（5xx 等） | 暂时 | 按重连节奏 |

重连成功后：清空分片缓冲（旧连接的分片不再有效）；event_id 去重窗口**保留**
（服务端可能重投断线前事件）。

## 4. REST API 面

所有请求复用 `StdHttpClient`（native TLS 或 helper 回退，`SEKAI_HTTPS_MODE`
语义不变）。**新增依赖：无**。

### 4.1 tenant_access_token

`POST {domain}/open-apis/auth/v3/tenant_access_token/internal`，
body `{"app_id","app_secret"}`（无需鉴权头）。响应
`{"code":0,"tenant_access_token":"t-xxx","expire_in":7200}`（约 2 小时）。

- **内存缓存，不落盘**；`expire_in - 300s` 前提视为过期，惰性刷新。
- 收到 token 失效错误码（如 `99991663`/`99991661`，以对拍为准）：强制刷新
  后**重试一次**原请求，仍失败则按该消息 outcome 记审计。

### 4.2 发送与回复

| 用途 | 端点 | body |
| --- | --- | --- |
| 主动发送（仅 ack 与提示） | `POST /open-apis/im/v1/messages?receive_id_type=chat_id` | `{"receive_id":"<chat_id>","msg_type":"text","content":"{\"text\":\"...\"}"}` |
| 回复原消息（答案） | `POST /open-apis/im/v1/messages/{message_id}/reply` | `{"msg_type":"text","content":"{\"text\":\"...\"}"}` |

- 鉴权头 `Authorization: Bearer <tenant_access_token>`。
- 失败重试：仅对 **429/5xx** 指数退避（1s、2s）各重试一次；其余（4xx 权限类）
  不重试。重试耗尽 → stderr 审计 `outcome=reply_failed`，该消息处理结束
  （不重跑 Agent）。

## 5. 消息回路语义

### 5.1 事件过滤（进入回路的条件）

只处理 `im.message.receive_v1`（`header.event_type`）事件，其余事件类型
ack 后丢弃。

| 条件 | p2p（单聊） | group（群聊） |
| --- | --- | --- |
| 触发 | 任何文本消息 | **仅 @机器人**的消息（`event.message.mentions` 含本机器人；实现按「非空」判定，见 §14） |
| prompt 文本 | `content` JSON 的 `text` 去首尾空白 | 剥离 @ 片段后的剩余文本，去首尾空白 |
| `message_type != "text"` | 回复「V1 仅支持文本消息」，正常 ack | 同左 |
| 剥离后空文本 | 回复 usage 提示（「直接发文本，或 /status /cancel」） | 同左 |
| 文本 > 8192 字节（UTF-8） | 回复超限提示（与 `PromptCore` `invalid_arguments` 上限一致） | 同左 |

`event.message.content` 是 **JSON 字符串**，需二次解析取 `text`。

### 5.2 执行与回复

1. （可选）ack 消息：`已收到，正在处理…`（`SEKAI_FEISHU_ACK=off` 关闭；
   命令类消息 §5.3 不发 ack）。
2. 调 `PromptCore::run_prompt(text, SEKAI_FEISHU_MAX_STEPS)`（该调用内部
   已克隆策略并置 `deny_llm_relay=true`、过 RunGate 全局 4 路、5s 等槽后回
   `busy`）。
3. 回复映射（reply 到原 `message_id`）：

| run_prompt 结果 | 回复文本 |
| --- | --- |
| `Ok{status: Completed, answer}` | `answer`（无 final_text 时回「（无输出）」） |
| `Ok{status: MaxSteps, answer?}` | `（已达最大步数 n）` + answer（如有） |
| `Err llm_unavailable` | `LLM 未配置：需要 DEEPSEEK_API_KEY` |
| `Err not_linked` | `Worker 链路未建立` |
| `Err no_workers` | `暂无 Worker 在线，请稍后再试` |
| `Err agent_error` | `执行失败：<message>`（LLM/网关错误的 Display 文本，不含凭据） |
| `Err busy` | `系统繁忙，请稍后再试` |

4. 截断：回复文本 > **4000 字节**（UTF-8）时按字符边界截断到 ≤4000 字节并
   追加 `\n[[truncated]]`（风格同 `pc.*` 截断惯例）。
5. 每条消息（无论结局）一行 stderr 审计（§6.5）。

### 5.3 小命令层（不进 Agent）

`/` 开头的消息**不进 LLM**（先 trim；未知 `/` 命令回可用命令清单）：

| 命令 | 行为 |
| --- | --- |
| `/cancel` | 置位该 chat 的在途 `CancelToken`（复用 `agent::CancelToken`）；回复「已请求取消」；无在途运行时回复「当前没有运行中的任务」 |
| `/status` | 回复一行状态：链路类型与连接态、在线 Worker 数（id 列表，截断到前 5）、当前 chat 是否有在途运行、全局运行数/4 |

### 5.4 并发与串行

- **per-chat 串行，深度 1**：同 chat 在途时新消息回
  `上一条还在处理中，可发送 /cancel 中止`，不排队。
- 跨 chat：各自独立运行，总量受 PromptCore `RunGate`（4）约束；等待超 5s
  得到 `busy`（§5.2 已映射）。
- 每条消息运行在独立短线程（由 dispatch 线程 spawn），运行结束即回收；
  `CancelToken` per-chat 存于 `Mutex<HashMap<chat_id, CancelToken>>`，运行
  结束移除。

### 5.5 会话上下文与多轮记忆（Router 内）

长连接与事件泵都在 Router，因此 Router 直接读取飞书事件里的会话信息，并
在进程内维护按会话隔离的多轮记忆；两者在运行前注入 LLM（不改变 Worker 与
V1 帧协议）。

**元数据（每条消息都注入）**：`chat_id`、`chat_type`、`message_id`、
`thread_id`、`root_id`、`parent_id`、发送者 id、消息时间（`message.create_time`
原样字符串）。空字段省略。注入位置是 agent 的 system prompt 末尾
「## 当前会话（Router 注入）」段，经
[`ConversationContext`](../crate/sekai-router/src/agent.rs) 传递。

**多轮记忆（默认开启，可配置）**：

- 键：普通消息按 `chat_id`；话题消息（`thread_id` 非空）按
  `chat_id/thread_id`，避免群内多个话题串上下文。per-chat 串行仍按 `chat_id`。
- 只记录**已放行、非 `/` 命令、`run_prompt*` 返回 `Ok`** 的轮次；用户文本与
  实际回复文本（经 §5.2 的 4000 字节截断后）成对保存。取消、错误、忙拒、
  未授权、限流、非文本/空文本/超长提示均不记录。
- **先记录本轮、再 REST 回复**：保证同会话下一条消息一定看到上一轮；若回复
  发送失败（`reply_failed`），该轮仍保留在记忆里。
- 有界：单条文本截断到 4 KiB（超限追加 `\n[[truncated]]`）；单会话最近 N 轮且
  总量 ≤32 KiB（超出从最旧轮丢弃）；最多 256 个会话（LRU 淘汰）。
- **仅内存**：WebSocket 重连不丢；进程重启清空；不落盘；不写 stderr 审计、
  不写 trace（trace 只记录上下文轮数）。
- `SEKAI_FEISHU_CONTEXT_TURNS=0` 关闭历史（元数据仍注入）。

## 6. 安全模型

安全姿态对齐 prompt ingress（B9）与 CLI 既有惯例：**默认拒绝、凭据不落盘、
审计不记内容**。

### 6.1 allowlist（默认拒绝）

| 变量 | 形态 | 匹配 |
| --- | --- | --- |
| `SEKAI_FEISHU_ALLOW_USERS` | 逗号分隔 `open_id`（可混 `user_id`） | 消息 `sender.sender_id` 命中即放行（p2p） |
| `SEKAI_FEISHU_ALLOW_CHATS` | 逗号分隔 `chat_id` | `event.message.chat_id` 命中即放行（p2p 与群聊皆查） |

- 放行条件 = **user 命中或 chat 命中**（群聊场景配 chat_id 最顺手）。
- **禁止通配**：任一变量值含 `*` → 启动失败，退出码 2。
- **两者皆空**：启动打 stderr 警告（「allowlist 为空：所有消息将被拒绝」）
  但**不退出**——这是有意设计：用户需要先跑起来给自己发一条消息、从审计行
  抄到自己的 open_id/chat_id 再回填（[建连 skill](../skills/feishu-setup/SKILL.md)
  第 4 步依赖此路径）。空 allowlist 下所有消息回
  `未授权：本机器人未向你开放`。
- 未命中 allowlist：ack 事件、回复未授权提示（每 chat 60s 内只回一次，避免
  被骚扰刷屏）、审计 `outcome=denied`。

### 6.2 策略与防递归

- 策略投影**同 CLI**：`--allow-write`/`--allow-tasks`/`--allow-admin`/
  `--worker` 语义不变，默认全关（只读白名单）。`deny_llm_relay` 由
  `run_prompt` 内部强制置 true，飞书回路发起的运行内再派 `pc.ask` 一律
  `policy_denied`（与 ingress/serve 完全一致）。

### 6.3 凭据与脱敏

- `SEKAI_FEISHU_APP_ID`/`_APP_SECRET` 仅从环境变量读取；`FeishuConfig` 的
  Debug 实现 app_secret 显示 `<redacted>`（同 LLM key 惯例）。
- endpoint 响应的 `URL`、WS 连接 URL、Authorization 头**不得**出现在日志、
  trace、错误 Display、panic 消息中；错误只报类别与状态码。

### 6.4 去重与限流

| 机制 | 参数 | 风格来源 |
| --- | --- | --- |
| event_id 去重 | `VecDeque` + `HashSet` 有界窗口 **1024**（应对 3s 重投；断线重连不清空） | WorkerRegistry `DedupWindow`（256）同构 |
| 每 chat 速率 | **60 msg/min**（滑动窗计数） | ingress `DEFAULT_REQUESTS_PER_MINUTE` 同值 |
| 全局并发 | RunGate **4**（PromptCore 内建） | `MAX_CONCURRENT_PROMPT_RUNS` |

超速消息回 `请求过于频繁，请稍后再试`，审计 `outcome=rate_limited`。

### 6.5 审计行（stderr，每消息一行）

```text
sekai-router: feishu chat=<chat_id> sender=<open_id> cmd=<cancel|status|-> \
len=<prompt 字节数> outcome=<ok|max_steps|error:<code>|busy|denied|rate_limited|\
non_text|empty|too_long|reply_failed> ms=<处理毫秒>
```

**不记录消息内容与回复内容**（同 ingress peer/method/outcome 风格）；§5.5 的
会话上下文也只存在内存并送给 LLM，不进审计行与 trace（trace 只多
`context_turns` 计数）。

## 7. 代码组织与零依赖取舍（实现目标）

### 7.1 新增/修改文件

```text
crate/sekai-router/src/
  ws.rs      新增  RFC 6455 客户端子集（握手+Accept 校验、mask 帧编解码、
                 ping/pong/close、continuation 接收）
  sha1.rs    新增  SHA-1 + base64 标准编码（仅握手所需；各约百行，
                 含官方测试向量，见 §10）
  pbbp.rs    新增  pbbp2.Frame 的 protobuf wire 编解码（varint/length-
                 delimited 子集，自备实现，不依赖核心 crate 私有函数）
  feishu.rs  新增  endpoint 客户端、WS 事件泵（分片/ack/去重）、token 管理、
                 REST 发送/回复、dispatch 与 per-chat 运行、命令层
  http.rs    修改  暴露 TLS 裸流（`connect_tls_stream(host, port, timeout) -> Box<dyn ReadWrite + Send>`）
                 供 ws.rs 分帧读写，并给 `ReadWrite` 加超时设置方法；现有请求路径不动
  agent.rs   修改  `ConversationContext`/`ConversationTurn` 类型与
                 `Agent::with_conversation_context`（元数据段 + 历史消息注入）
  prompt.rs  修改  `conversation_context_block`（system 段渲染）
  prompt_server.rs 修改 新增 `run_prompt_with_cancel`（`/cancel` 挂 `CancelToken` 的最小增量）
                 与 `run_prompt_with_context`（飞书会话上下文）；`run_prompt` 行为不变；
                 `MAX_CONCURRENT_PROMPT_RUNS` 改 pub（`/status` 展示 /4）
  config.rs  修改  `--feishu` 旗标、互斥校验、SEKAI_FEISHU_* 解析
  bin/sekai-router.rs 修改  feishu 分支（建链路、构造 PromptCore、调 feishu::run）
```

`crate/sekai-worker` **不新增/不修改任何飞书相关代码**：长连接、事件泵、
会话上下文与回复全部在 Router（验收以 worker 目录无飞书依赖为准）。

### 7.2 线程模型（同步 std，无 tokio）

| 线程 | 职责 |
| --- | --- |
| 主线程 | 配置解析、PromptCore 构造、信号处理、退出码 |
| WS IO 线程 | 阻塞读帧（`read_timeout` 切分：超时时检查是否该发 ping / 是否 2×interval 无 pong）；分片重组；**先 ack 再投递** |
| dispatch 线程 | 从 mpsc 取完整事件：allowlist/命令/限流判定，per-chat 串行 spawn 运行线程 |
| 运行线程（短命） | `run_prompt_with_context`（元数据 + 历史），Ok 后先写回历史，再 REST 回复 + 审计行，结束即退 |

### 7.3 为什么不引入现成库（对照表）

| 备选 | 取舍理由 |
| --- | --- |
| `tokio-tungstenite`/`tungstenite` | 引入 tokio 运行时或额外 TLS 栈，与全仓同步 std 线程模型冲突；所需仅为客户端子集（§3.2 表），自研面 ~300 行 |
| `feishu-ws` / 官方·社区 Rust SDK | 携带 reqwest/tokio 全家桶与超出所需的 REST 面；协议事实已固化本文 §3，可直接对拍官方 SDK 验证 |
| 做成 DSH 插件进程 | 耦合 DSH 形态（需 node 侧桥接），失去独立常驻服务定位；DSH 用户仍可由 [skill](../skills/feishu-setup/SKILL.md) 引导使用本形态 |

新增 crate 依赖：**0**（`sha1`/`base64`/`ws`/`pbbp` 全部仓内实现；TLS 复用
`native-tls` 既有 feature）。

## 8. 配置面全表

| 旗标 / 环境变量 | 含义 | 默认 | 校验 |
| --- | --- | --- | --- |
| `--feishu` | 启用飞书前端 | 关 | 与 `--serve/--tui/--goal/--goal-file/--dry-run/-i` 互斥 |
| `SEKAI_FEISHU_APP_ID` | 应用 ID（`cli_` 开头） | 无 | feishu 形态**必需**，缺失退出码 2 |
| `SEKAI_FEISHU_APP_SECRET` | 应用密钥 | 无 | 同上；Debug 脱敏 |
| `SEKAI_FEISHU_DOMAIN` | 开放平台域名 | `https://open.feishu.cn` | 必须为 `https://`（Lark 用 `https://open.larksuite.com`） |
| `SEKAI_FEISHU_ALLOW_USERS` | 允许的用户 id 列表 | 空（全拒） | 禁 `*`；非空项 trim 后非空 |
| `SEKAI_FEISHU_ALLOW_CHATS` | 允许的会话 id 列表 | 空（全拒） | 同上 |
| `SEKAI_FEISHU_ACK` | 收到消息先回「正在处理」 | `on` | `on`/`off` |
| `SEKAI_FEISHU_MAX_STEPS` | 每消息 agent 步数上限 | 不覆盖（随全局；feishu 默认 tcp = 直连 PC Worker → 不设上限） | `1..=32`（同 `MAX_PROMPT_STEPS`） |
| `SEKAI_FEISHU_CONTEXT_TURNS` | 每会话保留的最近问答轮数（0 = 只注入元数据，关闭多轮记忆） | `8` | `0..=50`；非法配置退出码 2 |
| 复用 | `DEEPSEEK_API_KEY`、`--link`/`SEKAI_LINK`（feishu 默认 tcp）、`SEKAI_LISTEN_ADDR`、`--allow-write/--allow-tasks/--allow-admin`、`--worker`、`SEKAI_MAX_STEPS`、`SEKAI_ROUTER_TRACE`、`SEKAI_HTTPS_MODE` | — | 语义不变 |

## 9. 飞书侧配置与引导 skill

本文是**权威参考**；**交互式引导**由 [skills/feishu-setup/SKILL.md](../skills/feishu-setup/SKILL.md)
承担（加载该 skill 的 agent 逐步带用户走完建连，每步检查点验证）。两者分工：
skill 引用本文的表（不复制协议常量，避免漂移），本文不重复 skill 的对话式细节。

飞书开放平台侧步骤概要（细节与最新 UI 以控制台为准）：

1. 开发者后台创建**企业自建应用**；「凭证与基础信息」页取 App ID / App Secret。
2. 「添加应用能力」→ **机器人**。
3. 「事件与回调」→ 订阅方式选 **使用长连接接收事件**（不填回调地址）。
4. 订阅事件 **`im.message.receive_v1`**（接收消息）。
5. 「权限管理」开通：
   - 接收：`im:message.p2p_msg:readonly`（单聊）与/或
     `im:message.group_at_msg:readonly`（群 @），或合并的 `im:message`；
   - 发送：`im:message:send_as_bot`。
6. 「版本管理与发布」创建版本并发布（可能需企业管理员审核；未发布的应用
   事件不投递）。
7. 取允许名单 id：启动 `sekai-router --feishu`（allowlist 可为空），给自己/
   群发一条消息，从 stderr 审计行抄 `chat=`/`sender=`（open_id 以 `ou_`
   开头、chat_id 以 `oc_` 开头）。

## 10. 测试策略

全仓惯例：**无网络、无外部服务、无 broker**（`cargo test --workspace` 直跑）。

| 层 | 内容 |
| --- | --- |
| 单测 · sha1/base64 | SHA-1 FIPS 180 官方向量（`abc`→`a9993e36…`、空串→`da39a3ee…` 等）；base64 RFC 4648 向量 |
| 单测 · ws.rs | 帧编解码：7/16/64 位长度边界、mask 生成与校验、ping/pong/close、continuation 重组、握手 Accept 计算（RFC 6455 示例向量） |
| 单测 · pbbp.rs | Frame golden 字节：**实现期从官方 SDK 提取**（WP2 验收项，含一条完整事件帧与一条 ping 帧）；未知字段跳过；varint 边界 |
| 单测 · feishu.rs | 分片重组（TTL/乱序）、ack 帧、去重窗口、截断（4000 字节含多字节字符边界）、allowlist 判定、命令解析；会话字段解析（thread/root/parent/时间）、`session_key`、历史轮数/字节预算/会话数上限、`context_turns=0` |
| 单测 · prompt/agent | `conversation_context_block` 渲染（必需/可选字段、历史不重复进 system）；`Agent::with_conversation_context` 的 messages 顺序（system+元数据 → 历史 user/assistant → 当前 user） |
| 单测 · config | `SEKAI_FEISHU_CONTEXT_TURNS` 默认 8、0/3/50 生效、51 与非整数报配置错误 |
| 集成 | `tests/feishu_mock.rs`：本地 `TcpListener` 假 WS 服务端（完成握手、推事件帧、收 ack）+ 假 HTTP 服务端（endpoint/token/messages/reply），驱动：授权拒绝/放行、去重（重投同 event_id）、限流、忙拒、/cancel、/status、分片、断线重连、ack 开关、同会话多轮上下文、跨会话隔离、`context_turns=0`（15 例） |
| 真连冒烟 | `#[ignore]` + `SEKAI_REAL_FEISHU=1`（真 App ID/Secret）跑建连→干净退出（先例：`real_api.rs`）；CI 不跑 |

## 11. 工作包拆分与顺序

| WP | 内容 | 验收标准 |
| --- | --- | --- |
| WP1 | `sha1.rs` + `ws.rs`（握手、帧、ping/pong、close） | §10 单测全绿；能对任一公网 wss echo 端点完成握手（手动） |
| WP2 | `pbbp.rs` + endpoint 客户端 + WS 事件泵（分片/ack/去重/重连） | golden 字节对拍官方 SDK 一致；mock 集成通过 |
| WP3 | token 管理 + REST 发送/回复（含 429 退避） | mock HTTP 集成通过；token 提前刷新有单测 |
| WP4 | 消息回路（过滤/执行/回复映射/截断/命令层）+ 安全（allowlist/限流/审计） | §5/§6 全表行为有集成测试覆盖 |
| WP5 | `config.rs`/bin 接入（旗标/互斥/默认 tcp/退出码） | `--help` 文案、冲突退出码 2、横幅输出 |
| WP6 | 测试收尾：`tests/feishu_mock.rs` 全场景 + `#[ignore]` 真连冒烟 | `cargo test --workspace` 全绿；真连冒烟手工跑通 |
| WP7 | 文档收尾：本文状态改「现行」+ 实现基线校准；[sekai-protocol.md](sekai-protocol.md) §0.1 带外清单补一行；[skill](../skills/feishu-setup/SKILL.md) 前置检查改为版本要求 | 文档与源码一致（写作约定） |

> 2026-11-07：WP1~WP7 全部完成，`cargo test --workspace` 全绿；测试分布见 §10。

## 12. 边界与后续（不进 V1）

- 上下文的**跨重启持久化**与从飞书历史消息 API（`GET /open-apis/im/v1/messages`）
  回填（V1 为进程内、按会话最近 N 轮；见 §5.5）。
- `post` 富文本 / 交互卡片回调（card 帧已明确忽略）。
- Webhook 回调模式（需公网可达 + 验签，公网部署场景再评估）。
- 图片/文件/语音消息。
- 群自定义机器人 webhook（仅推送方向、无事件，形态不同）。
- REPL 内 `/feishu` 引导向导（先例 `/start`；skill 已覆盖引导需求）。
- 主动推送方向（定时任务把结果推到 chat；`messages` 端点已具备能力）。

## 13. 风险与开放问题

| 风险 | 缓解 |
| --- | --- |
| 长连接协议无正式规范，随官方 SDK 演进（帧 schema、错误码、限制） | 本文 §3 注明出处与对拍责任；WP2 golden 字节 + 真连冒烟；不一致时以官方为准回改本文（写作约定：文档尽快跟源码/现实） |
| 飞书控制台 UI/权限项改名或改版 | §9 只列稳定锚点（能力名、事件名、权限 scope 名），skill 引用本文不复制；控制台改版时只改 §9 与 skill |
| 出站需直连 443（native-tls），经 HTTP 代理的 WS 隧道 V1 不支持 | §12 列后续；部署文档（skill 第 0 步）提示网络要求 |
| 企业内创建/发布应用需管理员权限 | skill 第 3 步设检查点提示走审批；不影响个人测试版可用性 |
| 真机验证依赖真实飞书账号 | `#[ignore]` 冒烟隔离；WP6 手工验收 |

## 14. 实现基线校准（实现期与设计的差异）

实现以本文协议面为准；下列条目是**实现期确定的调整**（文档已回改，源码是实现基线）：

| 差异 | 设计原文 | 实现 | 理由 |
| --- | --- | --- | --- |
| 优雅退出 | Ctrl-C / SIGTERM / stdin EOF → 关 WS、cancel 在途、退出码 0 | Ctrl-C/SIGTERM 由 OS 默认终止；**不读 stdin**；内部 `stop` 标志用于收尾（测试/未来信号接入） | 本 crate `forbid(unsafe_code)` 无法注册信号处理器；服务管理器常把 stdin 接 `/dev/null`，stdin EOF 作退出信号会误退 |
| ack 粒度 | 「收到完整事件 DATA 帧」后回 ack | **每个** `type=event` 的 DATA 帧（含分片每一片）立即回 ack，再重组投递 | 分片事件若不逐片 ack，未 ack 的片会被服务端重投 |
| WS IO 线程归属 | 表格列「WS IO 线程」独立于主线程 | `feishu::run` 的调用线程即 IO 线程（重连循环）；dispatch 与运行线程独立 spawn | 退出码由重连循环直接决定，少一次线程交接；线程职责不变 |
| `ws://` 支持 | 只处理 `wss://` | `ws.rs` 同时支持 `ws://`（无 TLS 直连），生产 endpoint 返回 `wss://` 时走 TLS | 本地 mock 集成测试需要；不增加生产面风险（URL 只来自受信 endpoint） |
| `prompt_server.rs` 增量 | §7.1 未列 | 新增 `pub run_prompt_with_cancel`（`/cancel` 必需）与 `pub MAX_CONCURRENT_PROMPT_RUNS` | 原 `run_prompt` 不接受取消令牌，无法实现 `/cancel` |
| `/status` 全局运行数 | 「全局运行数/4」 | 本入口活跃运行线程数/4（PromptCore 的 RunGate 计数未对外开放） | 不扩大 prompt 通道公开面；飞书入口即 V1 主要带外来源 |
| 群聊触发判据 | `mentions` 含本机器人 | 依赖 Feishu 仅向机器人投递被 @ 的群消息 + `mentions` 非空静态判定，并剥离所有 @ 占位符 | 事件载荷不含机器人 open_id，无法静态比对。**若应用被授予 `im:message`（群内全部消息），含 @ 他人的群消息也会被当作触发**：V1 请按 §9 只授 `im:message.group_at_msg:readonly`；上线验证见 §9 第 4 步实操 |
| 内存上限 | 未规定 | WS 单帧 ≤16 MiB（`ws.rs`）；分片重组 ≤4 MiB/事件；ack/分片缓冲随连接重建清空 | 防异常/恶意事件撑爆常驻进程 |
| 退出码 4 | HTTPS helper 缺失或运行期 `llm_unavailable` 致命场景 | 仅当构建未启用 `tls` feature（无法承载 `wss://`）时返回 4；运行期 `llm_unavailable` 按 §5.2 回复「LLM 未配置」，不退出 | 常驻服务不因缺 API key 退出；缺 TLS 属于部署/构建错误，启动即失败最直观 |
| 链路建立失败 | 退出码 3 仅列飞书链路 | Worker 链路建立失败同样退出码 3（沿用批处理约定）；**建链后不等待 Worker 上线**即进入事件泵 | 常驻服务应能先于 Worker 启动（建连 skill 第 4 步先起 Router 取 chat_id）；无 Worker 时消息回「暂无 Worker 在线」，运行期断开回「Worker 链路未建立」 |
| 心跳实现 | §3.4：应用层 CONTROL `{type:"ping"}`/`{type:"pong"}` | 发 **RFC 6455 协议层 ping**，收到协议层 **pong** 刷新存活时间（`WsMessage::Pong`）；应用层 CONTROL pong 的解析保留（兼容/未来） | 2026-11-07 真连实测：飞书网关按下发的 `PingInterval`（实测 90s）对 WS ping 回 WS pong；此前仅按应用层 pong 刷新存活时间，导致每 2×PingInterval 误判断线重连一次 |
| 会话上下文与多轮记忆 | §1/§12 原列「每条消息独立目标（不保留跨消息上下文）」 | V1.1 增加进程内 per-session 元数据注入 + 最近 N 轮问答（§5.5）；`SEKAI_FEISHU_CONTEXT_TURNS` 默认 8、0 关闭；不新增飞书权限/API | Router 自身即长连接与事件泵所在处，可直接读取会话信息；用户明确要求 Router 能读取飞书会话上下文。跨重启持久化/历史 API 回填仍不做（§12） |

## 15. 修订记录

| 日期 | 变更 |
| --- | --- |
| 2026-11-06 | 初版：设计冻结（长连接模式、仅文本、企业自建应用；建连引导 skill 同步产出） |
| 2026-11-07 | 实现落地：状态改「现行」；补 §14 实现基线校准；§2 生命周期、§3.5 ack 粒度、§7.1 文件清单按实现回改；协议文档 §0.1 补飞书带外条目 |
| 2026-11-07 | V1.1：新增 §5.5 会话上下文与多轮记忆（Router 内进程级、`SEKAI_FEISHU_CONTEXT_TURNS`）；§1 范围、§6.5、§7.1/§7.2、§8、§10、§12、§14 同步 |
