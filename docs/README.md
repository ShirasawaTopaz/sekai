# Sekai 文档索引

本目录是 Sekai 的正式文档：所有协议常量、引脚表、命令行行为均以对应源码为实现基线
（每篇开头的「实现基线」指明了对哪些文件负责）。项目总览与快速开始见
[根 README](../README.md)。

## 文档一览

| 文档 | 一句话简介 | 状态 |
| --- | --- | --- |
| [sekai-architecture.md](sekai-architecture.md) | Router/Worker 总体架构：角色与 Crate 划分、通信矩阵、阶段计划、与 DSH superagent 架构的对照 | 现行 |
| [sekai-protocol.md](sekai-protocol.md) | V1 控制平面协议全规范：编码约定、Stdin/Stdout 帧布局、命令/事件/错误码、MQTT 主题与 QoS/retain、Hello/Offline、服务端契约 | 现行（V1 冻结） |
| [sekai-pc-agent.md](sekai-pc-agent.md) | PC agent：`pc.*` 工具/任务全表、prompt JSONL 协议（三方通用）、Router/Worker/插件配置清单、安全与防递归 | 现行 |
| [sekai-gpio.md](sekai-gpio.md) | GPIO 抽象与 `gpio.*` 用户工具：家族引脚表、能力/保留标志、四个工具的语法、错误映射 | 现行（`--gpio` opt-in） |
| [sekai-esp32.md](sekai-esp32.md) | ESP32 固件指南：Xtensa 工具链安装、`sekai-build` 构建/烧录、WiFi/MQTT 参数、最小链路与排障 | 现行（阶段置后） |
| [sekai-dsh-plugin.md](sekai-dsh-plugin.md) | Sekai × DSH 插件：双插件形态、serve JSONL 协议概览、安装配置 | 现行 |
| [sekai-ai.md](sekai-ai.md) | AI 控制端（`crate/sekai-ai`/`sekai-agent`）设计与验收记录 | **历史**（角色现由 `crate/sekai-router` 承担） |
| [sekai-ai-plan.md](sekai-ai-plan.md) | AI 控制端的计划文档 | 历史 |
| [sekai-gpio-plan.md](sekai-gpio-plan.md) | GPIO 抽象的计划文档 | 历史 |

另有两份独立的 README 不在本目录：

- [plugins/sekai-dsh/README.md](../plugins/sekai-dsh/README.md) — DSH 插件 bundle
  的配置全量说明（安装、config 键表、JSONL 协议 v1、生命周期容错、本地冒烟）。
- [crate/sekai-embedded/README.md](../crate/sekai-embedded/README.md) — 嵌入式 crate
  说明，含 esp 工具链的诊断记录与恢复步骤。

## 按角色阅读

**新手入门（跑通闭环）**

1. [根 README](../README.md) 的快速开始 —— 构建、REPL、Worker 接入。
2. [sekai-architecture.md](sekai-architecture.md) —— 理解 Router/Worker 分工与
   直连链路；§2.1 的五种运行形态命令一览。

**固件 / 板级开发者**

1. [sekai-protocol.md](sekai-protocol.md) —— 帧格式与 Worker 侧义务（去重、
   boot_id、Hello 生命周期）。
2. [sekai-gpio.md](sekai-gpio.md) —— 引脚表、后端 trait、`gpio.*` 工具注册。
3. [sekai-esp32.md](sekai-esp32.md) + [crate/sekai-embedded/README.md](../crate/sekai-embedded/README.md)
   —— 工具链、构建烧录、已知缺口。

**上位机 / Router 实现者**

1. [sekai-protocol.md](sekai-protocol.md) —— 服务端契约（cmd_id 生命周期、
   去重窗口、晚到事件、broker ACL）。
2. [sekai-architecture.md](sekai-architecture.md) —— Crate 依赖方向与阶段计划。
3. [sekai-ai.md](sekai-ai.md) —— 历史设计记录：LLM 循环、策略、trace 的原始
   决策（现实现于 `crate/sekai-router`）。

**DSH / AI 集成者**

1. [sekai-dsh-plugin.md](sekai-dsh-plugin.md) —— 双插件形态与职责划分。
2. [plugins/sekai-dsh/README.md](../plugins/sekai-dsh/README.md) —— config 键、
   工具面、JSONL 协议 v1 细节。
3. [sekai-pc-agent.md](sekai-pc-agent.md) —— 模型可调用的 `pc.*` 语法与 prompt
   JSONL 协议；`--gpio` 开启后另见 [sekai-gpio.md](sekai-gpio.md) 的 `gpio.*` 语法。

## 写作约定

- 文中「必须/不得」为硬性要求；「应该/建议」为约定（违背可能影响互通）；「可以」
  为实现自由。
- 术语：**Router**（原 server）负责评估派工；**Worker**（原设备，`device_id` 等
  线级标识符保留原名）负责执行上报。
- 文档与源码不一致时，以源码为准，并尽快修文档。
