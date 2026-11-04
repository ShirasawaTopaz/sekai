//! 芯片家族、引脚能力/标志与引脚表。
//!
//! 家族表是**安全默认值**：调试口、SPI flash、输入专用等引脚在这里标记，
//! 任何 configure/read/write 都会在触碰硬件之前被拒绝。AI 通过
//! `gpio.info` 读取这些信息。

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;

use crate::pin::{PinId, PinParseError, Port, format_pin_ranges};
use crate::port::GpioError;

/// 家族大类。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FamilyKind {
    /// Espressif ESP32。
    Esp32,
    /// STMicroelectronics STM32。
    Stm32,
}

impl FamilyKind {
    /// `gpio.info` 的 `family=` 文本。
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Esp32 => "esp32",
            Self::Stm32 => "stm32",
        }
    }
}

/// 具体芯片家族。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChipFamily {
    /// ESP32 classic（Xtensa，非 S3/C3）。
    Esp32,
    /// STM32F103C8（Cortex-M3）。
    Stm32F1,
    /// STM32F407VG（Cortex-M4F）。
    Stm32F4,
}

impl ChipFamily {
    /// 家族大类。
    pub const fn kind(self) -> FamilyKind {
        match self {
            Self::Esp32 => FamilyKind::Esp32,
            Self::Stm32F1 | Self::Stm32F4 => FamilyKind::Stm32,
        }
    }

    /// `gpio.info` 的 `chip=` 文本。
    pub const fn chip(self) -> &'static str {
        match self {
            Self::Esp32 => "esp32",
            Self::Stm32F1 => "stm32f1",
            Self::Stm32F4 => "stm32f4",
        }
    }

    /// 解析宿主环境变量 `SEKAI_GPIO_FAMILY` 的取值（大小写不敏感）。
    pub fn parse(text: &str) -> Option<Self> {
        match text.trim().to_ascii_lowercase().as_str() {
            "esp32" => Some(Self::Esp32),
            "stm32f1" => Some(Self::Stm32F1),
            "stm32f4" => Some(Self::Stm32F4),
            _ => None,
        }
    }

    /// 引脚表。
    pub fn spec(self) -> &'static FamilySpec {
        match self {
            Self::Esp32 => &ESP32_SPEC,
            Self::Stm32F1 => &STM32F1_SPEC,
            Self::Stm32F4 => &STM32F4_SPEC,
        }
    }
}

impl fmt::Display for ChipFamily {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.chip())
    }
}

/// 引脚能力位。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PinCaps {
    /// 可作为输入。
    pub input: bool,
    /// 可作为输出。
    pub output: bool,
    /// 支持内部上拉。
    pub pull_up: bool,
    /// 支持内部下拉。
    pub pull_down: bool,
    /// 支持开漏输出。
    pub open_drain: bool,
}

impl PinCaps {
    /// 无能力（保留引脚）。
    pub const NONE: Self = Self {
        input: false,
        output: false,
        pull_up: false,
        pull_down: false,
        open_drain: false,
    };

    /// 全部能力。
    pub const ALL: Self = Self {
        input: true,
        output: true,
        pull_up: true,
        pull_down: true,
        open_drain: true,
    };

    /// 仅输入（ESP32 IO34..IO39：无内部上下拉、不可输出）。
    pub const INPUT_ONLY: Self = Self {
        input: true,
        output: false,
        pull_up: false,
        pull_down: false,
        open_drain: false,
    };

    /// `gpio.info` 的 `caps=` 取值，例如 `in,out,pu,pd,od`。
    pub fn names(self) -> Vec<&'static str> {
        let mut names = Vec::new();
        if self.input {
            names.push("in");
        }
        if self.output {
            names.push("out");
        }
        if self.pull_up {
            names.push("pu");
        }
        if self.pull_down {
            names.push("pd");
        }
        if self.open_drain {
            names.push("od");
        }
        names
    }
}

/// 引脚特殊标志。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PinFlags {
    /// 保留：configure/read/write 一律拒绝。
    pub reserved: bool,
    /// 仅输入（无内部上下拉、不可输出）。
    pub input_only: bool,
    /// ESP32 strapping 引脚：启动时电平影响启动模式。
    pub strap: bool,
    /// ESP32 UART0（TX/RX）占用。
    pub uart0: bool,
    /// ESP32 WROVER 模块 PSRAM 可能占用。
    pub psram: bool,
    /// STM32 SWD 调试口。
    pub swd: bool,
    /// STM32 JTAG 调试口。
    pub jtag: bool,
    /// STM32 LSE 晶振占用。
    pub osc: bool,
    /// STM32 BOOT1。
    pub boot: bool,
}

impl PinFlags {
    /// 无标志。
    pub const NONE: Self = Self {
        reserved: false,
        input_only: false,
        strap: false,
        uart0: false,
        psram: false,
        swd: false,
        jtag: false,
        osc: false,
        boot: false,
    };

    /// 保留标志。
    pub const RESERVED: Self = Self {
        reserved: true,
        ..Self::NONE
    };

    /// 追加 `reserved`。
    pub const fn with_reserved(mut self) -> Self {
        self.reserved = true;
        self
    }

    /// 追加 `input_only`。
    pub const fn with_input_only(mut self) -> Self {
        self.input_only = true;
        self
    }

    /// 追加 `strap`。
    pub const fn with_strap(mut self) -> Self {
        self.strap = true;
        self
    }

    /// 追加 `uart0`。
    pub const fn with_uart0(mut self) -> Self {
        self.uart0 = true;
        self
    }

    /// 追加 `psram`。
    pub const fn with_psram(mut self) -> Self {
        self.psram = true;
        self
    }

    /// 追加 `swd`。
    pub const fn with_swd(mut self) -> Self {
        self.swd = true;
        self
    }

    /// 追加 `jtag`。
    pub const fn with_jtag(mut self) -> Self {
        self.jtag = true;
        self
    }

    /// 追加 `osc`。
    pub const fn with_osc(mut self) -> Self {
        self.osc = true;
        self
    }

    /// 追加 `boot`。
    pub const fn with_boot(mut self) -> Self {
        self.boot = true;
        self
    }

    /// 是否没有任何标志。
    pub const fn is_empty(self) -> bool {
        !self.reserved
            && !self.input_only
            && !self.strap
            && !self.uart0
            && !self.psram
            && !self.swd
            && !self.jtag
            && !self.osc
            && !self.boot
    }

    /// `gpio.info` 的 `flags=` 取值，按固定顺序输出。
    pub fn names(self) -> Vec<&'static str> {
        let mut names = Vec::new();
        if self.reserved {
            names.push("reserved");
        }
        if self.input_only {
            names.push("input_only");
        }
        if self.strap {
            names.push("strap");
        }
        if self.uart0 {
            names.push("uart0");
        }
        if self.psram {
            names.push("psram");
        }
        if self.swd {
            names.push("swd");
        }
        if self.jtag {
            names.push("jtag");
        }
        if self.osc {
            names.push("osc");
        }
        if self.boot {
            names.push("boot");
        }
        names
    }
}

/// 一个引脚的静态描述。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PinSpec {
    /// 家族无关的引脚标识。
    pub id: PinId,
    /// 后端状态数组下标。
    pub slot: u16,
    /// 能力位。
    pub caps: PinCaps,
    /// 特殊标志。
    pub flags: PinFlags,
    /// 备注（可用于保留原因/警告），可为空。
    pub note: &'static str,
}

/// 家族引脚表与元数据。
#[derive(Debug)]
pub struct FamilySpec {
    /// 家族大类。
    pub family: FamilyKind,
    /// 芯片名，例如 `esp32`。
    pub chip: &'static str,
    /// 表内引脚（按槽位升序）。
    pub pins: &'static [PinSpec],
    /// 后端状态槽位数。
    pub slots: usize,
    /// STM32 端口列表；ESP32 为空。
    pub ports: &'static [Port],
}

impl FamilySpec {
    /// 解析用户输入的引脚名并解析到表项。
    ///
    /// 语法错误/越界编号/不属于当前家族 → `InvalidArgument`。
    pub fn resolve(&self, text: &str) -> Result<&'static PinSpec, GpioError> {
        let pins: &'static [PinSpec] = self.pins;
        let id = match PinId::parse(text) {
            Ok(id) => id,
            Err(PinParseError::IoOutOfRange | PinParseError::PinOutOfRange) => {
                return Err(GpioError::unknown_pin_named(text));
            }
            Err(error) => {
                return Err(GpioError::invalid(alloc::format!(
                    "引脚名 `{text}` 非法：{error}"
                )));
            }
        };
        pins.iter()
            .find(|pin| pin.id == id)
            .ok_or_else(|| GpioError::unknown_pin_named(text))
    }

    /// 按引脚标识查找表项；不属于该家族时返回 `None`。
    pub fn by_id(&self, id: PinId) -> Option<&'static PinSpec> {
        let pins: &'static [PinSpec] = self.pins;
        pins.iter().find(|pin| pin.id == id)
    }

    /// 表内引脚数量。
    pub fn pin_count(&self) -> usize {
        self.pins.len()
    }

    /// 家族摘要：范围压缩后的引脚/保留/输入专用列表。
    pub fn summary(&self) -> FamilySummary {
        let pins = self.pins.iter();
        let labels = format_pin_ranges(pins.clone().map(|pin| pin.id));
        let reserved = format_pin_ranges(
            self.pins
                .iter()
                .filter(|pin| pin.flags.reserved)
                .map(|pin| pin.id),
        );
        let input_only = format_pin_ranges(
            self.pins
                .iter()
                .filter(|pin| pin.flags.input_only)
                .map(|pin| pin.id),
        );
        FamilySummary {
            pins: self.pins.len(),
            ports: self.ports,
            labels,
            reserved,
            input_only,
        }
    }
}

/// 家族摘要（`gpio.info` 无参数输出使用）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FamilySummary {
    /// 表内引脚数。
    pub pins: usize,
    /// STM32 端口；ESP32 为空。
    pub ports: &'static [Port],
    /// 范围压缩后的全部引脚，例如 `IO0-IO23,IO25-IO27,IO32-IO39`。
    pub labels: String,
    /// 范围压缩后的保留引脚。
    pub reserved: String,
    /// 范围压缩后的输入专用引脚。
    pub input_only: String,
}

/// STM32 槽位计算：端口序号 × 16 + 引脚号。
const fn port_slot(port: Port, pin: u8) -> u16 {
    port.index() as u16 * 16 + pin as u16
}

macro_rules! esp_pin {
    ($io:literal, $caps:expr, $flags:expr, $note:literal) => {
        PinSpec {
            id: PinId::Io($io),
            slot: $io as u16,
            caps: $caps,
            flags: $flags,
            note: $note,
        }
    };
}

macro_rules! st_pin {
    ($port:ident, $pin:literal, $flags:expr, $note:literal) => {
        PinSpec {
            id: PinId::port(Port::$port, $pin),
            slot: port_slot(Port::$port, $pin),
            caps: PinCaps::ALL,
            flags: $flags,
            note: $note,
        }
    };
}

/// ESP32 classic 引脚表：34 个可用引脚，槽位 = IO 编号（0..=39）。
///
/// 不含 IO20/IO24/IO28..IO31（芯片未引出），IO6..IO11 为 SPI flash 保留。
/// 计划文档中的 "35 个可用引脚" 把 IO20 误计入 IO18..IO23 范围；本表以
/// 逐脚清单（§4.5 表格与 §6.4 测试项）与硬件事实为准。
pub const ESP32_PINS: &[PinSpec] = &[
    esp_pin!(0, PinCaps::ALL, PinFlags::NONE.with_strap(), ""),
    esp_pin!(1, PinCaps::ALL, PinFlags::NONE.with_uart0(), ""),
    esp_pin!(2, PinCaps::ALL, PinFlags::NONE.with_strap(), ""),
    esp_pin!(3, PinCaps::ALL, PinFlags::NONE.with_uart0(), ""),
    esp_pin!(4, PinCaps::ALL, PinFlags::NONE, ""),
    esp_pin!(5, PinCaps::ALL, PinFlags::NONE.with_strap(), ""),
    esp_pin!(6, PinCaps::NONE, PinFlags::RESERVED, "SPI flash"),
    esp_pin!(7, PinCaps::NONE, PinFlags::RESERVED, "SPI flash"),
    esp_pin!(8, PinCaps::NONE, PinFlags::RESERVED, "SPI flash"),
    esp_pin!(9, PinCaps::NONE, PinFlags::RESERVED, "SPI flash"),
    esp_pin!(10, PinCaps::NONE, PinFlags::RESERVED, "SPI flash"),
    esp_pin!(11, PinCaps::NONE, PinFlags::RESERVED, "SPI flash"),
    esp_pin!(12, PinCaps::ALL, PinFlags::NONE.with_strap(), ""),
    esp_pin!(13, PinCaps::ALL, PinFlags::NONE, ""),
    esp_pin!(14, PinCaps::ALL, PinFlags::NONE, ""),
    esp_pin!(15, PinCaps::ALL, PinFlags::NONE.with_strap(), ""),
    esp_pin!(
        16,
        PinCaps::ALL,
        PinFlags::NONE.with_psram(),
        "WROVER PSRAM"
    ),
    esp_pin!(
        17,
        PinCaps::ALL,
        PinFlags::NONE.with_psram(),
        "WROVER PSRAM"
    ),
    esp_pin!(18, PinCaps::ALL, PinFlags::NONE, ""),
    esp_pin!(19, PinCaps::ALL, PinFlags::NONE, ""),
    esp_pin!(21, PinCaps::ALL, PinFlags::NONE, ""),
    esp_pin!(22, PinCaps::ALL, PinFlags::NONE, ""),
    esp_pin!(23, PinCaps::ALL, PinFlags::NONE, ""),
    esp_pin!(25, PinCaps::ALL, PinFlags::NONE, ""),
    esp_pin!(26, PinCaps::ALL, PinFlags::NONE, ""),
    esp_pin!(27, PinCaps::ALL, PinFlags::NONE, ""),
    esp_pin!(32, PinCaps::ALL, PinFlags::NONE, ""),
    esp_pin!(33, PinCaps::ALL, PinFlags::NONE, ""),
    esp_pin!(
        34,
        PinCaps::INPUT_ONLY,
        PinFlags::NONE.with_input_only(),
        ""
    ),
    esp_pin!(
        35,
        PinCaps::INPUT_ONLY,
        PinFlags::NONE.with_input_only(),
        ""
    ),
    esp_pin!(
        36,
        PinCaps::INPUT_ONLY,
        PinFlags::NONE.with_input_only(),
        ""
    ),
    esp_pin!(
        37,
        PinCaps::INPUT_ONLY,
        PinFlags::NONE.with_input_only(),
        ""
    ),
    esp_pin!(
        38,
        PinCaps::INPUT_ONLY,
        PinFlags::NONE.with_input_only(),
        ""
    ),
    esp_pin!(
        39,
        PinCaps::INPUT_ONLY,
        PinFlags::NONE.with_input_only(),
        ""
    ),
];

/// STM32F103C8（Blue Pill）引脚表：PA0-PA15、PB0-PB15、PC13-PC15，共 35 个。
pub const STM32F1_PINS: &[PinSpec] = &[
    st_pin!(A, 0, PinFlags::NONE, ""),
    st_pin!(A, 1, PinFlags::NONE, ""),
    st_pin!(A, 2, PinFlags::NONE, ""),
    st_pin!(A, 3, PinFlags::NONE, ""),
    st_pin!(A, 4, PinFlags::NONE, ""),
    st_pin!(A, 5, PinFlags::NONE, ""),
    st_pin!(A, 6, PinFlags::NONE, ""),
    st_pin!(A, 7, PinFlags::NONE, ""),
    st_pin!(A, 8, PinFlags::NONE, ""),
    st_pin!(A, 9, PinFlags::NONE, ""),
    st_pin!(A, 10, PinFlags::NONE, ""),
    st_pin!(A, 11, PinFlags::NONE, ""),
    st_pin!(A, 12, PinFlags::NONE, ""),
    st_pin!(A, 13, PinFlags::NONE.with_reserved().with_swd(), "SWD"),
    st_pin!(A, 14, PinFlags::NONE.with_reserved().with_swd(), "SWD"),
    st_pin!(A, 15, PinFlags::NONE.with_reserved().with_jtag(), "JTAG"),
    st_pin!(B, 0, PinFlags::NONE, ""),
    st_pin!(B, 1, PinFlags::NONE, ""),
    st_pin!(B, 2, PinFlags::NONE.with_boot(), "BOOT1"),
    st_pin!(B, 3, PinFlags::NONE.with_reserved().with_jtag(), "JTAG"),
    st_pin!(B, 4, PinFlags::NONE.with_reserved().with_jtag(), "JTAG"),
    st_pin!(B, 5, PinFlags::NONE, ""),
    st_pin!(B, 6, PinFlags::NONE, ""),
    st_pin!(B, 7, PinFlags::NONE, ""),
    st_pin!(B, 8, PinFlags::NONE, ""),
    st_pin!(B, 9, PinFlags::NONE, ""),
    st_pin!(B, 10, PinFlags::NONE, ""),
    st_pin!(B, 11, PinFlags::NONE, ""),
    st_pin!(B, 12, PinFlags::NONE, ""),
    st_pin!(B, 13, PinFlags::NONE, ""),
    st_pin!(B, 14, PinFlags::NONE, ""),
    st_pin!(B, 15, PinFlags::NONE, ""),
    st_pin!(C, 13, PinFlags::NONE, "RTC"),
    st_pin!(C, 14, PinFlags::NONE.with_osc(), "LSE"),
    st_pin!(C, 15, PinFlags::NONE.with_osc(), "LSE"),
];

/// STM32F407VG（LQFP100）引脚表：PA0-PE15 共 80 个。
pub const STM32F4_PINS: &[PinSpec] = &[
    st_pin!(A, 0, PinFlags::NONE, ""),
    st_pin!(A, 1, PinFlags::NONE, ""),
    st_pin!(A, 2, PinFlags::NONE, ""),
    st_pin!(A, 3, PinFlags::NONE, ""),
    st_pin!(A, 4, PinFlags::NONE, ""),
    st_pin!(A, 5, PinFlags::NONE, ""),
    st_pin!(A, 6, PinFlags::NONE, ""),
    st_pin!(A, 7, PinFlags::NONE, ""),
    st_pin!(A, 8, PinFlags::NONE, ""),
    st_pin!(A, 9, PinFlags::NONE, ""),
    st_pin!(A, 10, PinFlags::NONE, ""),
    st_pin!(A, 11, PinFlags::NONE, ""),
    st_pin!(A, 12, PinFlags::NONE, ""),
    st_pin!(A, 13, PinFlags::NONE.with_reserved().with_swd(), "SWD"),
    st_pin!(A, 14, PinFlags::NONE.with_reserved().with_swd(), "SWD"),
    st_pin!(A, 15, PinFlags::NONE.with_reserved().with_jtag(), "JTAG"),
    st_pin!(B, 0, PinFlags::NONE, ""),
    st_pin!(B, 1, PinFlags::NONE, ""),
    st_pin!(B, 2, PinFlags::NONE, ""),
    st_pin!(B, 3, PinFlags::NONE.with_reserved().with_jtag(), "JTAG"),
    st_pin!(B, 4, PinFlags::NONE.with_reserved().with_jtag(), "JTAG"),
    st_pin!(B, 5, PinFlags::NONE, ""),
    st_pin!(B, 6, PinFlags::NONE, ""),
    st_pin!(B, 7, PinFlags::NONE, ""),
    st_pin!(B, 8, PinFlags::NONE, ""),
    st_pin!(B, 9, PinFlags::NONE, ""),
    st_pin!(B, 10, PinFlags::NONE, ""),
    st_pin!(B, 11, PinFlags::NONE, ""),
    st_pin!(B, 12, PinFlags::NONE, ""),
    st_pin!(B, 13, PinFlags::NONE, ""),
    st_pin!(B, 14, PinFlags::NONE, ""),
    st_pin!(B, 15, PinFlags::NONE, ""),
    st_pin!(C, 0, PinFlags::NONE, ""),
    st_pin!(C, 1, PinFlags::NONE, ""),
    st_pin!(C, 2, PinFlags::NONE, ""),
    st_pin!(C, 3, PinFlags::NONE, ""),
    st_pin!(C, 4, PinFlags::NONE, ""),
    st_pin!(C, 5, PinFlags::NONE, ""),
    st_pin!(C, 6, PinFlags::NONE, ""),
    st_pin!(C, 7, PinFlags::NONE, ""),
    st_pin!(C, 8, PinFlags::NONE, ""),
    st_pin!(C, 9, PinFlags::NONE, ""),
    st_pin!(C, 10, PinFlags::NONE, ""),
    st_pin!(C, 11, PinFlags::NONE, ""),
    st_pin!(C, 12, PinFlags::NONE, ""),
    st_pin!(C, 13, PinFlags::NONE, ""),
    st_pin!(C, 14, PinFlags::NONE.with_osc(), "LSE"),
    st_pin!(C, 15, PinFlags::NONE.with_osc(), "LSE"),
    st_pin!(D, 0, PinFlags::NONE, ""),
    st_pin!(D, 1, PinFlags::NONE, ""),
    st_pin!(D, 2, PinFlags::NONE, ""),
    st_pin!(D, 3, PinFlags::NONE, ""),
    st_pin!(D, 4, PinFlags::NONE, ""),
    st_pin!(D, 5, PinFlags::NONE, ""),
    st_pin!(D, 6, PinFlags::NONE, ""),
    st_pin!(D, 7, PinFlags::NONE, ""),
    st_pin!(D, 8, PinFlags::NONE, ""),
    st_pin!(D, 9, PinFlags::NONE, ""),
    st_pin!(D, 10, PinFlags::NONE, ""),
    st_pin!(D, 11, PinFlags::NONE, ""),
    st_pin!(D, 12, PinFlags::NONE, ""),
    st_pin!(D, 13, PinFlags::NONE, ""),
    st_pin!(D, 14, PinFlags::NONE, ""),
    st_pin!(D, 15, PinFlags::NONE, ""),
    st_pin!(E, 0, PinFlags::NONE, ""),
    st_pin!(E, 1, PinFlags::NONE, ""),
    st_pin!(E, 2, PinFlags::NONE, ""),
    st_pin!(E, 3, PinFlags::NONE, ""),
    st_pin!(E, 4, PinFlags::NONE, ""),
    st_pin!(E, 5, PinFlags::NONE, ""),
    st_pin!(E, 6, PinFlags::NONE, ""),
    st_pin!(E, 7, PinFlags::NONE, ""),
    st_pin!(E, 8, PinFlags::NONE, ""),
    st_pin!(E, 9, PinFlags::NONE, ""),
    st_pin!(E, 10, PinFlags::NONE, ""),
    st_pin!(E, 11, PinFlags::NONE, ""),
    st_pin!(E, 12, PinFlags::NONE, ""),
    st_pin!(E, 13, PinFlags::NONE, ""),
    st_pin!(E, 14, PinFlags::NONE, ""),
    st_pin!(E, 15, PinFlags::NONE, ""),
];

/// ESP32 家族表。
pub static ESP32_SPEC: FamilySpec = FamilySpec {
    family: FamilyKind::Esp32,
    chip: "esp32",
    pins: ESP32_PINS,
    slots: 40,
    ports: &[],
};

/// STM32F1 家族表。
pub static STM32F1_SPEC: FamilySpec = FamilySpec {
    family: FamilyKind::Stm32,
    chip: "stm32f1",
    pins: STM32F1_PINS,
    slots: 48,
    ports: &[Port::A, Port::B, Port::C],
};

/// STM32F4 家族表。
pub static STM32F4_SPEC: FamilySpec = FamilySpec {
    family: FamilyKind::Stm32,
    chip: "stm32f4",
    pins: STM32F4_PINS,
    slots: 80,
    ports: &[Port::A, Port::B, Port::C, Port::D, Port::E],
};

#[cfg(test)]
mod tests {
    use super::{
        ChipFamily, ESP32_PINS, FamilyKind, PinCaps, PinFlags, STM32F1_PINS, STM32F4_PINS,
    };
    use crate::pin::{PinId, Port};
    use crate::port::GpioError;

    #[test]
    fn pin_counts_and_slots() {
        // ESP32 classic 表内 34 个引脚：GPIO0-19、21-23、25-27、32-39。
        // （计划文档写 35 是把 IO20 误算进 IO18..IO23 范围；§4.5/§6.4 明确
        //  IO20 不存在，这里按逐脚清单与硬件事实取 34。）
        assert_eq!(ChipFamily::Esp32.spec().pins.len(), 34);
        assert_eq!(ChipFamily::Esp32.spec().slots, 40);
        assert_eq!(ChipFamily::Stm32F1.spec().pins.len(), 35);
        assert_eq!(ChipFamily::Stm32F1.spec().slots, 48);
        assert_eq!(ChipFamily::Stm32F4.spec().pins.len(), 80);
        assert_eq!(ChipFamily::Stm32F4.spec().slots, 80);
        assert_eq!(ChipFamily::Esp32.spec().family, FamilyKind::Esp32);
        assert_eq!(ChipFamily::Stm32F1.spec().family, FamilyKind::Stm32);
        assert_eq!(ChipFamily::Stm32F4.spec().family, FamilyKind::Stm32);
        assert_eq!(ChipFamily::Esp32.chip(), "esp32");
        assert_eq!(ChipFamily::Stm32F1.chip(), "stm32f1");
        assert_eq!(ChipFamily::Stm32F4.chip(), "stm32f4");
    }

    #[test]
    fn esp32_slots_equal_io_numbers() {
        for pin in ESP32_PINS {
            match pin.id {
                PinId::Io(number) => assert_eq!(pin.slot, u16::from(number)),
                PinId::Port { .. } => panic!("ESP32 表出现 STM32 引脚"),
            }
        }
    }

    #[test]
    fn stm32_slots_are_port_16_plus_pin() {
        for pin in STM32F1_PINS.iter().chain(STM32F4_PINS.iter()) {
            match pin.id {
                PinId::Port { port, pin: number } => {
                    assert_eq!(pin.slot, u16::from(port.index()) * 16 + u16::from(number));
                }
                PinId::Io(_) => panic!("STM32 表出现 ESP32 引脚"),
            }
        }
    }

    #[test]
    fn esp32_reserved_and_input_only() {
        let spec = ChipFamily::Esp32.spec();
        for number in 6..=11 {
            let pin = spec.by_id(PinId::io(number)).unwrap();
            assert!(pin.flags.reserved, "IO{number} 应保留");
            assert_eq!(pin.caps, PinCaps::NONE);
            assert_eq!(pin.note, "SPI flash");
        }
        for number in 34..=39 {
            let pin = spec.by_id(PinId::io(number)).unwrap();
            assert!(pin.flags.input_only, "IO{number} 应仅输入");
            assert!(!pin.caps.output);
            assert!(!pin.caps.pull_up && !pin.caps.pull_down);
        }
        for number in [20, 24, 28, 29, 30, 31] {
            assert!(spec.by_id(PinId::io(number)).is_none(), "IO{number} 不存在");
        }
    }

    #[test]
    fn esp32_flag_pins() {
        let spec = ChipFamily::Esp32.spec();
        for number in [0, 2, 5, 12, 15] {
            assert!(
                spec.by_id(PinId::io(number)).unwrap().flags.strap,
                "IO{number} 应为 strap"
            );
        }
        for number in [1, 3] {
            assert!(
                spec.by_id(PinId::io(number)).unwrap().flags.uart0,
                "IO{number} 应为 uart0"
            );
        }
        for number in [16, 17] {
            assert!(
                spec.by_id(PinId::io(number)).unwrap().flags.psram,
                "IO{number} 应为 psram"
            );
        }
    }

    #[test]
    fn stm32_reserved_flags() {
        let spec = ChipFamily::Stm32F1.spec();
        for (port, number, swd, jtag) in [
            (Port::A, 13, true, false),
            (Port::A, 14, true, false),
            (Port::A, 15, false, true),
            (Port::B, 3, false, true),
            (Port::B, 4, false, true),
        ] {
            let pin = spec.by_id(PinId::port(port, number)).unwrap();
            assert!(pin.flags.reserved, "P{port}{number} 应保留");
            assert_eq!(pin.flags.swd, swd);
            assert_eq!(pin.flags.jtag, jtag);
        }
        assert!(spec.by_id(PinId::port(Port::B, 2)).unwrap().flags.boot);
        assert!(spec.by_id(PinId::port(Port::C, 14)).unwrap().flags.osc);
        assert!(!spec.by_id(PinId::port(Port::C, 13)).unwrap().flags.osc);
        // F1 表外引脚。
        assert!(spec.by_id(PinId::port(Port::C, 0)).is_none());
        assert!(spec.by_id(PinId::port(Port::D, 0)).is_none());
    }

    #[test]
    fn resolve_reports_bad_pins() {
        let spec = ChipFamily::Esp32.spec();
        assert_eq!(spec.resolve("io4").unwrap().id, PinId::io(4));
        assert_eq!(spec.resolve("GPIO4").unwrap().id, PinId::io(4));
        assert!(matches!(
            spec.resolve("IO20"),
            Err(GpioError::UnknownPin(_))
        ));
        assert!(matches!(
            spec.resolve("IO6"),
            Ok(pin) if pin.flags.reserved
        ));
        assert!(matches!(spec.resolve("PA5"), Err(GpioError::UnknownPin(_))));
        assert!(matches!(
            spec.resolve("IO04"),
            Err(GpioError::InvalidArgument(_))
        ));
        assert!(matches!(
            spec.resolve("IO40"),
            Err(GpioError::UnknownPin(_))
        ));

        let stm32 = ChipFamily::Stm32F1.spec();
        assert_eq!(stm32.resolve("PA5").unwrap().id, PinId::port(Port::A, 5));
        assert!(matches!(
            stm32.resolve("PJ0"),
            Err(GpioError::UnknownPin(_))
        ));
        assert!(matches!(
            stm32.resolve("PC0"),
            Err(GpioError::UnknownPin(_))
        ));
        assert!(matches!(stm32.resolve("4"), Err(GpioError::UnknownPin(_))));
    }

    #[test]
    fn summary_matches_documentation_examples() {
        let esp32 = ChipFamily::Esp32.spec().summary();
        assert_eq!(esp32.pins, 34);
        assert_eq!(esp32.labels, "IO0-IO19,IO21-IO23,IO25-IO27,IO32-IO39");
        assert_eq!(esp32.reserved, "IO6-IO11");
        assert_eq!(esp32.input_only, "IO34-IO39");
        assert!(esp32.ports.is_empty());

        let f1 = ChipFamily::Stm32F1.spec().summary();
        assert_eq!(f1.labels, "PA0-PA15,PB0-PB15,PC13-PC15");
        assert_eq!(f1.reserved, "PA13-PA15,PB3,PB4");
        assert_eq!(f1.input_only, "");
        assert_eq!(f1.ports, &[Port::A, Port::B, Port::C]);

        let f4 = ChipFamily::Stm32F4.spec().summary();
        assert_eq!(f4.pins, 80);
        assert_eq!(f4.reserved, "PA13-PA15,PB3,PB4");
        assert_eq!(f4.ports, &[Port::A, Port::B, Port::C, Port::D, Port::E]);
    }

    #[test]
    fn ids_and_slots_are_unique() {
        let mut ids = alloc::collections::BTreeSet::new();
        let mut slots = alloc::collections::BTreeSet::new();
        for family in [ChipFamily::Esp32, ChipFamily::Stm32F1, ChipFamily::Stm32F4] {
            ids.clear();
            slots.clear();
            for pin in family.spec().pins {
                assert!(ids.insert(pin.id), "{} 表内引脚重复", family.chip());
                assert!(slots.insert(pin.slot), "{} 表内槽位重复", family.chip());
            }
        }
    }

    #[test]
    fn flags_names_follow_documented_order() {
        let flags = PinFlags::NONE.with_reserved().with_swd();
        assert_eq!(flags.names(), alloc::vec!["reserved", "swd"]);
        assert!(PinFlags::NONE.is_empty());
        assert!(!flags.is_empty());
        assert_eq!(
            PinCaps::ALL.names(),
            alloc::vec!["in", "out", "pu", "pd", "od"]
        );
        assert_eq!(PinCaps::INPUT_ONLY.names(), alloc::vec!["in"]);
        assert!(PinCaps::NONE.names().is_empty());
    }
}
