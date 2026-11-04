//! Sekai 家族无关 GPIO 抽象。
//!
//! 本 crate 提供：
//! - 家族无关的引脚标识/解析（[`pin`]）与配置状态机（[`config`]）；
//! - ESP32 classic / STM32F1 / STM32F4 引脚表与能力/保留标志（[`family`]）；
//! - [`GpioPort`] 后端抽象（[`port`]）；
//! - 宿主虚拟后端（[`virtual_gpio`]，默认构建即可用；可用
//!   [`VirtualGpio::with_inputs`] / [`parse_external_levels`] 注入初始外部电平）；
//! - AI 可直接调用的 `gpio.info/config/read/write` 用户工具（[`tools`]）；
//! - 可选的真实硬件后端：ESP32（`esp32` 特性）、STM32F1/F4（`stm32f1`/`stm32f4` 特性）。
//!
//! 默认构建不依赖任何 HAL；`send` 特性把工具共享句柄切换到
//! `Arc<critical_section::Mutex<RefCell<_>>>`，以配合异步运行时。
//!
//! 协议层保持不变：GPIO 工具是用户工具，参数与结果都是 UTF-8 文本。

#![no_std]
#![deny(unsafe_code)]

extern crate alloc;

#[cfg(test)]
extern crate std;

pub mod config;
pub mod family;
pub mod pin;
pub mod port;
pub mod text;
pub mod tools;
pub mod virtual_gpio;

#[cfg(feature = "esp32")]
#[allow(unsafe_code)]
pub mod esp32;

#[cfg(any(feature = "stm32f1", feature = "stm32f4"))]
#[allow(unsafe_code)]
pub mod stm32;

pub use config::{ConfigRequest, Direction, Level, PinConfig, Pull, resolve_config};
pub use family::{ChipFamily, FamilyKind, FamilySpec, FamilySummary, PinCaps, PinFlags, PinSpec};
pub use pin::{Label, PinId, PinParseError, Port};
pub use port::{BackendKind, GpioError, GpioPort, PinDescription};
pub use tools::{GPIO_TOOL_NAMES, register_tools};
pub use virtual_gpio::{VirtualGpio, parse_external_levels};
