# sekai

Sekai 核心协议库：V1 控制平面帧编解码与 Worker 运行时。`no_std`（只依赖
`core` + `alloc`，`#![forbid(unsafe_code)]`，**零外部依赖**），同一份代码同时
编译进宿主测试、PC Worker 与 ESP32 固件——因此本库不含任何 IO：传输、时钟、
日志全部由调用方注入。

## 模块

| 模块 | 内容 |
| --- | --- |
| `sekai_wire` | 线级常量与校验（协议版本、`MAX_DDL_MS`、`worker_id` 合法性等） |
| `sekai_stdin` / `sekai_stdout` | Router→Worker 命令帧 / Worker→Router 事件帧的编解码 |
| `sekai_runtime` | Worker 端 `Runtime`：消费 Stdin、驱动 Task/ToolCall、产出 Stdout（`RuntimeConfig`、`LinkAction`） |
| `sekai_task` / `sekai_tool` | Task（协作式长任务）与 ToolCall（同步短调用）的注册面 |
| `sekai_device_list` | Worker 内置跳转目标列表（`DeviceList`/`Endpoint`，最多 8 项） |
| `sekai_clock` | 时钟抽象（单调毫秒），由启动层提供实现 |
| `mqtt` | MQTT 5.0 客户端状态机：`codec`/`packet`/`property`/`reason`/`qos`/`topic`/`varint` 与 `client`（config/session/event） |

## Features

| feature | 作用 |
| --- | --- |
| （默认无） | 上述全部模块，宿主与固件通用 |
| `send` | 注册表强制工具/任务处理器 `Send`（异步运行时前置） |
| `esp32` | 等价 `send`，供 ESP32 Embassy 任务使用 |
| `x86` | 预留的宿主标记 |

## 帧格式速览

```text
Stdin / Stdout 帧：协议版本字节 = 1；多字节整数一律大端；字符串 = u16 长度 + UTF-8。
承载：MQTT 一条 PUBLISH = 恰好一帧；直连链路 = u32 大端长度前缀 + 一帧。
去重：Router 按 (worker_id, boot_id, seq) 去重 Stdout。
```

完整规范（帧布局、命令/事件/错误码、QoS/retain、Hello/Offline）见
[docs/sekai-protocol.md](../../docs/sekai-protocol.md)，本文档以
`src/sekai_wire.rs`、`src/sekai_stdin.rs`、`src/sekai_stdout.rs` 等源码为基线。

## 测试

`tests/` 覆盖协议往返（`mqtt_codec_v5`、`mqtt_v5`）、Runtime 行为
（`sekai_runtime`）、文档示例（`docs_examples`）与 `send` 特性。宿主直接
`cargo test -p sekai` 即可，无需网络与 broker。
