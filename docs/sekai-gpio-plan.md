# Sekai 抽象 GPIO + ESP32/STM32 家族支持 + ESP32 最小 WiFi/TCP 链路实施计划

> 状态：本计划已实施并经 2025 Router/Worker 重构（原 server→Router、设备→Worker、`sekai-start` 拆分为 `sekai-worker`/`sekai-embedded`）。文中 `sekai-start`/`sekai-host` 等旧名按历史记录保留，现行对应关系见 `docs/sekai-architecture.md`。

> **状态**：计划（仅输出，不执行）。本文件由计划阶段生成；未修改任何源代码、未运行构建/测试、未提交。
>
> **执行方式**：切换到新 session 后按本文档实施。本计划的 Phase 0-8 命令均由执行者运行。

## 0. 已确认决策与假设

本计划覆盖以下用户已确认的范围：

- **STM32 同时做 F4 与 F1**：F4 以 STM32F407VG（LQFP100，PA0-PE15）为参考；F1 以 STM32F103C8（Blue Pill，PA0-PB15 + PC13-PC15）为参考。两者共用同一套家族无关抽象、工具层和测试，底层分别用 PAC 寄存器实现。
- **ESP32 包含最小 WiFi/TCP 链路**：ESP32 classic（不是 S3/C3），Station 模式 + DHCP + 明文 MQTT 5 over TCP；让运行时真正跑在 ESP32 上，AI 可以通过 MQTT ToolCall 操作 GPIO。
- **工具参数使用 UTF-8 文本语法**，不采用二进制布局。理由：协议规定 Tool 参数对协议层不透明；文本对 AI 生成、人工调试、日志排查都更友好；语法会严格校验并完整写入文档。
- **GPIO 工具注册为用户工具 `gpio.*`**，不是 `sekai.*` 内置工具。这样 `sekai` 运行时保持硬件无关，工具通过 Hello 正常通告，且不占用保留前缀。
- **GPIO 抽象放在新 crate `sekai-gpio`**：核心、家族表、虚拟后端、文本工具层默认不依赖 HAL；ESP32 和 STM32 后端用 feature 隔离。`sekai-start` 只负责平台初始化和工具注册。
- **本轮不做**：TLS、DNS、静态 IP、NVS/WiFi 配网、AP 模式、BLE、ESP32-S3/C3、STM32 网络链路、GPIO 中断/边沿检测/PWM/ADC/复用功能、多板级自动探测。STM32 本轮交付“抽象 + 可编译后端 + 单元测试 + API 文档”，硬件点灯示例由用户板级工程完成。
- **安全边界**：GPIO 是物理执行能力，协议本身无认证；文档必须强调 broker ACL、不要把 `in` 主题暴露给不可信主体。

假设执行环境具备：

- Rust 1.99 / edition 2024，已安装 `esp` 工具链（`espup install -t esp32`）。
- 可以 `rustup target add thumbv7m-none-eabi`；`thumbv7em-none-eabi` 已安装。
- 可以访问 crates.io/镜像下载 `stm32f1`、`stm32f4`、`embassy-net`、`esp-println` 等新依赖。
- 可用的 ESP32 开发板、STM32F103C8/F407VG 板、MQTT 5 broker 用于最终硬件验收；没有硬件时，编译矩阵 + host 虚拟端到端测试仍必须通过。

---

## 1. 目标与验收

### 1.1 目标

1. 提供一套家族无关、`no_std` 的 GPIO 抽象，覆盖：
   - 引脚命名/解析：ESP32 `IO4`/`GPIO4`，STM32 `PA5`；
   - 方向、电平、上下拉、开漏；
   - 能力/保留引脚校验；
   - 统一错误到 `CommandError`/`ErrorCode`。
2. 在抽象之上提供 AI 可直接调用的工具：
   - `gpio.info`（含 `help` 自描述、家族摘要、单引脚详情、STM32 按端口列引脚）；
   - `gpio.config`；
   - `gpio.read`；
   - `gpio.write`。
3. 提供 ESP32 classic 真实 GPIO 后端（`esp-hal` 1.2.2 `Flex`）。
4. 提供 STM32F1/F4 真实 GPIO 后端（直接 PAC 寄存器；F1 用 CRL/CRH，F4 用 MODER/PUPDR/OTYPER/OSPEEDR）。
5. 提供 host 虚拟后端，支持 ESP32/F1/F4 三种引脚表，AI 可以在没有硬件时完整走 MQTT ToolCall 流程。
6. 在 ESP32 上接入最小 WiFi/TCP 链路，使 `Runtime` 真正运行并发布 Hello、处理 ToolCall。
7. 完整文档、单元测试、运行时集成测试、编译矩阵。

### 1.2 成功标准（命令矩阵）

以下命令全部通过：

```bash
# 1. 现有 host 测试
cargo test --workspace

# 2. 新增 send 特性测试（ESP32 异步运行时前置）
cargo test -p sekai --features send
cargo test -p sekai-gpio --features send

# 3. STM32 交叉编译检查
rustup target add thumbv7m-none-eabi
cargo check -p sekai-gpio --no-default-features --features stm32f1 --target thumbv7m-none-eabi
cargo check -p sekai-gpio --no-default-features --features stm32f4 --target thumbv7em-none-eabi

# 4. ESP32 固件构建（WiFi/TCP + GPIO 工具 + 异步运行时）
cargo run -p sekai-build -- build

# 5. 质量门禁
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
```

此外：

- `sekai-gpio` 的运行时集成测试必须断言：
  - Hello 包含 `gpio.info/config/read/write`；
  - `gpio.info help` 返回自描述文本；
  - `gpio.config`、`gpio.write`、`gpio.read` 的 `Completed` 载荷符合契约；
  - 非法引脚/保留引脚/错误模式返回 `InvalidArgument`；
  - 6 个内置工具 + 4 个 GPIO 工具 + 应用工具不超过 16。
- host `sekai-host` 在 `SEKAI_GPIO_FAMILY=esp32|stm32f1|stm32f4` 下都能注册工具并正常处理 ToolCall。
- ESP32 硬件验收（有板时）：
  - 串口打印 WiFi 连接、DHCP 地址、MQTT 连接；
  - broker 收到 retained Hello，`tools` 含 4 个 GPIO 工具；
  - AI 调 `gpio.config IO2 out level=0`、`gpio.write IO2 1` 能驱动板载 LED/逻辑分析仪。
- STM32 硬件验收（有板时）：
  - 用户板级工程能 `Stm32Gpio::<Stm32F1>::new(...)` / `Stm32Gpio::<Stm32F4>::new(...)`；
  - `configure`/`write`/`read` 能操作一个 LED 和输入引脚。

### 1.3 不在本轮

- ESP32 上的 MQTT over TLS、DNS 主机名解析、静态 IP、NVS 配网。
- STM32 的 MQTT 链路（F1/F4 无内置 WiFi；需要外部 PHY/模块，另行设计）。
- GPIO 中断、边沿事件、脉冲计数、PWM、ADC、复用功能。
- 多款 ESP32 变体（S3/C3/C6）和更多 STM32 子家族；只保留扩展点。
- 工具二进制参数格式；本轮固定 UTF-8 文本。
- 运行时协议变更；本轮不新增 Stdin/Stdout 帧类型或错误码。

---

## 2. 现状与缺口

当前仓库：

- `crate/sekai/`：协议、MQTT 5 编解码、运行时、Task/Tool 注册表、内置工具；`#![no_std]`、`#![forbid(unsafe_code)]`。
- `crate/sekai-start/`：
  - `x86`：TCP + MQTT + 控制平面运行时，示例任务 `echo`/`sleep`、示例工具 `echo`；
  - `esp32`：只有 `esp_hal::init` + 空转，没有 WiFi/TCP、没有 Runtime、没有 GPIO；
  - `time.rs`：已有 ESP32/宿主时间校准；
  - `framing.rs`/`link.rs`：只有 x86 使用。
- `crate/sekai-build/`：ESP32 固件构建/烧录驱动。
- `docs/sekai-protocol.md`：明确说 GPIO 是 Tool 示例，但没有实现。
- 没有任何 GPIO 工具、GPIO 抽象、STM32 支持、WiFi 链路。

关键缺口：

1. `sekai` 的 `ToolRegistry`/`TaskRegistry` 里是 `Box<dyn ToolHandler>` / `Box<dyn TaskHandler>`，没有 `Send` 约束；ESP32 的 `#[embassy_executor::task]` 异步 main 需要 future 为 `Send`。
2. `sekai-start` 的 `esp32` 驱动没有运行时、没有网络、没有 GPIO。
3. ESP32 引脚是 `IO0..IO39`，STM32 是 `PA5` 形式；需要统一运行时表示。
4. F1 HAL 的 GPIO 动态模式比 F4 更受限；直接使用 PAC 寄存器可以同时覆盖 F1/F4，并避免 HAL 类型状态。
5. host 需要一个虚拟后端，才能在没有硬件时验证 AI 调用路径。

---

## 3. 总体架构

### 3.1 crate 依赖方向

```text
sekai (协议/runtime，no_std)
  ↑
sekai-gpio (抽象 + 家族表 + 虚拟后端 + 工具 + 可选 ESP32/STM32 后端)
  ↑                         ↑
sekai-start (x86/esp32)    未来 STM32 板级工程
```

- `sekai-gpio` 依赖 `sekai` 的 `ToolHandler`、`ToolRegistry`、`CommandError`。
- `sekai` 不依赖 `sekai-gpio`，协议层保持硬件无关。
- `sekai-start` 依赖 `sekai-gpio`：
  - `x86` 注册虚拟 GPIO 工具；
  - `esp32` 注册真实 ESP32 GPIO 工具，并跑 WiFi/TCP 运行时。
- STM32 板级工程可以直接依赖 `sekai-gpio` + `stm32f1`/`stm32f4` PAC。

### 3.2 feature 矩阵

| crate | feature | 作用 |
| --- | --- | --- |
| `sekai` | `x86` | 现有 host 构建，无变化 |
| `sekai` | `esp32` | 新：等价于 `send`，为异步运行时准备 |
| `sekai` | `send` | 新：注册表强制 `Send` 处理器，使 `Runtime` 可用于 Embassy task |
| `sekai-gpio` | 默认 | 核心 + 家族表 + 虚拟后端 + 工具，无 HAL 依赖 |
| `sekai-gpio` | `send` | 新：工具共享句柄改用 `Arc<critical_section::Mutex<RefCell<P>>>`，启用 `sekai/send` |
| `sekai-gpio` | `esp32` | 新：`esp-hal` 1.2.2 `Flex` 后端 + `send` |
| `sekai-gpio` | `stm32f1` | 新：`stm32f1` PAC + F103C8 引脚表 |
| `sekai-gpio` | `stm32f4` | 新：`stm32f4` PAC + F407VG 引脚表 |
| `sekai-start` | `x86` | 现有 host + 虚拟 GPIO 工具 |
| `sekai-start` | `esp32` | 真实 GPIO + WiFi/TCP + Runtime |
| `sekai-build` | `esp32` | 现有构建入口，无协议变化 |

### 3.3 目录结构（新增/修改）

```text
Cargo.toml                              # 工作区成员 + esp-radio profile
crate/sekai/Cargo.toml                  # send/esp32 feature
crate/sekai/src/sekai_tool.rs           # cfg 选择的 Box<dyn ToolHandler [+ Send]>
crate/sekai/src/sekai_task.rs           # cfg 选择的 Box<dyn TaskHandler [+ Send]>
crate/sekai/src/sekai_runtime.rs        # Running.handler 使用 BoxedTask
crate/sekai/tests/sekai_runtime.rs      # 在 send 下加 cfg(not(feature="send"))
crate/sekai/tests/send.rs               # 新增：Runtime: Send 编译断言 + 小烟测

crate/sekai-gpio/Cargo.toml             # 新 crate
crate/sekai-gpio/src/lib.rs
crate/sekai-gpio/src/family.rs          # ChipFamily/FamilyKind/FamilySpec/PinSpec/表
crate/sekai-gpio/src/pin.rs             # Port/PinId/解析/格式化
crate/sekai-gpio/src/config.rs          # Level/Direction/Pull/PinConfig/合并/校验
crate/sekai-gpio/src/port.rs            # GpioPort/PinDescription/GpioError/BackendKind
crate/sekai-gpio/src/text.rs            # 参数分词/key=value/解析辅助
crate/sekai-gpio/src/virtual_gpio.rs    # 虚拟后端
crate/sekai-gpio/src/tools.rs           # 4 个 ToolHandler + 注册 + help 文本
crate/sekai-gpio/src/esp32.rs           # esp-hal Flex 后端
crate/sekai-gpio/src/stm32/mod.rs       # Stm32Family/Stm32Gpio/通用状态机
crate/sekai-gpio/src/stm32/f1.rs        # F1 PAC 寄存器 + 规格
crate/sekai-gpio/src/stm32/f4.rs        # F4 PAC 寄存器 + 规格
crate/sekai-gpio/tests/*.rs             # 单元/集成测试

crate/sekai-start/Cargo.toml            # 新依赖/feature
crate/sekai-start/src/lib.rs            # async main、模块可见性
crate/sekai-start/src/config.rs         # 共享 no_std 配置解析
crate/sekai-start/src/clock.rs          # x86 + esp32 共用 SystemClock
crate/sekai-start/src/framing.rs        # x86 + esp32 共用分帧
crate/sekai-start/src/driver/x86.rs     # 注册虚拟 GPIO + 共享配置解析
crate/sekai-start/src/driver/esp32.rs   # GPIO + WiFi/TCP + Runtime 主循环
crate/sekai-start/src/driver/esp32/net.rs # WiFi 网络栈/TCP 辅助

crate/sekai-build/src/main.rs           # 固件环境变量用法/警告
docs/sekai-gpio.md                      # 新：抽象 + 工具 + AI 调用 + 移植
docs/sekai-esp32.md                     # 新：ESP32 构建/配网/限制
docs/sekai-protocol.md                  # 更新参考实现映射/清单
```

---

## 4. 核心抽象设计（`sekai-gpio`）

### 4.1 基础类型

```rust
// family.rs
pub enum FamilyKind { Esp32, Stm32 }

pub enum ChipFamily {
    Esp32,
    Stm32F1,
    Stm32F4,
}

impl ChipFamily {
    pub const fn kind(self) -> FamilyKind;
    pub const fn chip(self) -> &'static str; // "esp32" / "stm32f1" / "stm32f4"
    pub fn spec(self) -> &'static FamilySpec;
}

pub struct PinCaps {
    pub input: bool,
    pub output: bool,
    pub pull_up: bool,
    pub pull_down: bool,
    pub open_drain: bool,
}
impl PinCaps {
    pub const NONE: Self;
    pub const ALL: Self;
    pub const INPUT_ONLY: Self; // input=true，其余 false
}

pub struct PinFlags {
    pub reserved: bool,
    pub input_only: bool,
    pub strap: bool,
    pub uart0: bool,
    pub psram: bool,
    pub swd: bool,
    pub jtag: bool,
    pub osc: bool,
    pub boot: bool,
}
impl PinFlags {
    pub const NONE: Self;
    pub const RESERVED: Self;
    pub fn is_empty(self) -> bool;
    pub fn names(self) -> &'static [&'static str]; // 用于 gpio.info
}

pub struct PinSpec {
    pub id: PinId,
    pub slot: u16,          // 后端状态数组下标
    pub caps: PinCaps,
    pub flags: PinFlags,
    pub note: &'static str, // 可为空
}

pub struct FamilySpec {
    pub family: FamilyKind,
    pub chip: &'static str,
    pub pins: &'static [PinSpec],
    pub slots: usize,
    pub ports: &'static [Port], // ESP32 为空
}
```

### 4.2 引脚标识与命名

```rust
// pin.rs
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Port { A, B, C, D, E, F, G, H, I, J, K }

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum PinId {
    Io(u8),                         // ESP32：IO4
    Port { port: Port, pin: u8 },   // STM32：PA5
}

impl PinId {
    pub const fn io(number: u8) -> Self;
    pub const fn port(port: Port, pin: u8) -> Self;
    pub fn label(self) -> Label;    // Display：IO4 / PA5
    pub fn parse(text: &str) -> Result<Self, PinParseError>;
    pub fn slot(self, spec: &FamilySpec) -> Option<u16>;
}
```

解析规则（大小写不敏感）：

- ESP32：`IO<n>`、`GPIO<n>`、纯十进制 `<n>`；`n` 十进制 0..=39；拒绝前导零（`IO04` 非法），`IO0` 合法。
- STM32：`P<A-K><n>`；`n` 0..=15；拒绝 `A5`、`PA05`。
- `Display` 规范化输出：`IO4` / `PA5`。
- `PinParseError` 只描述语法；是否属于当前家族由 `FamilySpec::resolve` 判断。

`FamilySpec` 方法：

```rust
impl FamilySpec {
    pub fn resolve(&self, text: &str) -> Result<&'static PinSpec, GpioError>;
    pub fn by_id(&self, id: PinId) -> Option<&'static PinSpec>;
    pub fn summary(&self) -> FamilySummary; // labels/reserved/input_only/configured 由工具层补
}
```

### 4.3 配置与状态机

```rust
// config.rs
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level { Low, High }
impl Level { pub const fn from_bool(v: bool) -> Self; pub const fn as_bool(self) -> bool; }

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction { Input, Output }

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pull { None, Up, Down }

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PinConfig {
    pub direction: Direction,
    pub level: Level,
    pub pull: Pull,
    pub open_drain: bool,
}
impl Default for PinConfig {
    fn default() -> Self { Self { direction: Direction::Input, level: Level::Low, pull: Pull::None, open_drain: false } }
}
```

`gpio.config` 的请求是“部分更新”，由核心函数合并并校验：

```rust
pub struct ConfigRequest {
    pub direction: Option<Direction>,
    pub level: Option<Level>,
    pub pull: Option<Pull>,
    pub open_drain: Option<bool>,
}

pub fn resolve_config(
    spec: &'static PinSpec,
    current: Option<PinConfig>,
    request: &ConfigRequest,
) -> Result<PinConfig, GpioError>;
```

合并规则（必须写进文档/测试）：

1. 起始配置 = `current.unwrap_or_default()`。
2. 如果给了 `direction`：
   - 变成 `Input` 时：`pull` 保留/应用，`open_drain=false`，`level=Low`；
   - 变成 `Output` 时：`pull=None`，`open_drain` 应用，`level` 应用。
3. 没给 `direction` 时沿用当前方向；未配置默认 `Input`。
4. `level` 只允许在最终方向为 `Output` 时出现，否则 `InvalidArgument`。
5. `pull` 只允许在最终方向为 `Input` 时出现，否则 `InvalidArgument`。
6. `open_drain=true` 只允许在最终方向为 `Output` 时出现，否则 `InvalidArgument`。
7. 能力校验：
   - 方向 `Input` 需要 `caps.input`；
   - 方向 `Output` 需要 `caps.output`；
   - `pull=Up/Down` 需要 `caps.pull_up`/`caps.pull_down`；
   - `open_drain` 需要 `caps.open_drain`。
8. 保留引脚（`flags.reserved`）在任何操作前直接拒绝，`gpio.info` 详情除外。
9. 引脚不存在 → `UnknownPin`；保留 → `ReservedPin`；能力不足 → `Unsupported`。

### 4.4 `GpioPort` trait 与错误

```rust
// port.rs
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendKind { Hardware, Virtual }

pub struct PinDescription {
    pub spec: &'static PinSpec,
    pub config: Option<PinConfig>,
    pub level: Option<Level>, // 输入读 IDR；输出读锁存/ODR；未配置为 None
}

pub trait GpioPort {
    fn spec(&self) -> &'static FamilySpec;
    fn backend(&self) -> BackendKind { BackendKind::Hardware }
    fn configure(&mut self, pin: PinId, config: PinConfig) -> Result<(), GpioError>;
    fn read(&mut self, pin: PinId) -> Result<Level, GpioError>;
    fn write(&mut self, pin: PinId, level: Level) -> Result<(), GpioError>;
    fn describe(&mut self, pin: PinId) -> Result<PinDescription, GpioError>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GpioError {
    UnknownPin(String),
    ReservedPin(String),
    NotConfigured(String),
    WrongMode(String),
    Unsupported(String),
    InvalidArgument(String),
    Backend(String),
}

impl GpioError {
    pub fn code(&self) -> ErrorCode; // 全部 InvalidArgument，Backend -> Internal
    pub fn message(&self) -> &str;
}
impl From<GpioError> for CommandError { ... }
```

语义：

- `read`：只允许已配置为输入；输出模式返回 `WrongMode`。返回输入电平。
- `write`：只允许已配置为输出；输入/未配置返回 `WrongMode`/`NotConfigured`。
- `describe`：未配置也允许；返回能力/标志/配置/当前电平；输出电平取 ODR/锁存。
- `configure`：可以在输入/输出之间反复切换；由各后端实现。
- 所有 GPIO 错误码使用 `ErrorCode::InvalidArgument`；硬件内部异常使用 `ErrorCode::Internal`。不产生协议保留的 `Unsupported` 错误码。

### 4.5 家族表

#### ESP32 classic（35 个可用引脚）

| 引脚 | caps | flags/note |
| --- | --- | --- |
| IO0 | ALL | strap |
| IO1 | ALL | uart0（UART0 TX） |
| IO2 | ALL | strap |
| IO3 | ALL | uart0（UART0 RX） |
| IO4 | ALL | — |
| IO5 | ALL | strap |
| IO6..IO11 | NONE | reserved（SPI flash） |
| IO12 | ALL | strap |
| IO13..IO14 | ALL | — |
| IO15 | ALL | strap |
| IO16..IO17 | ALL | psram（WROVER 模块可能占用） |
| IO18..IO23 | ALL | — |
| IO25..IO27 | ALL | — |
| IO32..IO33 | ALL | — |
| IO34..IO39 | INPUT_ONLY | input_only（无内部上下拉、不可输出） |

- 不存在：IO20、IO24、IO28..IO31；`gpio.info` 对它们返回“引脚不存在”。
- `labels=IO0-IO23,IO25-IO27,IO32-IO39`；
- `reserved=IO6-IO11`；`input_only=IO34-IO39`。
- IO1/IO3 不保留，但 `gpio.info` 标记 `uart0`；文档警告如果启用 `esp-println` 会占用 UART0。
- 保留引脚 `configure/read/write` 一律 `InvalidArgument`；`gpio.info PIN` 仍可看到原因。

#### STM32F103C8（F1，35 个可用引脚）

- 端口 A/B 的全部 16 位 + PC13/PC14/PC15。
- 全部 `caps=ALL`（F1 GPIO 都支持输入/输出/上下拉/开漏）。
- `reserved`：PA13、PA14（SWD）、PA15、PB3、PB4（JTAG 默认占用）。
- 标志：PB2 `boot`（BOOT1）、PC13 `rtc`、PC14/PC15 `osc`（LSE）。
- `ports=A,B,C`；`labels=PA0-PA15,PB0-PB15,PC13-PC15`；
- `reserved=PA13-PA15,PB3,PB4`；`slots=48`。

#### STM32F407VG（F4，80 个可用引脚）

- 端口 A/B/C/D/E 的全部 16 位，共 80。
- 全部 `caps=ALL`。
- `reserved`：PA13、PA14（SWD）、PA15、PB3、PB4（JTAG 默认占用）。
- 标志：PC14/PC15 `osc`（LSE）。
- `ports=A,B,C,D,E`；`labels=PA0-PA15,...,PE0-PE15`；
- `reserved=PA13-PA15,PB3,PB4`；`slots=80`。

> 保留策略是安全默认值：AI 不应在不知道板级连接的情况下把调试口改成普通输出。文档会说明如何通过复制/修改 `PinSpec` 表解除保留。

### 4.6 虚拟后端

```rust
// virtual_gpio.rs
pub struct VirtualGpio {
    family: ChipFamily,
    spec: &'static FamilySpec,
    pins: Vec<Option<VirtualPinState>>, // 长度 = spec.slots
}

struct VirtualPinState {
    config: PinConfig,
    output_level: Level,
    external_level: Level, // 模拟外部输入
}

impl VirtualGpio {
    pub fn new(family: ChipFamily) -> Self;
    pub fn set_external_level(&mut self, pin: PinId, level: Level) -> Result<(), GpioError>;
}
```

- `backend()` 返回 `Virtual`。
- `configure` 走统一 `resolve_config` + 能力校验。
- `read` 输入返回 `external_level`；输出返回 `WrongMode`。
- `write` 输出更新 `output_level`；输入返回 `WrongMode`。
- `describe` 返回配置和当前电平。
- 未配置的输入/输出默认 `external_level=Low`。
- 未配置时 `gpio.info` 显示 `mode=unconfigured`。
- 保留/不存在/输入专用行为与硬件一致，保证 AI 在 host 上就能发现错误。

### 4.7 工具层

工具名称（4 个）：

```text
gpio.info
gpio.config
gpio.read
gpio.write
```

注册：

```rust
pub const GPIO_TOOL_NAMES: [&str; 4] =
    ["gpio.config", "gpio.info", "gpio.read", "gpio.write"];

pub fn register_tools<P>(registry: &mut ToolRegistry, port: P) -> Result<(), RegistryError>
where P: GpioPort + 'static; // send feature 下额外 + Send

pub(crate) fn register_impl<P: GpioPort + 'static>(
    registry: &mut ToolRegistry,
    shared: SharedPort<P>,
) -> Result<(), RegistryError>;
```

- 默认 `SharedPort<P> = Rc<RefCell<P>>`；
- `send` 下 `SharedPort<P> = Arc<critical_section::Mutex<RefCell<P>>>`；
- 所有工具通过 `with_port` 借出 `&mut P`：
  - 默认 `try_borrow_mut()`，失败返回 `Internal("GPIO 被占用")`；
  - send 下 `critical_section::with(|cs| ...)`，失败同样返回 `Internal`。
- 处理器必须实现 `ToolHandler`；send 下 `ToolRegistry::register` 的 `Send` 约束会自动要求 `P: Send`。
- 如果应用已注册同名 `gpio.*`，`register_tools` 返回 `Duplicate`；文档说明 `gpio.*` 由本 crate 约定占用。

### 4.8 `sekai` 的 `send` 特性

为让 ESP32 的 `#[embassy_executor::task]` 异步 main 能持有 `Runtime`，对 `sekai` 做最小、默认关闭的改动：

```rust
// sekai_tool.rs
#[cfg(not(feature = "send"))]
pub type BoxedTool = Box<dyn ToolHandler>;
#[cfg(feature = "send")]
pub type BoxedTool = Box<dyn ToolHandler + Send>;

pub struct ToolRegistry { tools: BTreeMap<String, BoxedTool> }

impl ToolRegistry {
    #[cfg(not(feature = "send"))]
    pub fn register<T: ToolHandler + 'static>(&mut self, name: &str, tool: T)
        -> Result<(), RegistryError>;
    #[cfg(feature = "send")]
    pub fn register<T: ToolHandler + Send + 'static>(&mut self, name: &str, tool: T)
        -> Result<(), RegistryError>;

    pub fn get_mut(&mut self, name: &str) -> Option<&mut BoxedTool>;
}
```

```rust
// sekai_task.rs
#[cfg(not(feature = "send"))]
pub type BoxedTask = Box<dyn TaskHandler>;
#[cfg(feature = "send")]
pub type BoxedTask = Box<dyn TaskHandler + Send>;

#[cfg(not(feature = "send"))]
pub type TaskFactory = Box<dyn Fn(&[u8]) -> Result<BoxedTask, CommandError>>;
#[cfg(feature = "send")]
pub type TaskFactory = Box<dyn Fn(&[u8]) -> Result<BoxedTask, CommandError> + Send>;

impl TaskRegistry {
    #[cfg(not(feature = "send"))]
    pub fn register<F>(...) where F: Fn(...) + 'static;
    #[cfg(feature = "send")]
    pub fn register<F>(...) where F: Fn(...) + Send + 'static;
}
```

`Running.handler` 改为 `BoxedTask`。当 `C: Send` 且工具/任务注册表为 Send 时，`Runtime<C>` 自动 `Send`；不需要不安全代码。

测试策略：

- 现有 `crate/sekai/tests/sekai_runtime.rs` 使用 `Rc<Cell>` 测试时钟；在文件顶部加 `#![cfg(not(feature = "send"))]`，保持默认测试覆盖。
- 新增 `crate/sekai/tests/send.rs`（`#![cfg(feature = "send")]`）：
  - 编译断言 `Runtime<SendClock>: Send`；
  - 注册一个 Send 工具、一个 Send 任务，跑最小 Runtime 烟测。
- `sekai` 的 `esp32` feature 设为 `["send"]`；`sekai-start` esp32 启用 `sekai/esp32`。
- `cargo test -p sekai --features send` 必须通过（只跑 send 测试文件 + 其他不依赖 Rc 的测试）。

### 4.9 `sekai-gpio` 的 `send` 共享句柄

```rust
#[cfg(not(feature = "send"))]
pub(crate) type SharedPort<P> = alloc::rc::Rc<core::cell::RefCell<P>>;
#[cfg(feature = "send")]
pub(crate) type SharedPort<P> =
    alloc::sync::Arc<critical_section::Mutex<core::cell::RefCell<P>>>;

fn with_port<P, R>(
    port: &SharedPort<P>,
    f: impl FnOnce(&mut P) -> R,
) -> Result<R, CommandError>;
```

- `send` 下 `SharedPort<P>` 是 Send/Sync（要求 `P: Send`）。
- `ToolHandler` 实现本身不需要条件编译；注册时由 `ToolRegistry::register` 的 `Send` 约束校验。
- `register_tools` 的两个 cfg 版本可重复 4 行，内部调用同一个 `register_impl`；`register_impl` 的 `P` 约束在 send 下加 `Send`。
- `sekai-gpio` 的 `send` feature 拉入 `critical-section`；测试时通过 dev-dependency 的 `critical-section/std` 提供 host 实现。

---

## 5. 工具契约（给 AI 的正式接口）

### 5.1 通用约定

- 参数是 UTF-8 文本；最大 **256 字节**（超过返回 `InvalidArgument`）。
- 结果也是 UTF-8 文本；最大 **1024 字节**（超过返回 `ResultTooLarge`）。
- 分词：ASCII 空白（空格/Tab）分隔；不支持引号/转义。
- 键/值大小写不敏感；引脚名规范化输出大写。
- 结果格式：
  - 多行用 `\n` 分隔，**末尾不带换行**；
  - `gpio.info` 摘要/详情使用 `key=value` 行；
  - `gpio.read`/`gpio.write`/`gpio.config` 使用单行空格分隔的 `key=value`；
  - 值为空时输出 `key=`（仅 `note` 可能为空，实际省略）。
- `gpio.info help` 是唯一非 `key=value` 的结果，固定英文自描述文本。

### 5.2 引脚名

| 家族 | 接受 | 规范化 |
| --- | --- | --- |
| ESP32 | `IO4`、`GPIO4`、`4` | `IO4` |
| STM32 | `PA5`（大小写不敏感） | `PA5` |

非法示例：`IO04`、`IO40`、`PA16`、`A5`、`P5`、`PJ0`（F4 表外）、`PC0`（F1 表外）。

### 5.3 `gpio.info`

语法：

```text
gpio.info
gpio.info help
gpio.info PIN
gpio.info port=A          # 仅 STM32
```

无参数摘要（ESP32 示例）：

```text
gpio=1
family=esp32
chip=esp32
backend=virtual
pins=35
labels=IO0-IO23,IO25-IO27,IO32-IO39
reserved=IO6-IO11
input_only=IO34-IO39
configured=0
```

无参数摘要（STM32F1 示例）：

```text
gpio=1
family=stm32
chip=stm32f1
backend=hardware
ports=A,B,C
pins=35
labels=PA0-PA15,PB0-PB15,PC13-PC15
reserved=PA13-PA15,PB3,PB4
configured=0
```

单引脚详情（未配置）：

```text
pin=IO4
caps=in,out,pu,pd,od
flags=none
mode=unconfigured
configured=0
```

单引脚详情（已配置输出）：

```text
pin=IO4
caps=in,out,pu,pd,od
flags=none
mode=output
level=1
pull=none
open_drain=0
configured=1
```

- `flags` 取值：`reserved,input_only,strap,uart0,psram,swd,jtag,osc,boot`，逗号分隔；无标志输出 `none`。
- `note` 非空时额外输出 `note=...`。
- `caps` 取值：`in,out,pu,pd,od`。
- `mode=unconfigured|input|output`。
- 输入电平：`level=0|1`；输出电平：ODR/锁存值。
- 保留引脚允许详情，不允许 configure/read/write。
- STM32 `port=A` 输出该端口的全部表内引脚，每行 `PAx caps=... flags=...`，首行 `port=A`；ESP32 返回 `InvalidArgument`。

`help` 固定文本（必须逐字写入测试）：

```text
gpio tools v1
info [help|PIN|port=A]
config PIN [in|out] [level=0|1] [pull=none|up|down] [open_drain=0|1]
read PIN
write PIN 0|1
pins: IO4 / PA5
read requires input mode; write requires output mode
```

### 5.4 `gpio.config`

语法：

```text
gpio.config PIN [in|out] [DIR=...] [LEVEL=...] [PULL=...] [OPEN_DRAIN=...]
```

- 裸方向：`in` / `input` / `out` / `output`，最多一个。
- 键：
  - `dir` 或 `direction`：`in|input|out|output`；
  - `level` 或 `init`：`0|1|low|high|on|off`；
  - `pull`：`none|up|down`；
  - `open_drain` 或 `od`：`0|1|true|false|on|off`。
- 同一个键/方向重复出现 → `InvalidArgument`。
- 未知键/未知 token → `InvalidArgument`。
- 合并/校验语义见 §4.3。
- 成功结果单行：

```text
pin=PA5 mode=output level=1 pull=none open_drain=0
```

- 输入结果：

```text
pin=PA5 mode=input level=0 pull=up open_drain=0
```

- `level` 对输入、`pull` 对输出、`open_drain=1` 对输入 → `InvalidArgument`。
- 未配置且没有 `dir` → 默认输入（安全默认）。

### 5.5 `gpio.read`

语法：

```text
gpio.read PIN
```

- 仅输入模式；返回：

```text
pin=IO4 level=1
```

- 输出/未配置 → `InvalidArgument`（消息说明“需要输入模式；请先 gpio.config PIN in”）。

### 5.6 `gpio.write`

语法：

```text
gpio.write PIN 0|1|low|high|on|off
```

- 仅输出模式；返回：

```text
pin=IO4 level=1
```

- 输入/未配置 → `InvalidArgument`（消息说明需要输出模式）。
- 引脚保留/不存在/能力不足 → `InvalidArgument`。

### 5.7 `gpio.info help` 的 AI 使用顺序

1. 从 retained Hello 读取 `tools`，确认存在 `gpio.*`。
2. 调 `gpio.info help` 获取语法。
3. 调 `gpio.info` 摘要，选择未保留、非输入专用的引脚。
4. 调 `gpio.info PIN` 确认 caps/flags。
5. 调 `gpio.config PIN out level=0`。
6. 调 `gpio.write PIN 1`。
7. 需要读取时先 `gpio.config PIN in pull=up`，再 `gpio.read PIN`。

### 5.8 示例 Stdin 帧（`gpio.write IO4 1`）

- ToolBody：`name="gpio.write"`（10 字节），args=`"IO4 1"`（5 字节）；payload = 2 + 10 + 5 = 17 字节。
- Stdin 总长 30 + 17 = 47 字节。
- `cmd_id=1`、`timestamp=1700000000000`、`delay_ms=0`、`ddl_ms=500`。
- 单行十六进制（实现时必须用测试生成并校验，文档引用该测试）：

```text
01 02 00 00 00 00 00 00 00 01 00 00 01 8B CF E5 68 00
00 00 00 00 00 00 01 F4 00 00 00 11
00 0A 67 70 69 6F 2E 77 72 69 74 65 49 4F 34 20 31
```

- 成功 `Completed` 载荷为文本 `pin=IO4 level=1`（15 字节，无换行）。

### 5.9 错误码映射

| 场景 | 错误码 | 消息示例 |
| --- | --- | --- |
| 参数语法/键值非法 | `InvalidArgument` | `参数 foo 非法` |
| 引脚不存在 | `InvalidArgument` | `引脚 IO40 不存在` |
| 保留引脚 | `InvalidArgument` | `引脚 IO6 已保留（SPI flash）` |
| 引脚能力不足 | `InvalidArgument` | `引脚 IO34 仅支持输入` |
| 未配置/模式不匹配 | `InvalidArgument` | `引脚 IO4 处于输出模式，无法读取` |
| 共享句柄借用失败 | `Internal` | `GPIO 被占用` |
| 后端内部错误 | `Internal` | `GPIO 后端错误：...` |
| 结果超 1024 字节 | `ResultTooLarge` | `GPIO 结果过大` |

---

## 6. ESP32 GPIO 后端

### 6.1 实现方式

- 依赖 `esp-hal = { version = "1.2.2", features = ["esp32", "unstable"] }`。
- 使用 `esp_hal::gpio::Flex<'static>` 统一输入/输出/上下拉/开漏；`Flex` 在 `unstable` feature 下可用。
- 延迟创建：`Esp32Gpio` 只保存 `[Option<Flex<'static>>; 40]` 和 `[Option<PinConfig>; 40]`；首次 configure 时：
  ```rust
  let any = unsafe { esp_hal::gpio::AnyPin::steal(number) };
  let flex = Flex::new(any);
  ```
- `AnyPin::steal` 是唯一 unsafe 点；安全不变量：
  - 驱动在 `esp_hal::init` 之后调用 `Esp32Gpio::new()`；
  - 应用不再直接使用 `Peripherals.GPIOx`；
  - 每个 pin 只 steal 一次（`slots[n]` 为 `Some` 后不再 steal）。
- 所有 pin 表外的编号（20/24/28..31/40+）在 steal 前拒绝。

### 6.2 配置映射

- 输入：
  - `flex.set_output_enable(false)`;
  - `flex.set_input_enable(true)`;
  - `flex.apply_input_config(&InputConfig::default().with_pull(map_pull(pull)))`。
- 输出：
  - `flex.set_level(map_level(level))`;
  - `flex.apply_output_config(&OutputConfig::default().with_drive_mode(if open_drain { DriveMode::OpenDrain } else { DriveMode::PushPull }))`;
  - `flex.set_output_enable(true)`;
  - `flex.set_input_enable(false)`。
- 重新配置时重复上述步骤；`Flex` 保留在 slot 中。
- `read` 输入：`flex.level()`；`write` 输出：`flex.set_level()`。
- `describe` 输出：`flex.output_level()`；输入：`flex.level()`。
- 保留/能力校验在核心 `resolve_config` 完成，后端不再重复。

### 6.3 初始化与工具注册

- `Esp32Gpio::new()` 不接收 `Peripherals`；文档要求它在 `esp_hal::init` 之后调用。
- 驱动：
  ```rust
  let gpio = sekai_gpio::esp32::Esp32Gpio::new();
  let mut tools = ToolRegistry::new();
  sekai_gpio::register_tools(&mut tools, gpio).expect("注册 GPIO 工具");
  ```
- 编译验证：`cargo run -p sekai-build -- build`。
- 由于 `esp32` feature 启用 `send`，`register_tools` 使用 `Arc` 句柄；`Esp32Gpio` 必须实现 `Send`（`Flex`/`AnyPin` 自动 Send）。如果编译器报告某字段非 Send，改用 `Arc<Mutex<Box<Esp32Gpio>>>` 并保持句柄可 Send；不得使用 `unsafe impl Send`。

### 6.4 ESP32 规格测试

- `sekai-gpio` 单元测试直接验证 `ChipFamily::Esp32.spec()`：
  - 35 个引脚；
  - IO34..39 只能输入；
  - IO6..11 保留；
  - IO20/24/28..31 不存在；
  - IO0/2/5/12/15 带 `strap`；
  - IO1/3 带 `uart0`。
- 这些测试不需要 esp-hal，host 默认 feature 即可运行。

---

## 7. STM32F1/F4 后端（PAC 寄存器）

### 7.1 为什么直接用 PAC

- F1 的 HAL 动态引脚模式需要 CRL/CRH 寄存器，`AnyPin`/`ErasedPin` 没有统一的运行时 `into_mode`；F4 的 HAL 虽然有 `AnyPin`，但两套 HAL API 不一致。
- GPIO 寄存器模型简单且稳定；直接使用 `stm32f1`/`stm32f4` PAC 可以：
  - 一套 `Stm32Gpio<F>` 状态机覆盖 F1/F4；
  - 完整支持运行时输入/输出/上下拉/开漏切换；
  - 只依赖 PAC，不依赖 HAL 的类型状态和特性组合。
- unsafe 只出现在 `stm32/f1.rs`、`stm32/f4.rs` 的寄存器读写；核心 `sekai-gpio` 其余模块保持安全代码。

### 7.2 依赖与 feature

```toml
stm32f1 = { version = "0.16", features = ["stm32f103", "critical-section"], optional = true }
stm32f4 = { version = "0.16", features = ["stm32f407", "critical-section"], optional = true }
```

```toml
[features]
stm32f1 = ["dep:stm32f1", "stm32f1/stm32f103", "stm32f1/critical-section"]
stm32f4 = ["dep:stm32f4", "stm32f4/stm32f407", "stm32f4/critical-section"]
```

目标：

- F1：`thumbv7m-none-eabi`（需安装）。
- F4：`thumbv7em-none-eabi`（已安装；PAC 不需要硬浮点 ABI）。

### 7.3 通用家族 trait

```rust
// stm32/mod.rs
pub trait Stm32Family {
    const SPEC: &'static FamilySpec;
    type Ports;
    type Rcc;

    fn enable_clocks(rcc: &mut Self::Rcc, ports: &mut Self::Ports);
    fn configure(ports: &mut Self::Ports, spec: &'static PinSpec, config: PinConfig);
    fn read(ports: &Self::Ports, spec: &'static PinSpec) -> Level;
    fn write(ports: &mut Self::Ports, spec: &'static PinSpec, level: Level);
}

pub struct Stm32Gpio<F: Stm32Family> {
    ports: F::Ports,
    configs: Vec<Option<PinConfig>>, // 长度 = F::SPEC.slots
}

impl<F: Stm32Family> Stm32Gpio<F> {
    pub fn new(ports: F::Ports, rcc: &mut F::Rcc) -> Self;
    pub fn ports(&self) -> &F::Ports;
}
```

- `new` 先调用 `F::enable_clocks`，再初始化 `configs` 为 `None`。
- `configure`/`read`/`write`/`describe` 先通过统一 `resolve_config`/能力校验，再调用家族寄存器函数；校验失败不触碰硬件。
- 状态机只保存配置，不保存 HAL 类型；寄存器就是硬件状态。

### 7.4 F1 寄存器映射

- 端口 A/B/C；状态 slot：A=0..15，B=16..31，C=32..47；表内只用 C13..C15。
- 时钟：`RCC.apb2enr`，IOPAEN=bit2、IOPBEN=bit3、IOPCEN=bit4；读改写。
- CRL（pin 0..7）/CRH（pin 8..15），每 pin 4 bit：`CNF[1:0] MODE[1:0]`。
  - 输入浮空：`CNF=01, MODE=00`；
  - 输入上/下拉：`CNF=10, MODE=00`；ODR 对应位 1=上拉、0=下拉；
  - 输出推挽：`CNF=00, MODE=10`（2MHz）；
  - 输出开漏：`CNF=01, MODE=10`。
- 数据：
  - 写高：`BSRR = 1 << pin`；写低：`BSRR = 1 << (pin + 16)`；
  - 读输入：`IDR`；
  - 读输出锁存：`ODR`。
- `configure` 顺序：先写 CRL/CRH，再写 ODR 初始电平/上下拉；输出模式先设置 ODR 再启用输出，避免毛刺。
- 所有寄存器读改写用 `modify(|r, w| unsafe { w.bits((r.bits() & !mask) | value) })`；`BSRR` 用 `write(|w| unsafe { w.bits(bits) })`。

### 7.5 F4 寄存器映射

- 端口 A..E；slot：port*16 + pin。
- 时钟：`RCC.ahb1enr`，GPIOAEN=bit0 .. GPIOEEN=bit4。
- `MODER` 每 pin 2 bit：`00` 输入、`01` 输出。
- `OTYPER` 每 pin 1 bit：`0` 推挽、`1` 开漏。
- `PUPDR` 每 pin 2 bit：`00` 无、`01` 上拉、`10` 下拉。
- `OSPEEDR` 每 pin 2 bit：输出设置为 `01`（medium）。
- 数据：`BSRR` 高/低；`IDR` 输入；`ODR` 输出锁存。
- `configure` 顺序：先关闭输出（MODER=输入）→ 写 OTYPER/PUPDR/OSPEEDR → 写 ODR 初始电平 → MODER=输出；输入则 MODER=输入后写 PUPDR。
- 保留引脚（PA13/14/PA15/PB3/PB4）不会被配置。

### 7.6 构造与板级使用

F1：

```rust
use stm32f1::stm32f103 as pac;

let dp = pac::Peripherals::take().expect("外设只能初始化一次");
let mut gpio = sekai_gpio::stm32::Stm32Gpio::<sekai_gpio::stm32::f1::F1>::new(
    sekai_gpio::stm32::f1::F1Ports {
        gpioa: dp.GPIOA,
        gpiob: dp.GPIOB,
        gpioc: dp.GPIOC,
    },
    &mut dp.RCC,
);
gpio.configure(PinId::port(Port::C, 13), PinConfig { direction: Direction::Output, level: Level::High, ..Default::default() })?;
```

F4：

```rust
use stm32f4::stm32f407 as pac;

let dp = pac::Peripherals::take().expect("外设只能初始化一次");
let mut gpio = sekai_gpio::stm32::Stm32Gpio::<sekai_gpio::stm32::f4::F4>::new(
    sekai_gpio::stm32::f4::F4Ports {
        gpioa: dp.GPIOA, gpiob: dp.GPIOB, gpioc: dp.GPIOC,
        gpiod: dp.GPIOD, gpioe: dp.GPIOE,
    },
    &mut dp.RCC,
);
gpio.configure(PinId::port(Port::A, 5), PinConfig { direction: Direction::Output, level: Level::Low, ..Default::default() })?;
```

- 板级工程保留其他外设（USART 等）的所有权；本后端只接管 GPIO 端口和 RCC 的 GPIO 时钟位。
- 若板级已启用 GPIO 时钟，`enable_clocks` 的置位是幂等的。

### 7.7 STM32 测试

- host 单元测试无法访问真实寄存器；为 `Stm32Gpio` 增加一个 `#[cfg(test)]` 的 mock family：
  - `Ports` 含 `Rc<RefCell<MockRegs>>`；
  - `configure/read/write` 更新 mock 位；
  - 测试输入/输出切换、上下拉、开漏、保留/能力错误、`describe`。
- F1/F4 各自测试规格表（引脚数、保留、端口、slot）。
- 真实寄存器代码只做交叉编译检查；文档注明需要硬件验证。

---

## 8. ESP32 最小 WiFi/TCP 链路

### 8.1 依赖与 feature（`sekai-start`）

```toml
esp-hal = { version = "1.2.2", features = ["esp32", "unstable"], optional = true }
esp-rtos = { version = "0.4.0", features = ["esp-alloc", "esp-radio", "esp32", "embassy"], optional = true }
esp-radio = { version = "1.0.0-beta.1", features = ["wifi", "esp-alloc", "esp32", "unstable"], optional = true }
esp-alloc = { version = "0.11.0", features = ["esp32"], optional = true }
esp-bootloader-esp-idf = { version = "0.6.0", features = ["esp32"], optional = true }
critical-section = { version = "1.2.0", optional = true }
embedded-io = { version = "0.7.1", optional = true }

embassy-executor = { version = "0.10", optional = true }
embassy-net = { version = "0.9", features = ["tcp", "dhcpv4", "proto-ipv4", "medium-ethernet"], optional = true }
embassy-time = { version = "0.5", optional = true }
static_cell = { version = "2", optional = true }
esp-println = { version = "0.17", features = ["esp32"], optional = true }
```

- 移除 `bleps` 和 `esp-radio/ble`；本轮只做 WiFi，避免 BLE/coex 复杂度。BLE 后续可加 `ble` + `coex`。
- 根 `Cargo.toml` 增加：
  ```toml
  [profile.dev.package.esp-radio]
  opt-level = 3
  ```
  （esp-radio 文档要求 WiFi blob 在 debug 下至少 opt-level 2/3。）
- `sekai-start` 的 `esp32` feature 拉入上述依赖，并启用 `sekai/esp32`、`sekai-gpio/esp32`。

### 8.2 启动顺序

```rust
#[esp_hal::main]
async fn main(spawner: embassy_executor::Spawner) {
    esp_alloc::heap_allocator!(#[ram(reclaimed)] size: 96 * 1024);
    esp_alloc::heap_allocator!(size: 48 * 1024);

    let config = esp_hal::Config::default().with_cpu_clock(esp_hal::clock::CpuClock::max());
    let peripherals = esp_hal::init(config);

    let timg0 = esp_hal::timer::timg::TimerGroup::new(peripherals.TIMG0);
    esp_rtos::start(timg0.timer0, peripherals.FROM_CPU_INTR0);

    let mut wifi = esp_radio::wifi::WifiController::new(peripherals.WIFI, Default::default())
        .expect("WiFi 初始化失败");
    let sta = esp_radio::wifi::Interface::station();
    let sta_config = esp_radio::wifi::Config::Station(
        esp_radio::wifi::sta::StationConfig::default()
            .with_ssid(WIFI_SSID.try_into().expect("SSID 非法"))
            .with_authentication(wifi_auth()),
    );
    wifi.set_config(&sta_config).expect("WiFi 配置失败");
    wifi.connect_async().await.expect("WiFi 连接失败");

    let resources = Box::leak(Box::new(embassy_net::StackResources::<4>::new()));
    let (stack, runner) = embassy_net::new(
        sta,
        embassy_net::Config::dhcpv4(Default::default()),
        resources,
        random_seed(),
    );
    spawner.spawn(net_task(runner)).unwrap();
    embassy_time::with_timeout(
        embassy_time::Duration::from_secs(15),
        stack.wait_config_up(),
    ).await.expect("DHCP 超时");

    let mut tools = ToolRegistry::new();
    sekai_gpio::register_tools(&mut tools, sekai_gpio::esp32::Esp32Gpio::new())
        .expect("注册 GPIO 工具");
    let runtime = Runtime::new(
        runtime_config(),
        SystemClock,
        parse_devices_env(),
        TaskRegistry::new(),
        tools,
    ).expect("运行时初始化失败");

    run_mqtt(runtime, stack, wifi).await
}
```

- `#[esp_hal::main]` 对 async main 生成 `esp_rtos::embassy::Executor`；`sekai-start` 必须直接依赖 `embassy-executor`（生成代码使用 `::embassy_executor`）。
- `net_task`：
  ```rust
  #[embassy_executor::task]
  async fn net_task(mut runner: embassy_net::Runner<'static, esp_radio::wifi::Interface>) {
      runner.run().await
  }
  ```
- `esp_rtos::start` 必须在 `WifiController::new` 之前。
- `esp_alloc` 堆大小 96+48KB；如果硬件上 WiFi 初始化失败，优先增大 reclaimed 堆并记录。

### 8.3 编译期环境变量（`option_env!`）

| 变量 | 必填 | 说明 |
| --- | --- | --- |
| `SEKAI_WIFI_SSID` | 是 | 2.4GHz SSID |
| `SEKAI_WIFI_PASSWORD` | 否 | 空 = Open；非空 = WPA2 Personal |
| `SEKAI_MQTT_ADDR` | 是 | **IPv4:port**，例如 `192.168.1.10:1883`；本轮不做 DNS |
| `SEKAI_MQTT_USERNAME` | 否 | MQTT 用户名 |
| `SEKAI_MQTT_PASSWORD` | 否 | MQTT 密码 |
| `SEKAI_DEVICE_ID` | 否 | 默认 `sekai-esp32` |
| `SEKAI_TOPIC_ROOT` | 否 | 默认 `sekai/v1` |
| `SEKAI_DEVICES` | 否 | Jump 目标，`name=IPv4:port,...` |

- 常量形式：
  ```rust
  const WIFI_SSID: &str = match option_env!("SEKAI_WIFI_SSID") { Some(v) => v, None => "" };
  ```
- 启动时校验：SSID 非空、`SEKAI_MQTT_ADDR` 可解析为 IPv4:port；失败用 `esp_println::println!` 打印原因并进入重试/空转，不能静默 panic（panic handler 只 loop）。
- `sekai-build` 的 usage 增加这些变量；构建前对缺失的必填变量打印 warning（不失败，方便 CI 只编译）。
- `SEKAI_DEVICES` 用共享 `config::parse_devices_str`；`SEKAI_MQTT_ADDR` 用 `config::parse_ipv4_endpoint`。

### 8.4 TCP/MQTT 主循环

伪代码（必须实现为独立 `async fn`，便于审阅）：

```rust
async fn run_mqtt(
    mut runtime: Runtime<SystemClock>,
    stack: Stack<'static>,
    mut wifi: WifiController<'static>,
) -> ! {
    let mut endpoint = parse_mqtt_endpoint();
    let mut backoff = Backoff::new();

    loop {
        if !wifi.is_connected() {
            let _ = with_timeout(Duration::from_secs(20), wifi.connect_async()).await;
        }
        let _ = with_timeout(Duration::from_secs(15), stack.wait_config_up()).await;

        let mut rx = Box::new([0u8; 4096]);
        let mut tx = Box::new([0u8; 4096]);
        let mut socket = TcpSocket::new(stack, &mut rx[..], &mut tx[..]);

        match with_timeout(Duration::from_secs(5), socket.connect(endpoint)).await {
            Ok(Ok(())) => {
                esp_println::println!("[sekai] MQTT TCP connected {}", endpoint);
                if let Err(e) = runtime.link_up(endpoint) {
                    esp_println::println!("[sekai] link_up failed: {e}");
                }
                let outcome = mqtt_session(&mut runtime, &mut socket).await;
                socket.close();
                match outcome {
                    SessionEnd::Lost => runtime.link_lost(),
                    SessionEnd::Closed => runtime.link_closed(),
                    SessionEnd::Switch(action) => { /* 下一轮使用 action.endpoint */ }
                }
            }
            _ => {
                runtime.link_failed(endpoint);
                esp_println::println!("[sekai] TCP connect failed {}", endpoint);
            }
        }

        if let Some(action) = runtime.take_link_action() {
            endpoint = match action { LinkAction::Switch { endpoint, .. } => endpoint };
        }
        Timer::after(Duration::from_millis(backoff.next())).await;
    }
}
```

`mqtt_session` 内：

- `let mut decoder = StreamDecoder::new(runtime.max_incoming_packet_size().unwrap_or(16 * 1024));`
- 循环：
  1. 发送 pending/outgoing：
     - 维护 `pending: Option<Vec<u8>>`；若 `pending` 为空，`runtime.next_outgoing()` 取包并 `encode()`；
     - `with_timeout(Duration::from_secs(5), write_all(&mut socket, &bytes)).await`；
     - 失败 → `SessionEnd::Lost`；
  2. 读取：
     - `with_timeout(Duration::from_millis(20), socket.read(&mut buf)).await`；
     - `Ok(Ok(0))` → `Lost`；
     - `Ok(Ok(n))` → `decoder.push` + 循环 `decoder.next()` → `runtime.feed(packet)`；
     - `Ok(Err(e))` → `Lost`；
     - `Err(Timeout)` → 继续；
  3. `runtime.poll()`；失败 → `Lost`；
  4. 取 `LinkAction`：如果出现，先 flush 剩余 outgoing，`socket.close()`，返回 `Switch(action)`；
  5. 按 `runtime.next_deadline()` 睡眠，最大 20ms、最小 1ms。
- `write_all` 自己循环 `socket.write`（不依赖 embedded-io-async trait，避免版本不一致）。
- `socket.close()` 后 `runtime.link_closed()`；重连带 `runtime.link_up`。
- `Jump` 的目标端点只改 TCP 目标；WiFi 不断开。
- `link_failed` 后运行时可能给出 `Fallback` action；按 `take_link_action` 处理。
- MQTT Keep Alive 由 runtime/client 处理；20ms poll 足够。

### 8.5 时钟与配置

- `SystemClock` 从 x86 专属改为 x86/esp32 共用：
  - `monotonic_ms` → `crate::time::hardware_time_ms()`；
  - `unix_ms` → `crate::time::unix_time_ms()`（ESP32 未校准为 `None`）；
  - `calibrate_unix_ms` → `crate::time::calibrate_unix_ms`。
- `RuntimeConfig`：
  - `device_id` 从 `SEKAI_DEVICE_ID`；
  - `firmware` 常量 `"sekai-esp32/0.1.0"`；
  - `boot_id` 用 `esp_hal::rng::Rng::new().random()` 两次拼成非零 u64；
  - `mqtt.username/password`、`topic_root`、`will_offline=true`；
  - `devices` 来自 `SEKAI_DEVICES`。
- `Runtime` 在 `send` 下应自动 `Send`；若编译报某个字段非 Send，逐字段定位，不要用 `unsafe impl` 绕过。

### 8.6 诊断与限制

- `esp-println` 0.17 使用 UART0；打印：
  - 启动、WiFi 连接中/成功、DHCP 地址；
  - TCP 连接/失败、MQTT 连接；
  - 配置错误/重连。
- UART0 占用 IO1/IO3；`gpio.info` 标记 `uart0`；文档警告 AI 不要在这些引脚上做输出。
- 限制：
  - 仅 Station + DHCP；无静态 IP；
  - 仅 IPv4 字面量 broker；无 DNS；
  - 无 TLS；broker 必须在可信 LAN；
  - 无 NVS；WiFi/MQTT 配置编译期固定；
  - 无 BLE/AP/企业 WiFi；
  - 单 TCP 连接；断线指数退避重连。
- 这些限制写入 `docs/sekai-esp32.md`。

### 8.7 构建与烧录

```bash
SEKAI_WIFI_SSID=my-ssid \
SEKAI_WIFI_PASSWORD=my-pass \
SEKAI_MQTT_ADDR=192.168.1.10:1883 \
SEKAI_DEVICE_ID=esp32-lab \
cargo run -p sekai-build -- build --release

cargo run -p sekai-build -- flash --release --monitor -- --baud 115200
```

- `sekai-build` 的 `build` 仍使用 `esp` 工具链、`xtensa-esp32-none-elf`、`-Zbuild-std=core,alloc`、`-Tlinkall.x`。
- 新增依赖必须能被 `-Zbuild-std` 构建；若某个 crate 需要 `std`，必须替换或关闭对应 feature。

---

## 9. Host 集成（`sekai-start` x86）

### 9.1 共享模块重构

- 新增 `crate/sekai-start/src/config.rs`（`no_std` + `alloc`）：
  - `parse_devices_str(&str) -> Result<DeviceList, String>`；
  - `parse_ipv4_endpoint(&str) -> Result<Endpoint, String>`；
  - `parse_endpoint(&str) -> Result<Endpoint, String>`（host 可解析主机名；ESP32 只用 IPv4 版本）。
- 把 `driver/x86.rs` 里的 `parse_devices` 移入 `config.rs`，x86 测试迁到 `config.rs` 的 `#[cfg(test)]`。
- `framing.rs` 从 `#[cfg(feature = "x86")]` 改为 `#[cfg(any(feature = "x86", feature = "esp32"))]`；补 `use alloc::vec::Vec;`。
- `clock.rs` 从 `#[cfg(feature = "x86")]` 改为 `#[cfg(any(feature = "x86", feature = "esp32"))]`；文档改成“设备时钟”。
- `lib.rs` 增加 `extern crate alloc;`（`no_std` esp32 需要；x86 也合法）。
- `link.rs` 保持 x86-only。

### 9.2 虚拟 GPIO 工具注册

`driver/x86.rs`：

```rust
const ENV_GPIO: &str = "SEKAI_GPIO";             // on|off
const ENV_GPIO_FAMILY: &str = "SEKAI_GPIO_FAMILY"; // esp32|stm32f1|stm32f4
const ENV_GPIO_INPUTS: &str = "SEKAI_GPIO_INPUTS"; // IO4=1,PA5=0
```

- 默认 `SEKAI_GPIO=on`、`SEKAI_GPIO_FAMILY=esp32`。
- 解析失败打印 warning 并使用默认值；`SEKAI_GPIO=off` 时完全不注册。
- 构建虚拟后端：
  ```rust
  let mut gpio = VirtualGpio::new(family);
  for (pin, level) in parsed_inputs { gpio.set_external_level(pin, level)?; }
  sekai_gpio::register_tools(&mut tools, gpio).expect("注册 GPIO 工具");
  ```
- `demo_tools()` 先注册 `echo`，再按需注册 GPIO；工具总数 6+1+4=11。
- `print_usage()` 增加三个环境变量、`gpio.*` 工具列表、虚拟后端说明。
- 保留现有 `SEKAI_MQTT_ADDR`、`SEKAI_DEVICE_ID`、用户名/密码/设备列表行为。

### 9.3 Host 验收流程

```bash
# 终端 1：broker（示例）
mosquitto -p 1883

# 终端 2：host 虚拟设备（ESP32 引脚表）
SEKAI_MQTT_ADDR=127.0.0.1:1883 \
SEKAI_DEVICE_ID=host-gpio \
SEKAI_GPIO_FAMILY=esp32 \
cargo run -p sekai-start --bin sekai-host

# 终端 3：用任意 MQTT 5 客户端订阅 out/state，按文档构造 ToolCall
# gpio.info help -> gpio.info -> gpio.config IO4 out level=0 -> gpio.write IO4 1
```

- `SEKAI_GPIO_FAMILY=stm32f1` 时使用 `PA5`/`PC13`；`stm32f4` 时使用 `PA5`/`PE0`。
- 虚拟输入可用 `SEKAI_GPIO_INPUTS=IO4=1` 初始化。
- 没有 broker 时，运行时集成测试仍覆盖编解码/工具路径。

---

## 10. 文档

### 10.1 新增 `docs/sekai-gpio.md`

必须包含：

1. 目标、范围、非目标；
2. 架构图（`sekai` ← `sekai-gpio` ← `sekai-start`）；
3. 引脚命名（`IO4`/`PA5`）、家族表、保留引脚；
4. 配置状态机与 `gpio.config` 语义；
5. 四个工具的完整语法、结果格式、错误码、`gpio.info help` 文本；
6. AI 调用流程（Hello → help → info → config → write/read）；
7. 完整 Stdin 帧十六进制示例（`gpio.write IO4 1`，由测试生成）；
8. host 虚拟后端环境变量与示例；
9. ESP32 后端使用/安全注意；
10. STM32F1/F4 板级使用示例与保留引脚；
11. 新家族移植指南：
    - 实现 `FamilySpec` 表；
    - 实现 `GpioPort` 或在 `stm32` 模块实现 `Stm32Family`；
    - 注册工具；
    - 补充测试；
12. 安全与限制（broker ACL、物理执行、保留/strapping/UART0 引脚）。

### 10.2 新增 `docs/sekai-esp32.md`

必须包含：

1. 工具链/目标安装（`espup`、`esp`、`xtensa-esp32-none-elf`）；
2. `sekai-build` 命令；
3. 编译期环境变量表；
4. WiFi/TCP/MQTT 启动顺序与最小链路限制；
5. `esp-println`/UART0 引脚冲突；
6. 重连/退避行为；
7. 故障排查（WiFi、DHCP、TCP、MQTT、GPIO 保留）；
8. 硬件验收清单。

### 10.3 更新 `docs/sekai-protocol.md`

- §12.1 文件职责表增加 `crate/sekai-gpio/`；
- 新增 §12.4 “GPIO 用户工具（参考实现）”：工具名、文本参数、结果、错误码、与 `gpio.info help` 的关系，链接 `sekai-gpio.md`；
- §13.1 设备端检查清单增加：
  - GPIO 工具参数/结果有测试；
  - 保留引脚/输入专用引脚被拒绝；
  - Hello 中的工具数与运行时上限一致；
- 不改变协议帧格式/错误码表。

---

## 11. 文件级任务与实施顺序

建议按以下阶段执行；每阶段结束运行对应验证，再进入下一阶段。以下命令均由未来执行者运行。

### Phase 0：基线

- 运行并记录当前基线：
  ```bash
  cargo test --workspace
  cargo run -p sekai-build -- build
  ```
- 安装 F1 目标：
  ```bash
  rustup target add thumbv7m-none-eabi
  ```
- 写作用域：无代码改动。
- 验收：基线通过；记录 ESP32 现有构建是否成功。

### Phase 1：`sekai` send 特性

- 文件：`crate/sekai/Cargo.toml`、`src/sekai_tool.rs`、`src/sekai_task.rs`、`src/sekai_runtime.rs`、`tests/sekai_runtime.rs`、`tests/send.rs`。
- 内容：§4.8。
- 验收：
  ```bash
  cargo test -p sekai
  cargo test -p sekai --features send
  ```
- 注意：现有 `Rc` 测试文件在 send 下 gate；新增 send 编译断言。

### Phase 2：`sekai-gpio` 核心 + 家族表 + 虚拟后端

- 新建 `crate/sekai-gpio`，实现：
  - `lib.rs`、`pin.rs`、`family.rs`、`config.rs`、`port.rs`、`virtual_gpio.rs`；
  - ESP32/F1/F4 规格表与 §4.5 的标志；
  - 统一 `resolve_config`/能力校验。
- 单元测试：解析/格式化、能力校验、虚拟后端、规格表。
- 验收：
  ```bash
  cargo test -p sekai-gpio
  ```

### Phase 3：工具层

- 文件：`src/text.rs`、`src/tools.rs`。
- 实现 §5 的四个工具、帮助文本、结果格式、错误映射、注册函数。
- 单元测试：
  - 每种工具成功/失败；
  - 语法边界（大小写、重复键、超长、空 args、保留/输入专用）；
  - `gpio.info` 摘要/详情/port/help；
  - 结果长度。
- 验收：
  ```bash
  cargo test -p sekai-gpio
  ```

### Phase 4：运行时集成测试 + host 虚拟后端

- 文件：`crate/sekai-gpio/tests/runtime_tools.rs`；`crate/sekai-start/src/config.rs`、`clock.rs`、`framing.rs`、`lib.rs`、`driver/x86.rs`、`Cargo.toml`。
- 测试运行时：
  - 构造 `Runtime` + `VirtualGpio` 工具；
  - 合成 MQTT 报文，断言 Hello 工具列表、四个工具的结果/错误；
  - 工具总数 11。
- host 集成：`sekai-host` 注册虚拟工具、环境变量、usage。
- 验收：
  ```bash
  cargo test --workspace
  cargo run -p sekai-start --bin sekai-host   # 只验证启动/usage（无 broker 时按现有行为）
  ```

### Phase 5：STM32 F1/F4 后端

- 文件：`src/stm32/mod.rs`、`f1.rs`、`f4.rs`；`Cargo.toml` features。
- 实现 §7 的 PAC 寄存器与 mock family 测试。
- 验收：
  ```bash
  cargo test -p sekai-gpio
  cargo check -p sekai-gpio --no-default-features --features stm32f1 --target thumbv7m-none-eabi
  cargo check -p sekai-gpio --no-default-features --features stm32f4 --target thumbv7em-none-eabi
  ```

### Phase 6：ESP32 GPIO 后端

- 文件：`src/esp32.rs`；`Cargo.toml` esp-hal feature。
- 实现 §6；ESP32 规格测试在 host 做。
- 验收：
  ```bash
  cargo run -p sekai-build -- build
  ```
- 如果 ESP32 构建因 GPIO 代码失败，先只启用 `sekai-gpio/esp32` 的 `Esp32Gpio` 编译，再接入 driver。

### Phase 7：ESP32 WiFi/TCP 链路

- 文件：`crate/sekai-start/Cargo.toml`、`src/lib.rs`、`src/driver/esp32.rs`、`src/driver/esp32/net.rs`、`src/clock.rs`、`src/config.rs`、`src/framing.rs`、`src/driver/x86.rs`（若共享改动）、`crate/sekai-build/src/main.rs`（usage/warn）、根 `Cargo.toml`（profile）。
- 实现 §8。
- 先做一个“WiFi 连接 + DHCP + TCP echo/连接打印”的最小验证，再接入 MQTT 主循环。
- 验收：
  ```bash
  cargo run -p sekai-build -- build
  # 有硬件时：
  cargo run -p sekai-build -- flash -- --baud 115200
  ```
- 若 `WifiController`/`Runner` 的 Send 不满足，按 §8.5 定位字段；不得用 `unsafe impl Send`。必要时把网络栈拆成独立 `#[embassy_executor::task]`（Runner 本身应 Send），主任务只持有 `Stack` + `Runtime`。

### Phase 8：文档 + 最终验收

- 写 `docs/sekai-gpio.md`、`docs/sekai-esp32.md`，更新 `docs/sekai-protocol.md`。
- 全量命令矩阵重跑。
- 检查 `Cargo.lock` 已更新、无未使用依赖、无 `TODO/unimplemented`。
- 验收：§1.2 全部通过；有硬件则完成硬件清单。

---

## 12. 测试计划

### 12.1 单元测试（`sekai-gpio`）

- 引脚名：
  - `IO4`/`GPIO4`/`4`/`io4` 归一化；
  - `PA5`/`pa5` 归一化；
  - 非法：空、`IO04`、`IO40`、`PA16`、`A5`、`P5`、`PJ0`（F4 表外）、`PC0`（F1 表外）。
- 规格表：
  - ESP32 35、F1 35、F4 80；
  - 保留/输入专用/不存在；
  - `Port`/slot 映射。
- 配置：
  - 默认输入；
  - 输入→输出、输出→输入；
  - 上拉/下拉；开漏；
  - `level` 对输入、`pull` 对输出、`open_drain` 对输入返回错误；
  - 重复键/未知键/大小写/超长。
- 虚拟后端：
  - 输出写读；输入外部电平；输入专用拒绝输出；保留拒绝；未配置读写错误。
- 工具：
  - info 摘要/详情/port/help；
  - config/read/write 成功结果；
  - 错误码/消息包含引脚；
  - 结果 ≤1024；args >256 拒绝。
- STM32 mock family：
  - 状态机 + 寄存器写入次数/值；
  - 保留/能力错误不触碰硬件。

### 12.2 运行时集成测试（`sekai-gpio/tests/runtime_tools.rs`）

复用 `crate/sekai/tests/sekai_runtime.rs` 的合成 MQTT 思路（复制最小 harness，不修改 `sekai`）：

- 构造 `RuntimeConfig("demo", 1)`，`VirtualGpio`（ESP32/F1/F4 各一个用例）；
- `Runtime::new` 成功；Hello 工具列表包含 4 个 GPIO 工具；
- 发送 `ToolCall gpio.info` args `help`，断言 `Completed` 文本包含 `config`/`read`/`write`；
- `gpio.config IO4 out level=0` → `Completed` 含 `mode=output`；
- `gpio.write IO4 1` → `Completed` 含 `level=1`；
- `gpio.read IO4` → `Failed(InvalidArgument)`；
- `gpio.config IO4 in pull=up` → `Completed`；
- `gpio.read IO4` → `Completed`（虚拟外部默认 0）；
- `gpio.config IO6 out` → `Failed(InvalidArgument)`；
- `gpio.info IO40` → `Failed(InvalidArgument)`；
- `gpio.config PA5 out` 在 F4/F1 下成功；`PC0` 在 F1 下失败。
- 工具总数：6 builtin + 4 GPIO + 1 echo = 11 ≤16；再注册 5 个工具应使 `Runtime::new` 失败（6+11+? >16）——按实际数量断言。

### 12.3 `send` 测试

- `cargo test -p sekai --features send`：
  - `Runtime<SendClock>: Send` 编译断言；
  - 注册 Send 工具/任务并跑一次 ToolCall。
- `cargo test -p sekai-gpio --features send`：
  - 虚拟后端工具注册/调用；
  - `critical-section/std` 提供 host 实现。
- `cargo test --workspace` 默认仍覆盖原有完整运行时测试。

### 12.4 交叉编译

- F1：`cargo check --no-default-features --features stm32f1 --target thumbv7m-none-eabi`
- F4：`cargo check --no-default-features --features stm32f4 --target thumbv7em-none-eabi`
- ESP32：`cargo run -p sekai-build -- build`（debug/release 各一次）
- 说明：arm 目标只编译 `sekai-gpio`，不链接最终固件；最终板级链接由用户工程负责。

### 12.5 硬件验收（有板时）

- ESP32：
  1. 串口看到 WiFi 连接、DHCP 地址、MQTT TCP 连接；
  2. broker 收到 retained Hello，工具列表包含 4 个 GPIO；
  3. `gpio.info help` 返回正确文本；
  4. `gpio.config IO2 out level=0` + `gpio.write IO2 1` 用 LED/逻辑分析仪验证；
  5. 拔网线/关 AP，观察退避重连；
  6. 发送 Jump 到第二个 broker，确认重连和新链路 Hello（如果部署了第二个 broker）。
- STM32：
  1. 用用户板级工程构造 `Stm32Gpio::<F1/F4>`；
  2. 配置 LED 引脚输出并翻转；
  3. 配置输入上拉并读取按键；
  4. 验证保留引脚返回错误。
  本轮仓库内不提供 STM32 链接脚本/启动文件；板级集成由用户工程完成。

---

## 13. 边界情况与失败模式

| 场景 | 预期行为 |
| --- | --- |
| 工具参数非 UTF-8 | `InvalidArgument` |
| 参数 >256 字节 | `InvalidArgument` |
| 结果 >1024 字节 | `ResultTooLarge` |
| 空 args 调 `gpio.info` | 返回摘要 |
| 空 args 调 `gpio.read/write/config` | `InvalidArgument` |
| `gpio.info help` | 返回固定帮助 |
| 引脚名大小写混合 | 接受并规范化 |
| 前导零/越界端口 | `InvalidArgument` |
| 保留引脚 configure/read/write | `InvalidArgument`，不触碰硬件 |
| 输入专用引脚 configure out | `InvalidArgument` |
| 输出模式 read | `InvalidArgument`，提示先配置输入 |
| 输入模式 write | `InvalidArgument`，提示先配置输出 |
| 未配置 read/write | `InvalidArgument` |
| 重复键/方向 | `InvalidArgument` |
| 同时给裸 `out` 和 `dir=in` | `InvalidArgument` |
| `gpio.info port=A` 在 ESP32 | `InvalidArgument` |
| `gpio.info port=Z` | `InvalidArgument` |
| 虚拟外部电平设置到输出专用/保留引脚 | `InvalidArgument` |
| 共享句柄借用冲突 | `Internal("GPIO 被占用")` |
| ESP32 使用不存在的 IO20/24/28..31 | `InvalidArgument` |
| ESP32 未 init 就 `Esp32Gpio::new` | 文档禁止；`AnyPin::steal` 可能未定义行为，必须由驱动保证顺序 |
| ESP32 应用同时使用 GPIO token | 文档禁止；后端独占引脚 |
| WiFi SSID 为空 | 打印错误并进入空转/重试，不 panic |
| MQTT 地址不是 IPv4:port | 打印错误并进入空转/重试 |
| DHCP/TCP/MQTT 失败 | 指数退避重连，保留 runtime 状态 |
| TCP 读到 EOF/错误 | `runtime.link_lost()`，关闭 socket，重连 |
| Jump 期间链路断开 | `link_lost`/`link_failed`，按 Runtime 的 fallback 处理 |
| MQTT 发件箱满 | Runtime 自行处理；网络任务只负责字节流 |
| ESP32 堆不足 | WiFi 初始化失败；增大 `esp_alloc` 堆，打印错误 |

---

## 14. 风险与回退

| 风险 | 影响 | 缓解/回退 |
| --- | --- | --- |
| `send` 特性改动波及现有测试 | 中 | 默认 feature 关闭；gate `Rc` 测试；新增 send 专用测试；逐 crate 验证 |
| ESP32 `WifiController`/`Runner` 不是 Send | 高 | 先写最小 async main 编译验证；把 Runner 放独立 task；若 controller 非 Send，保留在 main 并缩小跨 await 范围；不得 `unsafe impl Send` |
| `embassy-net` 0.9 与 esp-radio 的 driver 版本不匹配 | 高 | 核对 `embassy-net-driver 0.2`；必要时降到 0.7/0.8；以 `cargo tree` 验证 |
| `esp-println` 0.17 与 esp-hal 1.2.2 不兼容 | 中 | 优先保证编译；必要时移除 `esp-println`，用静默重试 + 现有 `defmt`/日志方案 |
| `esp-alloc` 堆不足 | 中 | 增大 reclaimed 堆；减少 TCP buffer（4KB→2KB）；打印错误 |
| STM32 PAC 字段名/feature 与预期不同 | 中 | 以 `stm32f1`/`stm32f4` 0.16 源码为准；只使用 CRL/CRH/MODER/PUPDR/OTYPER/OSPEEDR/IDR/ODR/BSRR 这些稳定寄存器；交叉编译尽早验证 |
| F1 PC0..PC12 表外导致用户板引脚不可用 | 低 | 参考板是 F103C8；文档说明如何扩展 `PinSpec` 表 |
| 保留引脚策略过严 | 低 | 文档说明复制/修改表；AI 安全优先 |
| 文本工具结果被 AI 误解析 | 低 | 固定 key=value、help 自描述、完整测试 |
| 工具数量超限 | 低 | 4 个工具固定；host 默认 11；文档说明关闭 `SEKAI_GPIO` 可释放 4 个槽 |
| ESP32 硬件无法验证 | 中 | 编译 + host 虚拟端到端合格；硬件验收清单交给用户 |

---

## 15. 验收清单

- [ ] `cargo test --workspace` 通过。
- [ ] `cargo test -p sekai --features send` 通过。
- [ ] `cargo test -p sekai-gpio` 通过。
- [ ] `cargo test -p sekai-gpio --features send` 通过。
- [ ] `cargo check` F1 (`thumbv7m-none-eabi`) 通过。
- [ ] `cargo check` F4 (`thumbv7em-none-eabi`) 通过。
- [ ] `cargo run -p sekai-build -- build` 通过。
- [ ] `cargo fmt --all -- --check` 通过。
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` 通过。
- [ ] `sekai-gpio` 工具注册后 Hello 含 `gpio.config/gpio.info/gpio.read/gpio.write`。
- [ ] `gpio.info help` 文本与文档一致。
- [ ] `gpio.config`/`gpio.write`/`gpio.read` 成功结果符合 §5。
- [ ] 保留/输入专用/不存在/模式错误返回 `InvalidArgument`。
- [ ] host `SEKAI_GPIO_FAMILY=esp32|stm32f1|stm32f4` 都能注册并调用。
- [ ] ESP32 固件含 WiFi/TCP/MQTT 主循环，GPIO 工具已注册。
- [ ] STM32F1/F4 后端含真实寄存器实现 + mock 状态机测试。
- [ ] `docs/sekai-gpio.md`、`docs/sekai-esp32.md` 完成；`docs/sekai-protocol.md` 已更新。
- [ ] 有硬件时完成 §12.5 的 ESP32/STM32 清单。

---

## 16. 关键设计不变量（实现时不要偏离）

1. **协议层不变**：GPIO 是用户工具；不新增帧/错误码/`content_type`。参数文本对协议透明。
2. **默认 feature 不破坏 host**：`sekai`/`sekai-gpio` 默认构建不启用 `send`/HAL；现有 host 测试保持通过。
3. **所有后端共享同一套校验/状态机语义**：ESP32、STM32、虚拟后端都必须通过 `resolve_config` 和能力校验，保证 AI 在 host 上学到的行为与硬件一致。
4. **保留引脚先拒绝再碰硬件**：任何 configure/read/write 都不得在保留/不存在/能力不足时访问寄存器或 steal 引脚。
5. **ESP32 工具注册后应用不再直接使用 GPIO token**：`AnyPin::steal` 的安全不变量写入文档。
6. **STM32 只接管 GPIO 端口和 GPIO 时钟位**：保留其他外设所有权；F1/F4 的 RCC 操作幂等。
7. **ESP32 网络循环不阻塞 Embassy 执行器**：所有 TCP 读写/连接都有超时；运行时 poll 之后立即让出。
8. **断线可恢复**：WiFi/TCP/MQTT 任一失败都进入退避重连；Runtime 的 `link_lost`/`link_failed`/`link_up` 与 x86 语义一致。
9. **无 unsafe 绕过 Send**：`send` 通过类型系统实现；不得 `unsafe impl Send`。
10. **文档与测试同步**：`gpio.info help`、十六进制示例、结果格式必须有测试固定，防止文档漂移。
