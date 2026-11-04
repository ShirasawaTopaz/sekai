# sekai-gpio

Sekai 家族无关 GPIO 抽象与 `gpio.*` 用户工具。`no_std` + `alloc`，默认构建
**不依赖任何 HAL**（虚拟后端即可用），真实硬件后端按 feature 拉起。协议层保持
不变：GPIO 是普通用户工具，参数与结果都是 UTF-8 文本，经 Hello 正常通告。

## 提供什么

- **引脚标识与解析**（`pin`）：ESP32 的 `IO4`/`GPIO4`、STM32 的 `PA5`，
  大小写不敏感、规范化输出。
- **配置状态机**（`config`）：方向、电平、内部上/下拉、开漏；支持部分更新
  （只改显式给出的字段）。
- **家族引脚表**（`family`）：ESP32 classic、STM32F103、STM32F407 的静态
  `PinSpec` 表——能力位（输入/输出/上拉/下拉/开漏）与特殊标志（保留、仅输入、
  strapping、UART0、PSRAM、SWD、JTAG、晶振、BOOT）。保留引脚一律拒绝操作。
- **后端抽象**（`port::GpioPort`）与三种实现：
  - `virtual_gpio`：宿主虚拟后端，三种家族引脚表可选，无硬件端到端验证用；
  - `esp32`（feature）：esp-hal 1.2.2 `Flex` 真实后端；
  - `stm32f1` / `stm32f4`（feature）：PAC 寄存器直接操作（F103C8 / F407VG）。
- **用户工具**（`tools`）：`gpio.info` / `gpio.config` / `gpio.read` /
  `gpio.write`，注册进 Worker 运行时，AI 可直接经 ToolCall 调用；
  `gpio.info help` 返回固定英文自描述文本，模型无需外部文档即可学会语法。

```text
槽位（slot）约定：ESP32 槽位 = IO 编号（0..=39）；STM32 槽位 = 端口序号 × 16 + 引脚号。
```

## Features

| feature | 作用 | 拉起 |
| --- | --- | --- |
| （默认无） | 抽象 + 虚拟后端 + 工具，无 HAL 依赖 | — |
| `send` | 工具共享句柄切换为 `Arc<critical_section::Mutex<…>>`（异步运行时前置） | `critical-section` |
| `esp32` | ESP32 classic 真实后端（隐含 `send`） | `esp-hal`（unstable） |
| `stm32f1` | STM32F103C8 PAC 后端 | `stm32f1` |
| `stm32f4` | STM32F407VG PAC 后端 | `stm32f4` |

## 工具速览

| 工具 | 参数文本示例 | 说明 |
| --- | --- | --- |
| `gpio.info` | `help` / `list` / `IO4` | 自描述 / 家族与引脚一览 / 单引脚详情 |
| `gpio.config` | `IO4 output high` | 部分更新方向/电平/上下拉/开漏 |
| `gpio.read` | `IO4` | 读电平（含输入方向） |
| `gpio.write` | `IO4 high` | 写电平（须已配置为输出） |

参数语法、错误映射（协议 `ErrorCode`）与 AI 调用流程详见
[docs/sekai-gpio.md](../../docs/sekai-gpio.md)（以本 crate 源码为基线）。
测试覆盖工具语法（`tests/tools.rs`）、规范一致性（`spec_conformance.rs`）、
虚拟后端（`virtual_backend.rs`）与文档示例（`docs_examples.rs`）。
