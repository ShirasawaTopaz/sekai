---
name: feishu-setup
description: 在用户想把飞书（Feishu/Lark）接入 sekai-router 作为对话入口时使用——从零引导完成飞书企业自建应用创建、机器人能力、长连接事件订阅、权限申请、allowlist 配置，直到 sekai-router --feishu 收发验证通过。涉及 SEKAI_FEISHU_* 环境变量、--feishu 启动、飞书建连排障时加载。
---

# Sekai × 飞书建连引导

带你（agent）把用户的飞书接到 `sekai-router --feishu`。**权威参考是
[docs/sekai-feishu.md](../../docs/sekai-feishu.md)**（下称「设计文档」）：
协议常量、错误分类、配置表一律引用它，**不要复制数值到对话里再转述**；
本 skill 只定义引导流程与检查点。

执行原则：

- **逐步推进，检查点不过不进**：每步末尾有「检查点」，向用户确认或亲自验证
  通过后才进入下一步；失败则按「排障表」处理后重试本步。
- **凭据卫生**：App Secret、API key 绝不让用户粘贴到对话里；让用户自己
  在终端设置环境变量，你只校验「已设置且非空」（如
  `[ -n "$SEKAI_FEISHU_APP_SECRET" ]`），不回显值。
- 用户界面为飞书开放平台控制台（浏览器，用户操作）；本机操作（构建、
  启动、看日志）由你执行或给出可直接复制的命令。
- 用户全程在自己设备上操作，你不得代替用户在飞书控制台点击。

## 第 0 步：前置检查

1. Router 二进制支持该形态：`sekai-router --help`（按用户实际 PATH 或
   `target/release/sekai-router`）中**存在 `--feishu`**。
   - 不存在 → 该二进制版本**过旧**：飞书入口自 0.2.0 之后实现（见
     [CHANGELOG](../../CHANGELOG.md) 的 Unreleased 段）。让用户升级到包含
     `--feishu` 的版本（仓库内构建：`cargo build --release`），**不要**按
     设计文档手工实现该特性。
   - 存在但启动即报错 → 转「排障表」。
2. `DEEPSEEK_API_KEY` 已设置（飞书回路会真正调 LLM）。
3. 出站可直连 443（`https://open.feishu.cn`；Lark 用户为
   `https://open.larksuite.com`）。注意：V1 不支持经 HTTP 代理的 WebSocket。
4. 询问用户目标域名（国内飞书 / 海外 Lark）与使用场景（单聊 / 群聊 @ 机器人），
  后续步骤据此裁剪。

**检查点 0**：`--feishu` 在 help 中、API key 已设、443 可达。

## 第 1 步：创建飞书企业自建应用

引导用户在[飞书开放平台开发者后台](https://open.feishu.cn/app)（Lark：
<https://open.larksuite.com/app>）：

1. 「创建企业自建应用」，取名（如 `sekai-router`）。
2. 进入「凭证与基础信息」，让用户**自己**把 App ID / App Secret 设为环境
   变量（给出可复制的模板，值留空让用户填）：

   ```bash
   export SEKAI_FEISHU_APP_ID="cli_..."      # Windows PowerShell: $env:SEKAI_FEISHU_APP_ID="cli_..."
   export SEKAI_FEISHU_APP_SECRET="..."      # 同上
   ```

**检查点 1**：App ID 以 `cli_` 开头且已设入环境；Secret 已设且非空（不回显）。

## 第 2 步：机器人能力 + 长连接事件订阅 + 权限

引导用户在应用内（菜单名以控制台实际为准，锚点见设计文档 §9）：

1. 「添加应用能力」→ **机器人**。
2. 「事件与回调」→ 订阅方式选 **使用长连接接收事件**（不要填回调 URL）。
3. 添加事件：**`im.message.receive_v1`**（接收消息）。
4. 「权限管理」开通（按第 0 步场景裁剪）：
   - 单聊接收：`im:message.p2p_msg:readonly`；
   - 群聊接收：`im:message.group_at_msg:readonly`（或合并的 `im:message`）；
   - 发送：`im:message:send_as_bot`。
5. 让用户逐项回报勾选结果，你对照核对 scope 名。

**检查点 2**：机器人能力已添加；订阅方式为长连接；事件列表含
`im.message.receive_v1`；权限页含上述 scope。

## 第 3 步：发布应用

1. 「版本管理与发布」→ 创建版本 → 申请发布。
2. 视企业设置可能需管理员审核——提示用户找管理员放行；未发布的应用
   **不会投递事件**。

**检查点 3**：应用版本状态为「已发布 / 生效中」。

## 第 4 步：启动 Router，取 open_id / chat_id

此步**故意允许 allowlist 为空**（设计文档 §6.1：空 allowlist 拒绝所有消息
但不退出，正是为了走这一步）。

1. 你启动（临时前台任务或后台进程）：

   ```bash
   sekai-router --feishu --link tcp
   ```

   等待 stderr 出现 `飞书长连接已建立`（失败 → 排障表）。
2. 让用户**在飞书里给机器人发一条消息**（单聊直接发；群聊先拉机器人入群
   再 @ 它）。allowlist 为空，机器人会回「未授权」——这是预期行为。
3. 从 stderr 审计行抄录：`chat=<chat_id>` 与 `sender=<open_id>`
   （`ou_` 开头是用户，`oc_` 开头是会话）。
4. 停掉该进程。

**检查点 4**：拿到用户的 `open_id`（及群聊场景的 `chat_id`），进程已停。

## 第 5 步：配置 allowlist 并正式启动

1. 让用户设置（或写入其常用 profile/启动脚本）：

   ```bash
   export SEKAI_FEISHU_ALLOW_USERS="ou_..."          # 单聊：用户 open_id
   export SEKAI_FEISHU_ALLOW_CHATS="oc_..."          # 群聊：群 chat_id（可不要）
   ```

   - 只用单聊 → 仅设 `ALLOW_USERS`；群聊授权 → 设 `ALLOW_CHATS`（整群放行，
     群内仍只有 @ 机器人的消息会触发，见设计文档 §5.1）。
   - **不得使用 `*`**（启动会直接失败）。
2. 重新启动 `sekai-router --feishu`，确认无「allowlist 为空」警告。

**检查点 5**：allowlist 已设且不含 `*`；启动横幅显示 `allow_users/allow_chats`
数量 ≥1。

## 第 6 步：端到端验证

让用户在飞书依次发送，你盯 stderr 审计行核对 outcome：

1. `/status` → 机器人回链路/Worker/运行状态一行。
2. 一条自然语言目标（如 `列一下当前目录有哪些文件`）→ 先收
   「已收到，正在处理…」（若 `SEKAI_FEISHU_ACK=on`，默认开），再收答案
   reply；审计行 `outcome=ok`。
3. （可选）长任务运行中发 `/cancel` → 回「已请求取消」。

**检查点 6**：两轮对话均收到正确回复；审计行无 `denied`/`error:`/`busy`。

全部通过 → 建连完成。向用户交代：进程常驻（Ctrl-C 退出）；环境变量建议
固化到启动脚本；安全提醒（allowlist 保持最小、Secret 不入库不进聊天记录、
默认只读策略如何按需解锁——见设计文档 §6）。

## 排障表

症状 → 定位（细节一律查设计文档对应节）：

| 症状 | 定位 | 参考 |
| --- | --- | --- |
| 启动即退，退出码 3，stderr 提到 403 / 514 / `1000040350` | 凭据错 / 连接数超限（永久拒绝） | §3.6 |
| endpoint 报 `code=1`/`1000040343` 后重试不停 | 开放平台暂不可用，等或查公告 | §3.1 |
| 退出码 2 | 配置错：缺 APP_ID/SECRET、旗标冲突、allowlist 含 `*` | §8 |
| `飞书长连接已建立` 但发消息无任何反应 | 应用未发布（第 3 步）/ 事件未订阅 / 没选长连接 / 权限缺失 | §9、§3.5 |
| 收到「未授权」 | allowlist 未含该用户/会话 → 回第 5 步 | §6.1 |
| 回复 `LLM 未配置` | `DEEPSEEK_API_KEY` 未设 → 回第 0 步 | §5.2 |
| 回复 `Worker 链路未建立` / `暂无 Worker 在线` | 起一个 Worker：`sekai-worker connect <router tcp 地址>`（默认 `127.0.0.1:7421`） | §2 |
| 回复 `系统繁忙` | 全局 4 路并发满，稍后重试即可 | §5.4 |
| 群里 @ 没反应、单聊正常 | 缺 `im:message.group_at_msg:readonly` 权限，或消息未真正 @ 到机器人 | §9 |

## 边界（主动告知用户）

V1 仅支持**文本消息**、不支持图片/卡片交互、不支持经 HTTP 代理的连接。
同会话多轮记忆默认开启：Router 在**内存**中按会话保留最近
`SEKAI_FEISHU_CONTEXT_TURNS` 轮（默认 8，0 关闭），进程重启清空、不落盘；
其余后续方向见设计文档 §12。
