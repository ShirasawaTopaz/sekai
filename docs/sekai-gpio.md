# Sekai GPIO 抽象与 gpio.* 用户工具

本文描述 `crate/sekai-gpio`：家族无关的 GPIO 抽象、ESP32/STM32F1/F4 引脚表、宿主虚拟后端，
以及 AI 通过 MQTT ToolCall 直接调用的四个用户工具 `gpio.info`/`gpio.config`/`gpio.read`/`gpio.write`。

- **读者**：固件/板级工程开发者、需要给 AI 暴露 GPIO 能力的集成者、调试 GPIO ToolCall 的工程师。
- **实现基线**：`crate/sekai-gpio/**`、`crate/sekai-worker`（PC Worker 宿主驱动，原 `crate/sekai-start/src/driver/x86.rs`）、
  `crate/sekai-embedded/src/driver/esp32/**`；
  计划与设计背景见 `docs/sekai-gpio-plan.md`。文中所有引脚表、工具文本、错误码均以这些源码为准。
- **约定**：本文中的“必须/不得”表示实现硬性要求；“应该/建议”表示推荐做法；“可以”表示实现自由选择。
- **相关文档**：`docs/sekai-protocol.md`（帧格式、错误码、Hello、broker ACL 等协议层规则）、
  `docs/sekai-esp32.md`（ESP32 固件构建、烧录、配网与故障排查）。

> 核心设计约束：**协议层不变**。GPIO 是普通用户工具，不是 `sekai.*` 内置工具；参数与结果都是
> UTF-8 文本，对协议层不透明；本轮不新增帧类型、错误码或 `content_type`。

> **重构注记（Router/Worker）**：原 `crate/sekai-start` 已拆分为 `crate/sekai-worker`（PC Worker，
> 宿主进程，与 Router 走**直连链路** stdio/TCP + 长度前缀 V1 帧，**非 MQTT**）与
> `crate/sekai-embedded`（嵌入式 Worker，ESP32 固件，MQTT 主题链路，`esp32` feature 门控）；
> 宿主二进制 `sekai-host` 更名 `sekai-worker`。文中涉及“宿主经 MQTT”的示例为重构前流程，
> MQTT 主题链路仅嵌入式 Worker 使用。

---

## 0. 术语

| 术语 | 含义 |
| --- | --- |
| GPIO 工具 | 本 crate 注册的四个用户工具 `gpio.info/config/read/write`，名称以 `gpio.` 为前缀。 |
| 引脚表 | 每个芯片家族的静态 `PinSpec` 列表，描述引脚是否可用、能力位与特殊标志。 |
| 引脚槽位（slot） | 后端状态数组下标。ESP32 槽位 = IO 编号（0..=39）；STM32 槽位 = 端口序号 × 16 + 引脚号。 |
| 能力（caps） | 输入、输出、内部上拉、内部下拉、开漏五项能力位。 |
| 标志（flags） | 保留、仅输入、strapping、UART0、PSRAM、SWD、JTAG、晶振、BOOT 九类特殊提醒。 |
| 保留引脚 | `flags.reserved` 引脚的统称：configure/read/write 一律拒绝，只允许查看详情。 |
| 虚拟后端 | 宿主上的 `VirtualGpio`，只模拟状态、不触碰硬件，用于无硬件时的端到端验证。 |
| 后端（backend） | 实现 `GpioPort` trait 的具体 GPIO 驱动：ESP32、STM32 或虚拟后端。 |
| 部分更新 | `gpio.config` 只修改显式给出的字段，其余字段沿用当前配置。 |

---

## 1. 目标、范围与非目标

### 1.1 目标

1. 提供一套家族无关、`no_std` 的 GPIO 抽象，覆盖：
   - 引脚命名与解析：ESP32 的 `IO4`/`GPIO4`、STM32 的 `PA5`，大小写不敏感、规范化输出；
   - 方向（输入/输出）、电平、内部上下拉、开漏输出；
   - 能力/保留引脚校验；
   - 统一错误映射到协议 `CommandError`/`ErrorCode`。
2. 在抽象之上提供 AI 可直接调用的四个工具（`gpio.info/config/read/write`），
   其中 `gpio.info help` 是固定英文自描述文本，让 AI 无需外部文档即可学会语法。
3. 提供 ESP32 classic 真实 GPIO 后端（`esp-hal` 1.2.2 的 `Flex`）。
4. 提供 STM32F1/F4 真实 GPIO 后端（直接使用 `stm32f1`/`stm32f4` PAC 寄存器）。
5. 提供宿主虚拟后端，支持 ESP32/F1/F4 三种引脚表，AI 在没有硬件时也能完整走通 MQTT ToolCall 流程。
6. 不修改协议层：GPIO 只是用户工具，通过 Hello 正常通告；`sekai` 运行时保持硬件无关。

### 1.2 本轮交付范围

| 项 | 家族/形态 | 交付 |
| --- | --- | --- |
| ESP32 classic | 真实硬件 | `esp-hal` `Flex` 后端；IO0..IO39 引脚表（表内 34 个可用引脚） |
| STM32F103C8（Blue Pill） | 真实硬件 | `stm32f1` PAC 寄存器后端；PA0-PA15、PB0-PB15、PC13-PC15 |
| STM32F407VG（LQFP100） | 真实硬件 | `stm32f4` PAC 寄存器后端；PA0-PE15 共 80 个引脚 |
| 宿主虚拟后端 | `VirtualGpio` | 三种引脚表；`backend=virtual` |
| 工具层 | 四个用户工具 | 文本参数、文本结果、固定错误码映射、`gpio.info help` |
| 运行时集成 | `sekai-worker`/`sekai-embedded`（x86/esp32） | 注册工具并通过 Hello 通告；host 默认 11 个工具 |

### 1.3 非目标

以下能力**不在**本轮实现，也不在工具语法中预留：

- GPIO 中断、边沿检测、脉冲计数、PWM、ADC、复用功能（AF）；
- 多款 ESP32 变体（S3/C3/C6）与更多 STM32 子家族；只保留扩展点；
- STM32 的网络链路（F1/F4 无内置 WiFi，需要外部 PHY/模块另行设计）；
- ESP32 的 TLS、DNS、静态 IP、NVS/WiFi 配网、AP 模式、BLE、企业 WiFi；
- 二进制参数格式（本轮固定 UTF-8 文本）；
- GPIO 工具对协议帧格式/错误码表的任何改动。

---

## 2. 架构与依赖方向

### 2.1 crate 依赖

```text
sekai（协议 / MQTT / Runtime / Task/Tool 注册表，no_std，硬件无关）
  ↑ 依赖
sekai-gpio（引脚表 + 配置状态机 + GpioPort + 虚拟后端 + gpio.* 工具 + 可选 ESP32/STM32 后端）
  ↑ 依赖                                  ↑ 依赖
sekai-worker / sekai-embedded（x86 / esp32 驱动）  未来 STM32 板级工程（stm32f1/stm32f4 PAC）
```

- `sekai` **不依赖** `sekai-gpio`：协议层保持硬件无关，GPIO 与内置工具无关。
- `sekai-gpio` 依赖 `sekai` 的 `ToolHandler`/`ToolRegistry`/`CommandError` 等公共类型。
- `sekai-worker`/`sekai-embedded` 只负责平台初始化与工具注册：宿主（`sekai-worker`，PC Worker）注册虚拟 GPIO，
  `esp32`（`sekai-embedded`，嵌入式 Worker）注册真实 GPIO。
- STM32 板级工程可以直接依赖 `sekai-gpio` + `stm32f1`/`stm32f4` PAC，
  由用户工程提供链接脚本、启动文件与外设所有权。

### 2.2 feature 矩阵

| crate | feature | 作用 |
| --- | --- | --- |
| `sekai` | `x86` | 现有宿主构建，无变化。 |
| `sekai` | `send` | 注册表强制处理器 `Send`，使 `Runtime` 可放进 Embassy 异步任务。 |
| `sekai` | `esp32` | 等价于 `send`，供 ESP32 固件使用。 |
| `sekai-gpio` | 默认（无 feature） | 核心 + 三家族引脚表 + 虚拟后端 + 工具层，**不依赖任何 HAL**。 |
| `sekai-gpio` | `send` | 共享句柄改为 `Arc<critical_section::Mutex<RefCell<P>>>`，并启用 `sekai/send`。 |
| `sekai-gpio` | `esp32` | `esp-hal` 1.2.2 `Flex` 后端（隐含 `send`，供 Embassy 运行时使用）。 |
| `sekai-gpio` | `stm32f1` | `stm32f1` PAC + F103C8 引脚表。 |
| `sekai-gpio` | `stm32f4` | `stm32f4` PAC + F407VG 引脚表。 |
| `sekai-worker` | 默认 | PC Worker（宿主）：直连链路（stdio/TCP + V1 帧，非 MQTT）+ 控制平面运行时 + 虚拟 GPIO 工具。 |
| `sekai-embedded` | `esp32` | 嵌入式 Worker 固件：真实 GPIO + WiFi/TCP/MQTT + 异步运行时。 |
| `sekai-build` | `esp32` | ESP32 固件构建/烧录入口（见 `docs/sekai-esp32.md`）。 |

默认构建（不加任何 feature）不启用 `send`/HAL，宿主测试与现有行为不变。

### 2.3 关键不变量

实现与集成时不得偏离以下不变量：

1. **协议层不变**：GPIO 是用户工具；不新增帧/错误码/`content_type`；参数文本对协议透明。
2. **默认 feature 不破坏宿主**：`sekai`/`sekai-gpio` 默认构建不启用 `send`/HAL，现有 `cargo test --workspace` 保持通过。
3. **所有后端共享同一套校验/状态机语义**：ESP32、STM32、虚拟后端都通过 `resolve_config`/`validate_config`
   和能力/保留校验，保证 AI 在宿主虚拟后端上学到的行为与真实硬件一致。
4. **保留引脚先拒绝再碰硬件**：任何 configure/read/write 都不得在保留/不存在/能力不足时访问寄存器或 steal 引脚。
5. **ESP32 `AnyPin::steal` 安全不变量**：驱动必须在 `esp_hal::init` 之后创建；应用把 GPIO token 交给后端后
   不再直接使用；每个引脚只 steal 一次（见 §9.2）。
6. **STM32 只接管 GPIO 端口与 GPIO 时钟位**：保留其他外设所有权；F1/F4 的 RCC 置位是幂等的。
7. **ESP32 网络循环不阻塞 Embassy 执行器**：TCP 连接/读写全部带超时，运行时 poll 后立即让出。
8. **断线可恢复**：WiFi/TCP/MQTT 任一失败都进入退避重连；`Runtime` 的
   `link_lost`/`link_failed`/`link_up` 与 x86 语义一致。
9. **无 unsafe 绕过 Send**：`send` 通过类型系统实现；不得 `unsafe impl Send`。
10. **文档与测试同步**：`gpio.info help`、Stdin 帧十六进制示例、工具结果格式必须有测试固定，防止文档漂移
    （见 §7.4 与 §12.3）。

### 2.4 文件清单

| 文件 | 职责 |
| --- | --- |
| `src/lib.rs` | crate 文档、模块导出、`no_std` 与 `deny(unsafe_code)`（硬件后端模块单独放宽）。 |
| `src/pin.rs` | `Port`/`PinId`/`Label`/`PinParseError`、引脚名解析、范围压缩格式化。 |
| `src/family.rs` | `ChipFamily`/`PinCaps`/`PinFlags`/`PinSpec`/`FamilySpec`、三家族引脚表与摘要。 |
| `src/config.rs` | `Level`/`Direction`/`Pull`/`PinConfig`/`ConfigRequest`、`resolve_config`/`validate_config` 合并与校验。 |
| `src/port.rs` | `GpioPort` trait、`PinDescription`/`BackendKind`/`GpioError`、模式辅助函数。 |
| `src/text.rs` | 参数文本词法/取值解析、256/1024 字节上限、结果长度检查。 |
| `src/virtual_gpio.rs` | 宿主虚拟后端 `VirtualGpio`。 |
| `src/tools.rs` | 四个 ToolHandler、`gpio.info help` 文本、`register_tools`、共享端口句柄。 |
| `src/esp32.rs` | `esp-hal` `Flex` 真实后端 `Esp32Gpio`（仅 `esp32` feature）。 |
| `src/stm32/mod.rs` | `Stm32Family` trait、`Stm32Gpio<F>` 通用状态机（仅 STM32 feature）。 |
| `src/stm32/f1.rs` | F1 PAC 寄存器实现与 `F1Ports`/`F1`（CRL/CRH/RCC）。 |
| `src/stm32/f4.rs` | F4 PAC 寄存器实现与 `F4Ports`/`F4`（MODER/PUPDR/OTYPER/OSPEEDR/RCC）。 |
| `tests/docs_examples.rs` | 文档字节级示例的机器校验（Stdin 帧 47 字节、Completed 载荷 15 字节）。 |
| `tests/runtime_tools.rs` | 运行时集成测试：合成 MQTT 报文，验证 Hello 与四个工具的成功/失败路径。 |

---


## 3. 引脚命名与家族表

### 3.1 引脚名语法

工具、`gpio.info` 与宿主环境变量里的引脚名统一按以下规则解析（大小写不敏感）：

| 家族 | 接受的写法 | 规范化输出 | 编号范围 |
| --- | --- | --- | --- |
| ESP32 | `IO4`、`GPIO4`、`4`（以及小写形式） | `IO4` | 0..=39 |
| STM32 | `P<端口><编号>`，例如 `PA5`、`pc13` | `PA5` | 端口 A..K，编号 0..=15 |

约束：

- 引脚名最长 8 字节；空字符串非法。
- 编号**不允许前导零**：`IO04`、`04`、`PA05` 均为非法；`IO0`、`PA0` 合法。
- 解析分两层：`PinId::parse` 只做**语法**校验；某引脚是否属于当前芯片家族，由
  `FamilySpec::resolve` 结合引脚表判断。因此 `PJ0` 语法合法，但在 F1/F4 表外，报“引脚 PJ0 不存在”。

常见错误的归类：

| 输入 | 结果 | 说明 |
| --- | --- | --- |
| `io4`、`GPIO4`、`4` | `IO4` | 合法，输出规范化大写。 |
| `pa5` | `PA5` | 合法，输出规范化大写。 |
| `IO04` | `InvalidArgument`：`引脚名 IO04 非法：引脚编号不能有前导零` | 前导零。 |
| `IO40` | `InvalidArgument`：`引脚 IO40 不存在` | 编号越界按“不存在”处理。 |
| `PA16` | `InvalidArgument`：`引脚 PA16 不存在` | STM32 编号越界。 |
| `A5`、`P5`、`IO` | `InvalidArgument`：引脚名语法非法 | 无法识别的写法。 |
| `IO20`、`IO24`、`IO28`（ESP32） | `InvalidArgument`：`引脚 IO20 不存在` | 芯片未引出。 |
| `PC0`（F1）、`PJ0`（F1/F4） | `InvalidArgument`：引脚不存在 | 不在对应家族表中。 |
| `PE0`（F4） | 合法 | F4 表包含 PE0。 |

### 3.2 规范化与范围压缩

引脚名在结果中一律输出规范化形式（`IO4`/`PA5`，大写）。家族摘要中的
`labels`/`reserved`/`input_only` 使用**范围压缩**：

- 同一端口/同一编号序列中**连续 3 个及以上**的引脚压缩为 `起点-终点`；
- 只有 2 个连续引脚时**分开写**（例如 `PB3,PB4` 而不是 `PB3-PB4`）；
- 不连续的引脚用英文逗号分隔。

示例：

| 引脚集合 | 压缩结果 |
| --- | --- |
| IO0..IO19、IO21..IO23、IO25..IO27、IO32..IO39 | `IO0-IO19,IO21-IO23,IO25-IO27,IO32-IO39` |
| PA0..PA15、PB0..PB15、PC13..PC15 | `PA0-PA15,PB0-PB15,PC13-PC15` |
| PA13、PA14、PA15、PB3、PB4（保留） | `PA13-PA15,PB3,PB4` |

### 3.3 家族总览

| 家族 | 芯片 | 表内引脚 | slots | ports | reserved | input_only |
| --- | --- | --- | --- | --- | --- | --- |
| `FamilyKind::Esp32` | `esp32` | 34 | 40 | 无 | `IO6-IO11` | `IO34-IO39` |
| `FamilyKind::Stm32` / `ChipFamily::Stm32F1` | `stm32f1` | 35 | 48 | A、B、C | `PA13-PA15,PB3,PB4` | 无 |
| `FamilyKind::Stm32` / `ChipFamily::Stm32F4` | `stm32f4` | 80 | 80 | A、B、C、D、E | `PA13-PA15,PB3,PB4` | 无 |

- `family=` 取值为 `esp32` 或 `stm32`；`chip=` 取具体芯片（`esp32`/`stm32f1`/`stm32f4`）。
- `ports` 只对 STM32 有意义；ESP32 的 `FamilySpec::ports` 为空，`gpio.info` 摘要不输出 `ports` 行。
- `slots` 是后端状态数组长度，不是引脚数量；ESP32 的 slots（40）大于表内引脚数（34），因为芯片未引出 IO20/24/28..31。

### 3.4 ESP32 classic 引脚表

ESP32 classic（Xtensa，非 S3/C3）表内共有 **34 个可用引脚**：
IO0-IO19、IO21-IO23、IO25-IO27、IO32-IO39。

> **关于“35 个引脚”的说明**：`docs/sekai-gpio-plan.md` §4.5 的标题写作“35 个可用引脚”，
> 那是把 IO20 误算进了 “IO18..IO23” 的连续范围。硬件事实与逐脚清单是：IO20、IO24、IO28..IO31
> **不存在**，因此表内是 34 个。代码（`family.rs`）与测试（`pin_counts_and_slots`）都断言 34，
> 本文以 34 为准；计划文档的“35”是笔误。

| 引脚 | caps | flags | note |
| --- | --- | --- | --- |
| IO0 | `ALL` | `strap` | — |
| IO1 | `ALL` | `uart0` | — |
| IO2 | `ALL` | `strap` | — |
| IO3 | `ALL` | `uart0` | — |
| IO4 | `ALL` | — | — |
| IO5 | `ALL` | `strap` | — |
| IO6..IO11 | `NONE` | `reserved` | `SPI flash` |
| IO12 | `ALL` | `strap` | — |
| IO13..IO14 | `ALL` | — | — |
| IO15 | `ALL` | `strap` | — |
| IO16..IO17 | `ALL` | `psram` | `WROVER PSRAM` |
| IO18..IO19 | `ALL` | — | — |
| IO21..IO23 | `ALL` | — | — |
| IO25..IO27 | `ALL` | — | — |
| IO32..IO33 | `ALL` | — | — |
| IO34..IO39 | `INPUT_ONLY` | `input_only` | — |

补充说明：

- **不存在**：IO20、IO24、IO28、IO29、IO30、IO31。`gpio.info IO20`、`gpio.config IO24 out` 等
  一律返回“引脚不存在”（`InvalidArgument`），在触碰硬件之前拒绝。
- **SPI flash 保留**：IO6..IO11 是 flash 总线，永不可作为普通 GPIO；保留引脚只允许 `gpio.info PIN` 查看详情。
- **strapping 引脚**：IO0、IO2、IO5、IO12、IO15 在复位时决定启动模式（例如 IO0 低电平进入下载模式）。
  上电瞬间被外部电路拉错电平可能导致启动失败；输出配置后不要在这些引脚上产生复位期毛刺。
- **UART0**：IO1（TX）、IO3（RX）不保留（可以当普通 GPIO），但被 `esp-println`/串口日志/烧录占用时
  不能同时用作普通 IO。参考实现的固件默认使用 UART0 打印日志（见 `docs/sekai-esp32.md`），
  AI 不应把 IO1/IO3 配置成输出。
- **PSRAM**：IO16、IO17 在 WROVER 模块上可能被 PSRAM 占用；在 WROOM/普通模块上可用，但表内仍打 `psram` 标志提醒。
- **仅输入**：IO34..IO39 没有输出驱动与内部上下拉，`caps=INPUT_ONLY`：配置输出会返回“仅支持输入”，
  配置上/下拉会返回“不支持上拉/下拉”。

### 3.5 STM32F103C8（F1）引脚表

F103C8（Blue Pill，LQFP48）表内共 **35 个引脚**：PA0-PA15、PB0-PB15、PC13-PC15。
全部 `caps=ALL`（F1 GPIO 都支持输入/输出/内部上下拉/开漏）。

| 引脚 | flags | note | 说明 |
| --- | --- | --- | --- |
| PA0..PA12 | — | — | 普通 GPIO。 |
| PA13 | `reserved,swd` | `SWD` | SWDIO，调试口。 |
| PA14 | `reserved,swd` | `SWD` | SWCLK，调试口。 |
| PA15 | `reserved,jtag` | `JTAG` | JTDI，JTAG 默认占用。 |
| PB0..PB1 | — | — | 普通 GPIO。 |
| PB2 | `boot` | `BOOT1` | 启动配置引脚。 |
| PB3 | `reserved,jtag` | `JTAG` | JTDO。 |
| PB4 | `reserved,jtag` | `JTAG` | NJTRST。 |
| PB5..PB15 | — | — | 普通 GPIO。 |
| PC13 | — | `RTC` | RTC 相关用途（note 提醒，非 flag，不阻止操作）。 |
| PC14 | `osc` | `LSE` | LSE 晶振输入。 |
| PC15 | `osc` | `LSE` | LSE 晶振输出。 |

- `ports=A,B,C`；槽位 A=0..15、B=16..31、C=32..47，共 `slots=48`。
- **表外**：PC0..PC12、PD/PE 等不存在（F103C8 封装没有这些引脚），`gpio.config PC0 out` 返回“引脚 PC0 不存在”。
- 保留引脚：PA13、PA14（SWD）、PA15、PB3、PB4（JTAG）。保留是安全默认值：AI 不应在不知道板级连接的情况下
  把调试口改成普通输出。
- PB2 带 `boot` 标志（BOOT1）；不要在运行时随意驱动，除非确认板级启动配置。
- PC13 的 note 是 `RTC`：该引脚与 RTC/备份域相关，但**不是保留引脚**，工具不会拒绝它；
  如果板级把 PC13 用作 LED（Blue Pill 常见），可以正常配置输出。

### 3.6 STM32F407VG（F4）引脚表

F407VG（LQFP100）表内共 **80 个引脚**：PA0-PA15、PB0-PB15、PC0-PC15、PD0-PD15、PE0-PE15。
全部 `caps=ALL`；`ports=A,B,C,D,E`；槽位 = 端口序号 × 16 + 引脚号，共 `slots=80`。

| 引脚 | flags | note | 说明 |
| --- | --- | --- | --- |
| PA13 | `reserved,swd` | `SWD` | SWDIO。 |
| PA14 | `reserved,swd` | `SWD` | SWCLK。 |
| PA15 | `reserved,jtag` | `JTAG` | JTDI。 |
| PB3 | `reserved,jtag` | `JTAG` | JTDO。 |
| PB4 | `reserved,jtag` | `JTAG` | NJTRST。 |
| PC14 | `osc` | `LSE` | LSE 晶振输入。 |
| PC15 | `osc` | `LSE` | LSE 晶振输出。 |
| 其余 | — | — | 普通 GPIO。 |

- 保留引脚与 F1 相同：PA13、PA14、PA15、PB3、PB4。
- 与 F1 的差异：F4 表没有 `boot` 标志；PC13 没有 RTC note；PC0..PC12 存在。
- 参考板为 LQFP100；其他 F407 封装（如 LQFP64）引脚数更少，需要用户自行裁剪 `PinSpec` 表。

### 3.7 能力（caps）与标志（flags）

`caps` 的取值与输出顺序固定为：`in`、`out`、`pu`（内部上拉）、`pd`（内部下拉）、`od`（开漏）。
没有能力时输出 `none`。

`flags` 的取值与输出顺序固定为下列 9 个，逗号分隔；无标志输出 `none`：

| 顺序 | flag | 含义 | 影响的引脚示例 |
| --- | --- | --- | --- |
| 1 | `reserved` | 保留，configure/read/write 一律拒绝。 | ESP32 IO6-IO11；F1/F4 PA13-PA15、PB3、PB4 |
| 2 | `input_only` | 仅输入，无输出/上下拉能力。 | ESP32 IO34-IO39 |
| 3 | `strap` | 复位时决定启动模式。 | ESP32 IO0/2/5/12/15 |
| 4 | `uart0` | UART0（TX/RX）占用，日志/烧录冲突。 | ESP32 IO1/IO3 |
| 5 | `psram` | WROVER 模块 PSRAM 可能占用。 | ESP32 IO16/IO17 |
| 6 | `swd` | SWD 调试口。 | F1/F4 PA13/PA14 |
| 7 | `jtag` | JTAG 调试口。 | F1/F4 PA15、PB3、PB4 |
| 8 | `osc` | LSE 晶振占用。 | F1/F4 PC14/PC15 |
| 9 | `boot` | 启动配置引脚（BOOT1）。 | F1 PB2 |

`note` 是自由文本备注（例如 `SPI flash`、`SWD`、`LSE`），只在非空时由 `gpio.info` 输出。
注意：PC13 的 `RTC` 这类信息放在 note 里，**不是** flag；flag 集合只有上表 9 个。

### 3.8 槽位（slot）

槽位是后端数组下标，不对外暴露在工具语法里：

- ESP32：`slot = IO 编号`，范围 0..=39（`slots=40`），即使 IO20/24/28..31 不存在也保留空槽位。
- STM32：`slot = 端口序号 × 16 + 引脚号`（A=0、B=1、……）。F1 的 C 端口只用 13..15，但槽位仍按 32..47 计算；
  F4 的槽位是 0..79。

### 3.9 保留策略与解除

保留策略是**安全默认值**，目的是让 AI 在不知道板级连接时也不会把调试口、SPI flash 改成普通 IO：

- 保留引脚：`gpio.config`/`gpio.read`/`gpio.write` 在任何操作前返回 `InvalidArgument`，
  不触碰任何寄存器/引脚；`gpio.info PIN` 仍可查看原因（`flags=reserved` 与 `note`）。
- 输入专用引脚：配置输出返回“仅支持输入”，配置上下拉返回“不支持上拉/下拉”。
- 解除保留需要**修改引脚表**（复制 `crate/sekai-gpio/src/family.rs` 中的对应表项，去掉 `reserved` 标志，
  或在你自己的家族表里定义新表），并由你负责板级风险；工具层不提供运行时解除开关。
- 不建议解除 SWD/JTAG 保留（会失去调试能力）；ESP32 的 SPI flash 引脚（IO6..IO11）在任何情况下都不要解除。

---


## 4. 配置状态机

所有后端（ESP32、STM32、虚拟）共享同一套配置语义：`resolve_config` 负责合并部分更新，
`validate_config` 负责能力校验，后端只负责把**已经校验过**的配置写进硬件。

### 4.1 类型

| 类型 | 取值 | 说明 |
| --- | --- | --- |
| `Level` | `Low`/`High` | 文本输出为 `0`/`1`。 |
| `Direction` | `Input`/`Output` | 文本输出为 `input`/`output`。 |
| `Pull` | `None`/`Up`/`Down` | 文本输出为 `none`/`up`/`down`。 |
| `PinConfig` | `direction`/`level`/`pull`/`open_drain` | 一个引脚的完整配置。 |
| `ConfigRequest` | 四个字段均为 `Option` | `gpio.config` 的部分更新请求；`None` 表示沿用当前值。 |

`PinConfig::default()` 是**安全的输入默认值**：`Input`/`Low`/`None`/`false`。
未配置过的引脚在 `gpio.config` 未给方向时按这个默认值处理（不会意外变成输出）。

### 4.2 合并规则（九条）

`gpio.config PIN ...` 的执行顺序与语义：

1. **起始配置** = 当前配置；未配置过则用 `PinConfig::default()`（输入/低/无上下拉/推挽）。
2. **显式切到输入**（`in`/`input`）：强制 `level=0`、`open_drain=false`；
   如果请求给了 `pull` 则应用，否则保留起始配置的上下拉。
3. **显式切到输出**（`out`/`output`）：强制 `pull=none`；
   如果请求给了 `level`/`open_drain` 则应用，否则保留起始配置的电平/开漏。
4. **没有给方向**：沿用当前方向（未配置默认输入）；显式给出的 `level`/`pull`/`open_drain`
   覆盖对应字段。
5. **`level` 只允许在最终方向为输出时出现**：对输入引脚（或未配置且未给方向的引脚）显式给 `level`
   返回 `InvalidArgument`，消息提示先 `gpio.config PIN out`。
6. **`pull` 只允许在最终方向为输入时出现**：对输出引脚显式给 `pull` 返回 `InvalidArgument`，
   消息提示先 `gpio.config PIN in`。
7. **`open_drain=true` 只允许在最终方向为输出时出现**：对输入引脚返回 `InvalidArgument`。
8. **能力校验**：最终配置必须满足引脚能力（见 §4.3）；不满足返回“不支持/仅支持输入”类错误。
9. **保留/不存在优先**：保留引脚在合并开始前就返回 `InvalidArgument`（不触碰硬件）；
   不存在或不属于当前家族的引脚在解析阶段返回“引脚不存在”。

补充：

- 合并规则只检查**请求里显式给出的字段**：例如输出配置里残留的 `pull` 不会误报，
  因为它已经被规则 3 强制为 `none`。
- 同一个键可以重复出现在不同方向切换之后；工具层会拒绝**重复键/重复方向**（见 §5.3）。
- 输入/输出可以反复切换；后端不保留 HAL 类型状态，只保存 `PinConfig`。

### 4.3 能力校验

| 最终配置 | 需要的 caps | 典型失败消息 |
| --- | --- | --- |
| `direction=input` | `caps.input` | `引脚 X 不支持输入` |
| `direction=output` | `caps.output` | 输入专用引脚：`引脚 X 仅支持输入`；否则 `引脚 X 不支持输出` |
| `pull=up` | `caps.pull_up` | `引脚 X 不支持上拉` |
| `pull=down` | `caps.pull_down` | `引脚 X 不支持下拉` |
| `open_drain=true` | `caps.open_drain` | `引脚 X 不支持开漏输出` |

能力校验发生在合并之后、后端操作寄存器之前；失败时硬件状态不变。

### 4.4 错误语义与错误码

`GpioError` 的所有变体都携带已经格式化好的中文消息，映射到协议错误码如下：

| 变体 | 触发场景 | 协议错误码 | 消息示例 |
| --- | --- | --- | --- |
| `UnknownPin` | 引脚不在当前家族表中 | `InvalidArgument` | `引脚 IO40 不存在` |
| `ReservedPin` | configure/read/write 保留引脚 | `InvalidArgument` | `引脚 IO6 已保留（SPI flash）` |
| `NotConfigured` | 未配置就 read/write | `InvalidArgument` | `引脚 IO4 未配置；请先 gpio.config IO4 in 或 gpio.config IO4 out` |
| `WrongMode` | 对输出 read / 对输入 write | `InvalidArgument` | `引脚 IO4 处于输出模式，无法读取；请先 gpio.config IO4 in` |
| `Unsupported` | 能力不足（仅输入/无上下拉等） | `InvalidArgument` | `引脚 IO34 仅支持输入` |
| `InvalidArgument` | 参数/合并规则非法 | `InvalidArgument` | `参数 foo 非法` |
| `Backend` | 后端内部异常 | `Internal` | 由后端决定（例如 `引脚 IO4 驱动不可用`） |

工具层额外错误：

| 场景 | 协议错误码 | 消息示例 |
| --- | --- | --- |
| 参数不是合法 UTF-8 | `InvalidArgument` | `参数不是合法 UTF-8` |
| 参数超过 256 字节 | `InvalidArgument` | `参数超过 256 字节（实际 N 字节）` |
| 结果超过 1024 字节 | `ResultTooLarge` | `GPIO 结果过大` |
| 共享端口句柄借用失败 | `Internal` | `GPIO 被占用` |

> 协议还有一个保留的 `Unsupported` 错误码（0x000A），GPIO 工具**不会**产生它：
> 参数类错误统一 `InvalidArgument`，后端异常统一 `Internal`，结果过大统一 `ResultTooLarge`。

### 4.5 配置示例

| 当前状态 | 命令 | 结果 |
| --- | --- | --- |
| 未配置 | `gpio.config IO4` | 成功：`pin=IO4 mode=input level=0 pull=none open_drain=0` |
| 未配置 | `gpio.config IO4 out level=1` | 成功：`pin=IO4 mode=output level=1 pull=none open_drain=0` |
| 输出高 | `gpio.config IO4 in pull=up` | 成功：`pin=IO4 mode=input level=0 pull=up open_drain=0` |
| 输入 | `gpio.config IO4 level=1` | 失败 `InvalidArgument`：`引脚 IO4 处于输入模式，不能设置 level；请先 gpio.config IO4 out` |
| 输出 | `gpio.config IO4 pull=up` | 失败 `InvalidArgument`：`引脚 IO4 处于输出模式，不能设置 pull；请先 gpio.config IO4 in` |
| 输入 | `gpio.config IO4 open_drain=1` | 失败 `InvalidArgument`：`引脚 IO4 处于输入模式，不能启用 open_drain` |
| 未配置 | `gpio.config IO34 out` | 失败 `InvalidArgument`：`引脚 IO34 仅支持输入` |
| 未配置 | `gpio.config IO6 out` | 失败 `InvalidArgument`：`引脚 IO6 已保留（SPI flash）` |
| 未配置 | `gpio.config IO20 out` | 失败 `InvalidArgument`：`引脚 IO20 不存在` |

---

## 5. gpio.* 工具契约

四个工具的名称、顺序与注册函数是稳定的：

```text
gpio.info
gpio.config
gpio.read
gpio.write
```

`GPIO_TOOL_NAMES` 的声明顺序为 `["gpio.config", "gpio.info", "gpio.read", "gpio.write"]`
（Hello 中按注册表排序，工具名集合相等即可）。

### 5.1 通用约定

- **参数**是 UTF-8 文本，最大 **256 字节**；超过返回 `InvalidArgument`。
- **结果**也是 UTF-8 文本，最大 **1024 字节**；超过返回 `ResultTooLarge`。
- 分词：ASCII 空白（空格、Tab、换行）分隔；**不支持引号/转义**；空 token 被忽略。
- 键与值大小写不敏感；引脚名规范化输出大写（`IO4`/`PA5`）。
- 结果格式：
  - 多行用换行符分隔，**末尾不带换行**；
  - `gpio.info` 的摘要/详情使用逐行 `key=value`；
  - `gpio.config`/`gpio.read`/`gpio.write` 使用**单行空格分隔**的 `key=value`；
  - `gpio.info help` 是唯一非 `key=value` 的结果，固定英文自描述文本（见 §5.2.4）。
- 值为空时是否输出由字段决定：`note` 为空时整行省略；`gpio.info` 摘要里的 `input_only` 无条件输出（STM32 为空值）。

### 5.2 gpio.info

#### 5.2.1 语法

```text
gpio.info
gpio.info help
gpio.info PIN
gpio.info port=A          # 仅 STM32
```

- 无参数：家族摘要。
- `help`：固定帮助文本。
- `PIN`：单引脚详情；保留引脚也允许查看。
- `port=A`：列出该端口的全部表内引脚，仅 STM32 支持；ESP32 返回 `InvalidArgument`。
- 未知的额外参数 / `port=Z` / 缺值的 `port=`：`InvalidArgument`。

#### 5.2.2 无参数摘要

ESP32（宿主虚拟后端）：

```text
gpio=1
family=esp32
chip=esp32
backend=virtual
pins=34
labels=IO0-IO19,IO21-IO23,IO25-IO27,IO32-IO39
reserved=IO6-IO11
input_only=IO34-IO39
configured=0
```

STM32F1（宿主虚拟后端）：

```text
gpio=1
family=stm32
chip=stm32f1
backend=virtual
ports=A,B,C
pins=35
labels=PA0-PA15,PB0-PB15,PC13-PC15
reserved=PA13-PA15,PB3,PB4
input_only=
configured=0
```

- 字段顺序固定：`gpio`、`family`、`chip`、`backend`、`ports`（仅 STM32）、`pins`、
  `labels`、`reserved`、`input_only`、`configured`。
- `backend` 取 `virtual`（宿主虚拟后端）或 `hardware`（ESP32/STM32 真实后端）。
- `labels`/`reserved`/`input_only` 使用 §3.2 的范围压缩。
- `configured` 是当前已配置引脚数（0..=表内引脚数）。
- `ports` 行只在 STM32 后端输出；`input_only` 行**无条件输出**（STM32 没有仅输入引脚时为空值 `input_only=`）。

#### 5.2.3 单引脚详情

未配置的普通引脚：

```text
pin=IO4
caps=in,out,pu,pd,od
flags=none
mode=unconfigured
configured=0
```

已配置为输出：

```text
pin=PA5
caps=in,out,pu,pd,od
flags=none
mode=output
level=1
pull=none
open_drain=0
configured=1
```

已配置为输入：

```text
pin=IO4
caps=in,out,pu,pd,od
flags=none
mode=input
level=0
pull=up
open_drain=0
configured=1
```

保留引脚（仍然可以查看）：

```text
pin=IO6
caps=none
flags=reserved
note=SPI flash
mode=unconfigured
configured=0
```

- 字段顺序固定：`pin`、`caps`、`flags`、`note`（非空时）、`mode`、
  `level`/`pull`/`open_drain`（已配置时）、`configured`。
- `mode` 取 `unconfigured`/`input`/`output`；未配置时不输出 level/pull/open_drain。
- 输入电平读 IDR；输出电平读锁存/ODR；未配置为无电平。
- 保留/仅输入/PSRAM/strap 等信息通过 `caps`/`flags`/`note` 暴露，AI 应据此避开危险引脚。

#### 5.2.4 `gpio.info help`（逐字）

```text
gpio tools v1
info [help|PIN|port=A]
config PIN [in|out] [level=0|1] [pull=none|up|down] [open_drain=0|1]
read PIN
write PIN 0|1
pins: IO4 / PA5
read requires input mode; write requires output mode
```

这段文本是固定英文，必须逐字实现并由测试锁定；它是 AI 在没有外部文档时学习工具语法的唯一入口。

#### 5.2.5 STM32 `port=A`

```text
port=A
PA0 caps=in,out,pu,pd,od flags=none
PA1 caps=in,out,pu,pd,od flags=none
...
PA13 caps=in,out,pu,pd,od flags=reserved,swd
...
```

- 首行固定 `port=A`；其后每个表内引脚一行。
- 行内字段顺序：引脚名、`caps=...`、`flags=...`；**没有 note 字段**。
- ESP32 调用 `gpio.info port=A` 返回 `InvalidArgument`，消息为
  `esp32 家族没有端口；请使用 gpio.info PIN 查看单个引脚`；STM32 端口不在家族内
  （如 F1 的 `port=D`）返回 `InvalidArgument`：`stm32f1 家族没有端口 D`。
- `port` 值大小写不敏感（`a` 等价于 `A`），只接受单字母 A..K。

### 5.3 gpio.config

#### 5.3.1 语法

```text
gpio.config PIN [in|out] [DIR=...] [LEVEL=...] [PULL=...] [OPEN_DRAIN=...]
```

- 裸方向：`in`/`input`/`out`/`output`，最多一个。
- 键（大小写不敏感）：
  - `dir` 或 `direction`：`in`/`input`/`out`/`output`；
  - `level` 或 `init`：`0`/`1`/`low`/`high`/`on`/`off`；
  - `pull`：`none`/`up`/`down`；
  - `open_drain` 或 `od`：`0`/`1`/`true`/`false`/`on`/`off`。
- 参数重复（包括裸方向与 `dir=` 同时给出）→ `InvalidArgument`，消息为
  `gpio.config 参数重复：方向`、`gpio.config 参数重复：dir/direction`、
  `gpio.config 参数重复：level/init`、`gpio.config 参数重复：pull`、
  `gpio.config 参数重复：open_drain/od`。
- 未知键 → `gpio.config 未知键 foo；用法见 gpio.info help`；
  未知裸参数 → `gpio.config 未知参数 bogus；用法见 gpio.info help`。
- 缺引脚 → `gpio.config 缺少引脚；用法见 gpio.info help`；缺值（如 `level=`）由值解析报错
  （例如 `电平  非法；应为 0/1/low/high/on/off`）。
- 只给 `PIN` 不给任何字段：按默认输入配置（安全默认）。
- 合并/校验语义见 §4.2/§4.3；工具层负责把文本解析成 `ConfigRequest`，再调用核心合并函数。

#### 5.3.2 结果

成功时单行（`mode` 后跟随完整配置）：

```text
pin=PA5 mode=output level=1 pull=none open_drain=0
```

```text
pin=PA5 mode=input level=0 pull=up open_drain=0
```

- 输出配置的 `pull` 恒为 `none`；输入配置的 `level` 恒为 `0`、`open_drain` 恒为 `0`。
- 结果中的 `pin` 是规范化引脚名。
- 失败返回 `Failed` 事件与 §4.4 的错误码，不修改硬件。

### 5.4 gpio.read

```text
gpio.read PIN
```

- 仅允许已配置为**输入**的引脚；输出模式返回 `InvalidArgument`
  （消息提示 `请先 gpio.config PIN in`）；未配置返回“引脚未配置”。
- 成功结果单行：

```text
pin=IO4 level=1
```

- 虚拟后端返回模拟的外部输入电平；ESP32 读 `flex.level()`；STM32 读 `IDR`。
- 保留/不存在/仅输入（配置层面）引脚按统一错误处理；仅输入引脚本身可以正常 read。

### 5.5 gpio.write

```text
gpio.write PIN 0|1|low|high|on|off
```

- 仅允许已配置为**输出**的引脚；输入/未配置返回 `InvalidArgument`
  （消息提示 `请先 gpio.config PIN out`）。
- 成功结果单行：

```text
pin=IO4 level=1
```

- 虚拟后端更新锁存电平；ESP32 调用 `flex.set_level()`；STM32 写 `BSRR`。
- 保留/不存在/能力不足引脚返回 `InvalidArgument`，不触碰硬件。

### 5.6 错误码汇总

| 场景 | 错误码 | 是否最终事件 |
| --- | --- | --- |
| 参数语法/键值/长度非法（>256、非 UTF-8） | `InvalidArgument` | 是（`Failed`） |
| 引脚不存在 | `InvalidArgument` | 是 |
| 保留引脚 | `InvalidArgument` | 是 |
| 仅输入引脚配置输出/上下拉 | `InvalidArgument` | 是 |
| 未配置或模式不匹配 | `InvalidArgument` | 是 |
| 共享句柄借用失败 | `Internal` | 是 |
| 后端内部错误 | `Internal` | 是 |
| 结果超过 1024 字节 | `ResultTooLarge` | 是 |

所有错误消息都包含引脚名或具体字段，便于 AI 自我纠正；消息长度由协议层限制到 256 字节
（见 `docs/sekai-protocol.md` §5.4）。

#### 5.6.1 错误消息模板（逐字）

以下是工具层实际返回的错误消息模板（引脚名使用规范化写法；语法错误消息会保留用户输入原文）：

- 引脚不存在：`引脚 IO20 不存在`（`IO40`、表外引脚同理）。
- 引脚名语法：`引脚名 IO04 非法：引脚编号不能有前导零`；其他原因包括
  `无法识别的引脚名语法`、`引脚名超过 8 字节`、`STM32 端口必须是 A..K`、
  `ESP32 引脚编号必须在 0..=39`、`STM32 引脚编号必须在 0..=15`。
- 保留：`引脚 IO6 已保留（SPI flash）`；无 note 时形如 `引脚 PA13 已保留`。
- 仅输入：`引脚 IO34 仅支持输入`；其他能力：`引脚 X 不支持上拉/下拉/开漏输出/输入/输出`。
- 未配置 read/write：`引脚 IO4 未配置；请先 gpio.config IO4 in 或 gpio.config IO4 out`。
- 输出模式 read：`引脚 IO4 处于输出模式，无法读取；请先 gpio.config IO4 in`。
- 输入模式 write：`引脚 IO4 处于输入模式，无法写入；请先 gpio.config IO4 out`。
- 方向敏感字段：`引脚 IO4 处于输入模式，不能设置 level；请先 gpio.config IO4 out` /
  `引脚 IO4 处于输出模式，不能设置 pull；请先 gpio.config IO4 in` /
  `引脚 IO4 处于输入模式，不能启用 open_drain`。
- 参数个数/用法：`gpio.config 缺少引脚；用法见 gpio.info help`、
  `gpio.read 需要且仅需要一个引脚；用法见 gpio.info help`、
  `gpio.write 需要引脚和电平；用法见 gpio.info help`、
  `gpio.info 参数非法；用法见 gpio.info help`、
  `gpio.info 未知参数 xxx；用法见 gpio.info help`。
- 文本值解析（来自 `text.rs`）：`方向 inputx 非法；应为 in/input/out/output`、
  `电平 2 非法；应为 0/1/low/high/on/off`、`上下拉 float 非法；应为 none/up/down`、
  `布尔值 yes 非法；应为 0/1/true/false/on/off`、`端口 Z 非法；应为 A..K`。
  （这些消息里的值两侧本来带反引号，这里省略以避免 Markdown 嵌套。）
- 长度/借用：`参数超过 256 字节（实际 N 字节）`、`参数不是合法 UTF-8`、
  `GPIO 结果过大`、`GPIO 被占用`。

### 5.7 注册与 Hello

- 注册函数：

```rust
pub const GPIO_TOOL_NAMES: [&str; 4] = ["gpio.config", "gpio.info", "gpio.read", "gpio.write"];

pub fn register_tools<P>(registry: &mut ToolRegistry, port: P) -> Result<(), RegistryError>
where
    P: GpioPort + 'static; // send 特性下额外要求 Send
```

- 四个工具都是**用户工具**：不占用 `sekai.*` 保留前缀，通过 `ToolRegistry` 注册后出现在 Hello 的
  `tools` 列表中。
- 如果应用已经注册了同名 `gpio.*`，`register_tools` 返回 `RegistryError::Duplicate`；
  `gpio.` 前缀由本 crate 约定占用。
- 工具数上限：`Runtime` 校验“用户注册表工具数 + 6 个内置工具 ≤ 16”（协议 `MAX_TOOLS`）。
  参考实现：ESP32 固件注册 4 个 GPIO 工具（6 + 4 = 10）；宿主 `sekai-worker`（原 `sekai-host`）额外有 `echo` 工具（11）。
- `gpio.info` 的 `backend=virtual/hardware` 直接来自 `GpioPort::backend()`。

### 5.8 与协议层的关系

- 四个工具复用普通 ToolCall：`ToolBody` 只有工具名与 args，args 是 UTF-8 文本；结果作为
  `Completed` 的原始载荷返回（协议不解释内容）。
- 不新增 Stdin/Stdout 帧类型、不新增错误码、不修改 `content_type`；协议层完全不知道“GPIO”概念。
- 工具同步执行，受 Stdin `ddl_ms` 预算约束（`0` 取参考实现默认 500ms）；处理器不得阻塞或忙等。
- 工具结果上限 1024 字节，低于协议 `MAX_PAYLOAD`（4096 字节），因此 `ResultTooLarge` 是工具自身的
  约束而非常规协议载荷上限。
- `gpio.info help` 是 AI 的自描述入口；Router/工具文档应指引 AI 先调用它（见 §6）。

---


## 6. AI 调用流程

### 6.1 标准流程

AI 通过 MQTT ToolCall 操作 GPIO 的推荐顺序：

1. **读 Hello**：从 retained `state`（或 `sekai.info` 结果）读取 `tools`，
   确认包含 `gpio.info`/`gpio.config`/`gpio.read`/`gpio.write`；记录 Worker 是 ESP32 还是 STM32。
2. **读帮助**：调用 `gpio.info help`，得到语法与约束（这段文本是自描述的，不需要外部文档）。
3. **读摘要**：调用 `gpio.info`，得到 `family/chip/backend/pins/labels/reserved/input_only/configured`，
   选出**未保留、非仅输入**的候选引脚；避开 `uart0`（日志/烧录）、`strap`（启动模式）、
   `psram`（WROVER 模块）这些高风险引脚。
4. **确认引脚**：调用 `gpio.info PIN`，查看 `caps`/`flags`/`note` 与当前 `mode`。
5. **配置**：调用 `gpio.config PIN out level=0`（先输出低电平，避免毛刺；具体初值依硬件而定）。
6. **输出**：调用 `gpio.write PIN 1`；需要翻转就交替写 0/1。
7. **读取**：调用 `gpio.config PIN in pull=up`（按键/开关输入通常需要上拉），再 `gpio.read PIN`。

要点：

- 每一步都用上一步的结果确认；不要凭猜测使用表外引脚（例如 ESP32 的 IO20/24/28..31）。
- 配置与读写都有方向约束：`read` 只对输入、`write` 只对输出；违反时消息会提示先执行哪条 `gpio.config`。
- 保留引脚只能通过 `gpio.info` 查看；不要尝试在运行时“解除保留”。
- 工具是同步短调用，受 `ddl_ms` 预算约束（默认 500ms）；不要用大量 ToolCall 做忙等轮询。

### 6.2 失败恢复

| 现象 | 含义 | AI 应采取的动作 |
| --- | --- | --- |
| `InvalidArgument` + “不存在” | 引脚不在当前家族表 | 重新 `gpio.info` 选表内引脚；F1 不要用 PC0，ESP32 不要用 IO20/24/28..31。 |
| `InvalidArgument` + “已保留” | 引脚被保留（调试口/SPI flash 等） | 换引脚；保留原因见错误消息与 `gpio.info` 的 note。 |
| `InvalidArgument` + “仅支持输入” | 对仅输入引脚配置输出/上下拉 | 改用输入方向，或换引脚。 |
| `InvalidArgument` + “未配置”/“模式” | 读写方向不匹配 | 先执行消息里提示的 `gpio.config PIN in/out`。 |
| `InvalidArgument` + 参数语法 | 键/值/重复键非法 | 对照 `gpio.info help` 修正；不要传引号/转义。 |
| `Internal` | 共享句柄被占用或后端异常 | 重试一次；仍失败则报告 Worker GPIO 后端故障。 |
| `ResultTooLarge` | 结果超过 1024 字节 | GPIO 结果很小，正常不会触发；出现即视为实现缺陷并上报。 |
| `Timeout`（协议 0x0008） | 工具调用超过 DDL | 不要重试风暴；先 `sekai.info` 查看 Worker 状态，再降低调用频率。 |

### 6.3 完整对话示例（ESP32 虚拟后端）

下面是一次成功交互的文本视图（`>` 是 ToolCall，`<` 是 Completed 载荷，多行结果按行展开）：

```text
> gpio.info help
< gpio tools v1
  info [help|PIN|port=A]
  config PIN [in|out] [level=0|1] [pull=none|up|down] [open_drain=0|1]
  read PIN
  write PIN 0|1
  pins: IO4 / PA5
  read requires input mode; write requires output mode

> gpio.info
< gpio=1
  family=esp32
  chip=esp32
  backend=virtual
  pins=34
  labels=IO0-IO19,IO21-IO23,IO25-IO27,IO32-IO39
  reserved=IO6-IO11
  input_only=IO34-IO39
  configured=0

> gpio.info IO2
< pin=IO2
  caps=in,out,pu,pd,od
  flags=strap
  mode=unconfigured
  configured=0

> gpio.config IO2 out level=0
< pin=IO2 mode=output level=0 pull=none open_drain=0

> gpio.write IO2 1
< pin=IO2 level=1

> gpio.config IO4 in pull=up
< pin=IO4 mode=input level=0 pull=up open_drain=0

> gpio.read IO4
< pin=IO4 level=0
```

上例中 `gpio.read IO4` 返回 0，因为虚拟后端的外部输入默认低电平；宿主可用
`SEKAI_GPIO_INPUTS=IO4=1` 把它初始化为高电平（见 §8）。

上述流程（读 Hello 能力 → `gpio.info help` → `gpio.info` → `gpio.info PIN` → `gpio.config` →
`gpio.write`/`gpio.read`）可由 Router（`crate/sekai-router`；原 `crate/sekai-ai` 的 `sekai-agent` CLI）自动完成：
它把自然语言目标翻译成 LLM function tools，并在发布 Stdin 前强制执行服务端策略；
设计沿革、CLI 与安全开关见 `docs/sekai-ai.md`（历史文档），现行架构见 `docs/sekai-architecture.md`。

---

## 7. 完整 Stdin 帧示例

本节给出 `gpio.write IO4 1` 的完整 Stdin 帧（发布到 `sekai/v1/dev/demo/in`，QoS1）。
示例由 `crate/sekai-gpio/tests/docs_examples.rs` 机器校验，字节一旦漂移测试就会失败。

### 7.1 公共上下文

| 项 | 值 |
| --- | --- |
| 设备 id | `demo` |
| 主题根 | `sekai/v1` |
| 命令 id | 1 |
| Router 时间戳 | `1700000000000` ms |
| `delay_ms` | 0 |
| `ddl_ms` | 500 |
| 工具 | `gpio.write`（10 字节） |
| 参数 | `IO4 1`（5 字节） |

ToolBody 载荷 = `"gpio.write"` 的 `u16` 长度（2 字节）+ 名称（10 字节）+ args（5 字节）= **17 字节**；
Stdin 帧头固定 30 字节，因此整帧 = **47 字节**（0x2F）。

### 7.2 帧布局

```text
偏移  字节                                             说明
0x00  01 02                                            version=1, kind=2(ToolCall)
0x02  00 00 00 00 00 00 00 01                          id=1
0x0A  00 00 01 8B CF E5 68 00                          timestamp=1700000000000
0x12  00 00 00 00                                      delay_ms=0
0x16  00 00 01 F4                                      ddl_ms=500
0x1A  00 00 00 11                                      payload_len=17
0x1E  00 0A                                            ToolBody name_len=10
0x20  67 70 69 6F 2E 77 72 69 74 65                    "gpio.write"
0x2A  49 4F 34 20 31                                   args "IO4 1"
```

### 7.3 单行十六进制（47 字节）

```text
01 02 00 00 00 00 00 00 00 01 00 00 01 8B CF E5 68 00
00 00 00 00 00 00 01 F4 00 00 00 11
00 0A 67 70 69 6F 2E 77 72 69 74 65 49 4F 34 20 31
```

### 7.4 成功结果与测试锁定

`gpio.write IO4 1` 成功后，Worker 回一帧 `Completed`（phase=4），载荷是 15 字节 UTF-8 文本，
**不带换行**：

```text
pin=IO4 level=1
```

`crate/sekai-gpio/tests/docs_examples.rs` 中两个测试分别锁定：

| 测试 | 断言内容 |
| --- | --- |
| `gpio_write_stdin_frame_matches_documentation` | ToolBody 17 字节、Stdin 47 字节、逐字节十六进制与上面一致、往返解码一致。 |
| `gpio_write_result_payload_matches_documentation` | Completed 载荷为 15 字节 `pin=IO4 level=1`。 |

文档修改示例时必须同步修改测试（先改测试、再改文档），不允许文档与测试不一致。

---

## 8. Host 虚拟后端

宿主 `sekai-host`（x86）默认注册虚拟 GPIO 工具，用于没有硬件时验证完整的 MQTT ToolCall 路径。

### 8.1 环境变量

| 环境变量 | 默认值 | 取值 | 说明 |
| --- | --- | --- | --- |
| `SEKAI_GPIO` | `on` | `on`/`off` | `off` 时完全不注册任何 `gpio.*` 工具，也不再读取/校验另外两个变量。 |
| `SEKAI_GPIO_FAMILY` | `esp32` | `esp32`/`stm32f1`/`stm32f4` | 选择引脚表；大小写不敏感、首尾空白忽略。 |
| `SEKAI_GPIO_INPUTS` | 空 | `PIN=LEVEL[,PIN=LEVEL...]` | 初始化虚拟输入的外部电平；空表示全部低电平。 |

解析细节（与原 `crate/sekai-start/src/driver/x86.rs` 一致；本节与 §8.3/§8.4 均为重构前
`sekai-host` 的行为，现行宿主 CLI 见 `crate/sekai-worker/src/bin/sekai-worker.rs`）：

- `SEKAI_GPIO`：未设置或空字符串 → `on`；先 trim 再大小写不敏感匹配 `on`/`off`；
  非法值 → stderr 打印 `[sekai] SEKAI_GPIO 非法：<值> 应为 on 或 off；使用默认值 on`，按 `on` 处理。
- `SEKAI_GPIO_FAMILY`：未设置或空 → `esp32`；trim + 大小写不敏感；
  非法值 → stderr 打印 `[sekai] SEKAI_GPIO_FAMILY 非法：<值> 应为 esp32、stm32f1 或 stm32f4；使用默认值 esp32`。
- `SEKAI_GPIO_INPUTS`：英文逗号分隔，每项 `PIN=LEVEL`；首尾空白忽略，空项/末尾逗号忽略；
  `PIN` 与 `LEVEL` 大小写不敏感；`LEVEL` 接受 `0`/`1`/`low`/`high`/`on`/`off`。
  逐项解析，非法项只跳过该项并打印 warning，其余项照常应用：
  - 缺 `=`：`[sekai] SEKAI_GPIO_INPUTS：项 <项> 应为 PIN=0|1`；
  - 引脚语法/编号非法：沿用引脚解析错误（如 `引脚名 IO04 非法：引脚编号不能有前导零`、`引脚 IO40 不存在`）；
  - 电平非法：`电平 <值> 非法；应为 0/1/low/high/on/off`；
  - 保留引脚：`引脚 IO6 已保留（SPI flash），忽略`；
  - 表外引脚：按当前家族表判断（`esp32` 下 `PA5` 不存在；F1 无 `PC0`；F4 有 `PE0`）。

### 8.2 虚拟后端语义

- `backend=virtual`；`gpio.info` 摘要会输出 `backend=virtual`。
- 校验语义与真实硬件完全一致：保留/仅输入/模式/能力/不存在都会在“操作硬件”之前拒绝。
- 输入读取返回初始化时设置的外部电平；输出写入更新锁存电平；未配置的读写返回“未配置”。
- `configured` 统计已配置引脚数；`gpio.info PIN` 对未配置引脚输出 `mode=unconfigured`。
- 虚拟后端只用于验证控制路径，**不产生真实电平**；需要物理效果时必须换成 ESP32/STM32 后端。

### 8.3 完整示例

```bash
# 终端 1：MQTT broker（示例）
mosquitto -p 1883

# 终端 2：宿主虚拟设备（ESP32 引脚表，IO4 外部输入初始为高）
SEKAI_MQTT_ADDR=127.0.0.1:1883 \
SEKAI_DEVICE_ID=host-gpio \
SEKAI_GPIO_FAMILY=esp32 \
SEKAI_GPIO_INPUTS=IO4=1 \
cargo run -p sekai-start --bin sekai-host   # 重构前命令（原 crate/sekai-start，MQTT 链路）

# 终端 3：用任意 MQTT 5 客户端按 docs/sekai-protocol.md 构造 ToolCall：
#   gpio.info help
#   gpio.info
#   gpio.config IO4 out level=0
#   gpio.write IO4 1
#   gpio.config IO4 in pull=up
#   gpio.read IO4
```

STM32F1 引脚表示例：

```bash
SEKAI_MQTT_ADDR=127.0.0.1:1883 \
SEKAI_DEVICE_ID=host-f1 \
SEKAI_GPIO_FAMILY=stm32f1 \
SEKAI_GPIO_INPUTS=PA5=1,PC13=0 \
cargo run -p sekai-start --bin sekai-host   # 重构前命令（原 crate/sekai-start，MQTT 链路）
```

关闭虚拟 GPIO（只保留内置工具与示例工具）：

```bash
SEKAI_MQTT_ADDR=127.0.0.1:1883 SEKAI_GPIO=off \
cargo run -p sekai-start --bin sekai-host   # 重构前命令（原 crate/sekai-start，MQTT 链路）
```

### 8.4 sekai-host usage（GPIO 相关段落）

无 `SEKAI_MQTT_ADDR` 时 `sekai-host` 打印 usage 并空转。以下两段均为重构前 `sekai-host`
（原 `crate/sekai-start`）的 usage 原文；现行宿主 usage 见 `crate/sekai-worker/src/bin/sekai-worker.rs`。
GPIO 相关段落逐字如下：

```text
  SEKAI_GPIO              虚拟 GPIO 开关：on（默认）| off
  SEKAI_GPIO_FAMILY       虚拟 GPIO 引脚表：esp32（默认）| stm32f1 | stm32f4
  SEKAI_GPIO_INPUTS       初始输入电平，例如 IO4=1,PA5=0（可空）
```

示例行（重构前 `sekai-host` usage 原文，原 `crate/sekai-start`）：

```text
  SEKAI_MQTT_ADDR=127.0.0.1:1883 SEKAI_GPIO_FAMILY=stm32f1 SEKAI_GPIO_INPUTS=PA5=1 cargo run -p sekai-start --bin sekai-host
```

### 8.5 工具数量与上限

- 工具总数（含 6 个 `sekai.*` 内置工具）上限为 16：`Runtime::new` 校验
  “用户注册表工具数 + 6 个内置工具 ≤ 16”。
- 宿主默认：6 内置 + `echo` + 4 个 GPIO = **11**。
- `SEKAI_GPIO=off`：6 内置 + `echo` = 7，释放 4 个工具槽位。
- 边界示例：在默认 5 个用户工具（echo + 4 GPIO）基础上再加 5 个用户工具，用户工具共 10、总数 16，仍合法；
  再加第 6 个（用户 11）时 `Runtime::new` 返回错误。
- 应用要注册更多工具时，应先关闭 `SEKAI_GPIO` 或减少自定义工具，保证 Hello 的 `tools` 不超过协议上限。

---



## 9. ESP32 真实后端

ESP32 classic 后端使用 `esp-hal` 1.2.2 的 `Flex`（`unstable` 特性）统一运行时的
输入/输出/上下拉/开漏切换，位于 `crate/sekai-gpio/src/esp32.rs`。

### 9.1 feature 与编译

- `sekai-gpio` 的 `esp32` feature = `dep:esp-hal` + `send`（隐含异步运行时需要的 `Send` 约束）。
- `sekai-embedded` 的 `esp32` feature 会启用 `sekai-gpio/esp32` 与 `sekai/esp32`。
- 编译验证：`cargo run -p sekai-build -- build`（见 `docs/sekai-esp32.md`）。

### 9.2 初始化与注册

```rust
// 必须在 esp_hal::init(...) 之后调用；此后应用不再直接使用 Peripherals.GPIOx。
let gpio = sekai_gpio::esp32::Esp32Gpio::new();

let mut tools = ToolRegistry::new();
sekai_gpio::register_tools(&mut tools, gpio).expect("注册 GPIO 工具");
```

- `Esp32Gpio::new()` **不接收** `Peripherals`，只创建 40 个空槽位；某个引脚第一次被 configure 时才真正取得它。
- 参考驱动（`crate/sekai-embedded/src/driver/esp32/mod.rs`）的注册位置：WiFi/DHCP 就绪后、构造 `Runtime` 之前。
- `esp32` feature 隐含 `send`，所以 `register_tools` 要求端口 `Send`；`Flex`/`AnyPin` 本身是 `Send`。
  如果编译器报告某字段非 `Send`，应调整持有方式，**不得**使用 `unsafe impl Send` 绕过。

### 9.3 安全不变量（`AnyPin::steal`）

后端用 `unsafe { esp_hal::gpio::AnyPin::steal(number) }` 延迟取得引脚，这是 **ESP32 模块**唯一的 unsafe 点
（crate 内其余 unsafe 只在 STM32 的 `f1.rs`/`f4.rs` 寄存器读写）。调用方必须保证：

1. `Esp32Gpio::new()` 在 `esp_hal::init()` **之后**调用；
2. 应用把 GPIO 交给本后端后，**不再直接使用** `Peripherals.GPIOx` 或任何 GPIO token；
3. 每个引脚只 steal 一次：槽位为 `Some` 后不再 steal（`Esp32Gpio` 内部保证）。

安全边界由核心校验保护：保留/不存在/能力不足的引脚在 steal 之前就被拒绝，因此不会出现
“先拿到引脚再报错”的情况。

### 9.4 配置映射

| 最终配置 | esp-hal 操作 |
| --- | --- |
| 输入 | `set_output_enable(false)` → `set_input_enable(true)` → `apply_input_config(InputConfig::default().with_pull(map_pull))` |
| 输出 | `set_level(map_level)` → `apply_output_config(OutputConfig::default().with_drive_mode(OpenDrain/PushPull))` → `set_output_enable(true)` → `set_input_enable(false)` |

- `read`（输入）读 `flex.level()`；`write`/`describe`（输出）读 `flex.output_level()` 或最近写入的锁存值。
- 重新配置同一引脚是安全的：重复上述步骤，`Flex` 保留在槽位中。
- 保留/能力校验在核心 `resolve_config`/`validate_config` 完成，后端不重复校验。

### 9.5 引脚警告

| 引脚 | 风险 | 文档/工具提示 |
| --- | --- | --- |
| IO1/IO3 | UART0 TX/RX；`esp-println` 默认占 UART0 打印日志 | `gpio.info` 标记 `uart0`；不要把这两脚配置为输出，否则日志/烧录失败。 |
| IO6..IO11 | SPI flash 总线 | `flags=reserved`；任何 configure/read/write 都拒绝。 |
| IO0/2/5/12/15 | strapping，复位电平决定启动模式 | `flags=strap`；输出时避免复位期毛刺，不要在上电时外部拉错。 |
| IO16/IO17 | WROVER 模块 PSRAM 可能占用 | `flags=psram`；使用 PSRAM 的模块不要当普通 IO。 |
| IO34..IO39 | 无输出驱动与内部上下拉 | `flags=input_only`、`caps=in`；只能做输入。 |

### 9.6 限制

- 只支持 ESP32 classic（Xtensa）；S3/C3/C6 需要新的家族表与后端。
- 依赖 `esp-hal` 的 `unstable` API；升级 esp-hal 时需重新验证 `Flex` 接口。
- 真实电平/时序需要硬件验证；仓库内只做编译验证与宿主单元测试。

---

## 10. STM32F1/F4 板级使用

STM32 后端直接使用 `stm32f1`/`stm32f4` PAC 寄存器，不依赖 HAL 的类型状态；
`unsafe` 只出现在 `src/stm32/f1.rs`、`src/stm32/f4.rs` 的寄存器读改写中。

### 10.1 feature、目标与交叉编译

| 家族 | feature | 目标 triple | 参考芯片 |
| --- | --- | --- | --- |
| F1 | `stm32f1` | `thumbv7m-none-eabi` | STM32F103C8（Blue Pill） |
| F4 | `stm32f4` | `thumbv7em-none-eabi` | STM32F407VG（LQFP100） |

```bash
# F1 交叉编译检查
rustup target add thumbv7m-none-eabi
cargo check -p sekai-gpio --no-default-features --features stm32f1 --target thumbv7m-none-eabi

# F4 交叉编译检查
rustup target add thumbv7em-none-eabi
cargo check -p sekai-gpio --no-default-features --features stm32f4 --target thumbv7em-none-eabi
```

- 交叉编译只覆盖 `sekai-gpio` 库本身；最终固件的链接脚本、启动文件、入口由**用户板级工程**提供，
  仓库本轮不提供 STM32 固件。
- `stm32f1`/`stm32f4` feature 只拉入对应 PAC，不启用 `send`；板级工程按需自行加 `send`。

### 10.2 F1 板级示例

```rust
use stm32f1::stm32f103 as pac;

use sekai_gpio::config::{Direction, Level, PinConfig, Pull};
use sekai_gpio::pin::{PinId, Port};
use sekai_gpio::stm32::Stm32Gpio;
use sekai_gpio::stm32::f1::{F1, F1Ports};

let dp = pac::Peripherals::take().expect("外设只能初始化一次");

// 后端接管 GPIOA/B/C 的所有权，并使能它们的 GPIO 时钟。
let mut gpio = Stm32Gpio::<F1>::new(
    F1Ports {
        gpioa: dp.GPIOA,
        gpiob: dp.GPIOB,
        gpioc: dp.GPIOC,
    },
    &mut dp.RCC,
);

// PC13 输出高（Blue Pill 常见板载 LED 为低电平点亮，这里只演示 API）。
gpio.configure(
    PinId::port(Port::C, 13),
    PinConfig {
        direction: Direction::Output,
        level: Level::High,
        ..Default::default()
    },
)
.expect("配置 PC13");

// 翻转输出。
gpio.write(PinId::port(Port::C, 13), Level::Low).expect("写 PC13");

// PA5 输入上拉，并读取。
gpio.configure(
    PinId::port(Port::A, 5),
    PinConfig {
        direction: Direction::Input,
        pull: Pull::Up,
        ..Default::default()
    },
)
.expect("配置 PA5");
let level = gpio.read(PinId::port(Port::A, 5)).expect("读 PA5");
```

### 10.3 F4 板级示例

```rust
use stm32f4::stm32f407 as pac;

use sekai_gpio::config::{Direction, Level, PinConfig};
use sekai_gpio::pin::{PinId, Port};
use sekai_gpio::stm32::Stm32Gpio;
use sekai_gpio::stm32::f4::{F4, F4Ports};

let dp = pac::Peripherals::take().expect("外设只能初始化一次");

let mut gpio = Stm32Gpio::<F4>::new(
    F4Ports {
        gpioa: dp.GPIOA,
        gpiob: dp.GPIOB,
        gpioc: dp.GPIOC,
        gpiod: dp.GPIOD,
        gpioe: dp.GPIOE,
    },
    &mut dp.RCC,
);

gpio.configure(
    PinId::port(Port::A, 5),
    PinConfig {
        direction: Direction::Output,
        level: Level::Low,
        ..Default::default()
    },
)
.expect("配置 PA5");
gpio.write(PinId::port(Port::A, 5), Level::High).expect("写 PA5");
```

### 10.4 所有权、时钟与保留引脚

- **只接管 GPIO 端口与 GPIO 时钟位**：`new()` 会读改写 RCC 中对应端口的时钟使能位，保留其他外设
  （USART/SPI/TIM 等）的时钟位与所有权；重复置位是幂等的。
- **端口 token 移交后不再被其他代码使用**：`F1Ports`/`F4Ports` 取得 `GPIOx` 的所有权，
  板级工程不要把同一个 token 再交给别的驱动；需要直接操作端口时可调用 `gpio.ports()`。
- **保留引脚**：PA13、PA14（SWD）、PA15、PB3、PB4（JTAG）。工具层/后端在触碰寄存器前拒绝它们；
  真实产品若确实要复用，需要自行修改 `PinSpec` 表并承担失去调试口的风险。
- **F1 表内 35 个引脚**：PA0-PA15、PB0-PB15、PC13-PC15；PC0..PC12 不存在。
  **F4 表内 80 个引脚**：PA0-PE15。
- **本轮不提供** STM32 的链接脚本、启动文件、固件入口与网络链路；板级集成（`#[entry]`/`main`）
  由用户工程完成。

### 10.5 测试与硬件验证

- 仓库内的 STM32 测试使用 `#[cfg(test)]` 的 mock family（`Rc<RefCell<MockRegs>>`），验证状态机、
  寄存器写入值、保留/能力错误不触碰硬件等；真实寄存器路径只做交叉编译检查。
- 有板时必须自行验证点灯、按键输入、上下拉/开漏与保留引脚拒绝（见 §12.6 的验收思路）。

---


## 11. 新家族/新后端移植指南

本 crate 的扩展点有三层：引脚表（`FamilySpec`）、后端（`GpioPort` 或 `Stm32Family`）、工具注册。
下面以“新增一个 MCU 家族”为例说明步骤与测试要求。

### 11.1 第一步：定义引脚表（`FamilySpec`）

1. 为每个可用引脚定义 `PinSpec`：

```rust
pub struct PinSpec {
    pub id: PinId,          // PinId::Io(4) 或 PinId::port(Port::A, 5)
    pub slot: u16,          // 后端状态数组下标，必须唯一
    pub caps: PinCaps,      // input/output/pull_up/pull_down/open_drain
    pub flags: PinFlags,    // 9 个固定标志，按固定顺序输出
    pub note: &'static str, // 保留原因/警告，可为空
}
```

2. 定义 `FamilySpec` 常量并放进 `ChipFamily::spec()`：

```rust
pub struct FamilySpec {
    pub family: FamilyKind,        // Esp32 或 Stm32
    pub chip: &'static str,        // gpio.info 的 chip= 文本
    pub pins: &'static [PinSpec],  // 表内引脚（按槽位升序）
    pub slots: usize,              // 后端状态数组长度
    pub ports: &'static [Port],    // STM32 端口列表；ESP32 为空
}
```

3. 如果是全新芯片家族，还需要：
   - 在 `ChipFamily` 增加枚举分支，并在 `kind()`/`chip()`/`spec()`/`parse()` 中处理；
   - 在 `ChipFamily::parse` 中接受宿主环境变量 `SEKAI_GPIO_FAMILY` 的新取值（例如 `stm32f1`）。
4. 引脚编号规则：
   - ESP32 形式：`slot = IO 编号`，编号 0..=39；
   - STM32 形式：`slot = 端口序号 × 16 + 引脚号`；
   - 表内 `id` 与 `slot` 都必须唯一（有测试断言）。
5. 保留/仅输入/特殊功能引脚必须写入 `flags` 或 `note`，不要依赖后端自行拒绝。

### 11.2 第二步：实现后端

有两条路线：

**路线 A：直接实现 `GpioPort`**（适合任意新 MCU/HAL）：

```rust
pub trait GpioPort {
    fn spec(&self) -> &'static FamilySpec;
    fn backend(&self) -> BackendKind { BackendKind::Hardware }
    fn configure(&mut self, pin: PinId, config: PinConfig) -> Result<(), GpioError>;
    fn read(&mut self, pin: PinId) -> Result<Level, GpioError>;
    fn write(&mut self, pin: PinId, level: Level) -> Result<(), GpioError>;
    fn describe(&mut self, pin: PinId) -> Result<PinDescription, GpioError>;
    fn configured_count(&self) -> usize { 0 }
}
```

实现要求：

- 每次操作先 `lookup` 引脚表、再 `validate_config`（configure）或
  `ensure_input_mode`/`ensure_output_mode`（read/write），最后才碰硬件；
- `describe` 对未配置引脚也返回成功（`config=None`、`level=None`），供 `gpio.info` 使用；
- 输出锁存电平要在状态中记录，`describe` 返回它；
- 共享状态机代码保持安全；`unsafe` 只出现在具体的寄存器/HAL 访问文件里，并在 crate 内单独放宽
  `deny(unsafe_code)`。

**路线 B：实现 `Stm32Family`**（适合 STM32 子家族）：

```rust
pub trait Stm32Family {
    const SPEC: &'static FamilySpec;
    type Ports;
    type Rcc;
    fn enable_clocks(rcc: &mut Self::Rcc, ports: &mut Self::Ports);
    fn configure(ports: &mut Self::Ports, spec: &'static PinSpec, config: PinConfig);
    fn read(ports: &Self::Ports, spec: &'static PinSpec) -> Level;
    fn write(ports: &mut Self::Ports, spec: &'static PinSpec, level: Level);
}
```

然后直接用 `Stm32Gpio<YourFamily>`，无需重复实现校验/状态机：

- 在 `src/stm32/` 下新增文件（参考 `f1.rs`/`f4.rs` 的 CRL/CRH 或 MODER/PUPDR/OTYPER/OSPEEDR 写法）；
- 在 `src/stm32/mod.rs` 加 `#[cfg(feature = "yourchip")] pub mod yourchip;`；
- 在 `Cargo.toml` 加对应 PAC 依赖与 feature（例如 `stm32f1 = ["dep:stm32f1"]`）；
- `enable_clocks` 只置位 GPIO 端口时钟位，保留其他外设位，读改写保证幂等。

### 11.3 第三步：注册工具

```rust
let mut tools = ToolRegistry::new();
sekai_gpio::register_tools(&mut tools, my_port).expect("注册 GPIO 工具");
```

- 默认构建共享句柄是 `Rc<RefCell<P>>`；`send` 下是 `Arc<critical_section::Mutex<RefCell<P>>>`，
  且要求 `P: Send`。
- 若应用自己已经有同名 `gpio.*` 工具，注册会返回 `RegistryError::Duplicate`（可能部分注册）。
- 注册后 Hello 自动包含四个工具；确认工具总数（含 6 个内置工具）不超过 16。

### 11.4 第四步：测试清单

| 测试 | 建议位置 | 断言 |
| --- | --- | --- |
| 家族表 | `tests/spec_conformance.rs` | 引脚数/slots/保留/仅输入/不存在、`id` 与 `slot` 唯一。 |
| 引脚解析 | `src/pin.rs` 单元测试 | `IO4`/`GPIO4`/`4`、`PA5`/`pa5`/大小写；`IO04`/`PA16`/表外引脚反例。 |
| 配置九条规则 | `src/config.rs` 单元测试 | 默认输入、输入/输出切换、方向敏感字段、能力/保留拒绝。 |
| 虚拟后端一致性 | `tests/virtual_backend.rs` | 三种家族下的读写/模式/保留/外部电平/`configured_count`。 |
| 工具契约 | `tests/tools.rs` | help 逐字、结果 key 顺序、错误码、256/1024 边界。 |
| 运行时集成 | `tests/runtime_tools.rs` | Hello 含 4 个 gpio 工具；config/write/read 成功与失败路径；工具数边界。 |
| `send` | `tests/send_tools.rs` | `Arc` 句柄下注册与调用（`critical-section/std` 提供宿主实现）。 |
| 交叉编译/固件 | 命令矩阵 | `cargo check` 对应 target；ESP32 走 `sekai-build build`。 |

### 11.5 第五步：文档

- 在本文 §3 补新家族引脚表与 flags/note 说明；
- 若新家族有真实固件/构建链，另写/扩展对应设备文档（参考 `docs/sekai-esp32.md`）；
- 同步更新 `docs/sekai-protocol.md` §12.1 文件职责表与 §12.4 工具说明（若工具集有变化）。

---

## 12. 安全与限制

### 12.1 GPIO 是物理执行能力

- GPIO 输出直接驱动真实电路：LED、继电器、电机驱动、传感器供电使能等。错误的电平/方向
  可能造成短路、过流、设备损坏或人身风险；AI 调用必须经过人工确认的场景要由上层业务控制。
- 不要驱动未确认用途的引脚；对 strapping、调试口、flash、PSRAM、仅输入引脚尤其如此。
- 输出冲突（两个输出对接、输出对电源/地短路）属于电气风险，协议/工具层无法检测。

### 12.2 信任边界：broker ACL

协议本身**不提供认证**，任何能向 `{root}/dev/{device_id}/in` 发布合法 Stdin 的主体都能调用
`gpio.write` 等工具。安全边界完全由 MQTT broker 的认证与 ACL 承担：

| 主体 | 允许 | 禁止 |
| --- | --- | --- |
| Router/控制端 | 发布 `{root}/dev/+/in`；订阅 `out`/`state` | 其他 Worker 主题 |
| 单个 Worker | 发布自己的 `out`/`state`；订阅自己的 `in` | 其他 Worker 主题 |
| 其他主体 | 无 | 全部 |

- **不要把 `in` 主题暴露给不可信主体**；Router 凭据泄露等价于 Worker GPIO 被完全控制。
- Worker 无法校验命令来源；不要向不可信 broker/公网明文 broker 暴露 Worker。
- 建议按 Worker/租户隔离凭据与 ACL，并审计 `gpio.*` 调用日志。

### 12.3 特殊引脚纪律

- 保留引脚（ESP32 IO6..IO11；F1/F4 PA13/PA14/PA15/PB3/PB4）：任何操作前拒绝，永远不要解除。
- strapping 引脚（ESP32 IO0/2/5/12/15）：上电电平影响启动；不要让外部电路在复位期拉错。
- UART0（ESP32 IO1/IO3）：被日志/烧录占用时不要作普通 IO；配置为输出可能导致日志丢失/启动异常。
- PSRAM（ESP32 IO16/IO17）：WROVER 模块可能已占用。
- 仅输入（ESP32 IO34..IO39）：没有输出驱动与内部上下拉；不要尝试输出或上下拉。
- STM32 PC13（note `RTC`）与 PC14/PC15（`osc`）：若板级使用 LSE/RTC，避免当普通 IO。

### 12.4 传输与认证

- 参考实现的宿主链路是明文 TCP（`TcpLink`）；ESP32 固件也是明文 MQTT 5 over TCP，**无 TLS**。
- 协议无签名/加密/认证；MQTT 用户名/密码只是 broker 认证，不保护载荷。
- 部署必须在受信任网络内，或在 MQTT 传输层加 TLS（由部署/启动层决定，协议帧不感知）。
- 本轮不支持 ESP32 的 TLS/DNS/静态 IP/NVS 配网；broker 必须用 IPv4 字面量地址。

### 12.5 资源与拒绝服务

- 工具参数上限 256 字节、结果上限 1024 字节、工具总数（含 6 个内置）≤16。
- 工具同步执行且受 `ddl_ms` 预算约束（默认 500ms）；处理器不得阻塞、忙等或做长循环。
- 错误消息包含引脚名/字段名，由协议层截断到 256 字节，避免放大输出。
- Router 应限速 ToolCall；大量非法参数/高频写 GPIO 会占用 Worker CPU 并可能损坏外部电路。

### 12.6 验证边界与验收

- **虚拟后端不能代替真实硬件验证**：它只验证控制路径、参数、错误语义；电气行为必须用硬件验收。
- ESP32 后端在仓库内只做编译验证；真实 GPIO 电平/时序需要开发板。
- STM32 后端只做交叉编译检查 + mock 状态机测试；真实寄存器行为需要用户板级工程验证。
- 验收清单（有硬件时）：
  - ESP32：串口日志、retained Hello 含 4 个 GPIO 工具、`gpio.info help` 正确、
    `gpio.config IO2 out level=0` + `gpio.write IO2 1` 驱动 LED、断网退避重连、Jump 到第二 broker。
  - STM32：用板级工程构造 `Stm32Gpio::<F1/F4>`，验证 LED 输出、输入上拉读取、保留引脚拒绝。

### 12.7 本轮不支持的 GPIO 能力

| 不支持 | 说明 / 替代做法 |
| --- | --- |
| 中断、边沿检测、脉冲计数 | 本轮只轮询读写电平；需要时在板级工程自行实现。 |
| PWM / ADC / 复用功能（AF） | 不在工具语法内；需要时使用厂商 HAL 直接操作。 |
| ESP32-S3/C3/C6 等变体 | 需要新增家族表与后端。 |
| STM32 网络链路 | F1/F4 无内置 WiFi，需要外部 PHY/模块另行设计。 |
| 二进制参数/结果 | 固定 UTF-8 文本，协议对内容不透明。 |
| 运行时解除保留 | 只能修改 `PinSpec` 表，安全默认值不可在运行时绕过。 |
| TLS/认证/DNS/静态 IP/NVS | 由部署与后续版本解决，本轮无内建支持。 |

---

## 附录 A：文档与测试的对应关系

本文中的关键契约都由测试锁定，修改实现或文档时必须同步：

| 文档内容 | 锁定位置 |
| --- | --- |
| `gpio.info help` 逐字文本 | `src/tools.rs` 的 `INFO_HELP` + `tests/tools.rs` |
| 四个工具结果 key 顺序/错误消息 | `tests/tools.rs`（32 个集成断言） |
| 家族表（34/35/80、slots、保留/仅输入/不存在） | `src/family.rs` 单元测试 + `tests/spec_conformance.rs` |
| 引脚解析正反例 | `src/pin.rs` 单元测试 + `tests/spec_conformance.rs` |
| 配置九条规则/能力校验 | `src/config.rs` 单元测试 + `tests/spec_conformance.rs` |
| 虚拟后端语义 | `tests/virtual_backend.rs` |
| Stdin 帧 47 字节/Completed 15 字节 | `tests/docs_examples.rs` |
| Hello 工具列表与运行时集成 | `tests/runtime_tools.rs` |
| 宿主环境变量/usage 文本 | 原 `crate/sekai-start/src/driver/x86.rs` 单元测试（git 历史）；现行宿主 usage 文本见 `crate/sekai-worker/src/bin/sekai-worker.rs` |
| ESP32 固件编译 | `cargo run -p sekai-build -- build`（编译验证，无硬件电气验证） |

---



