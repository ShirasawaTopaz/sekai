//! ESP32 classic 真实 GPIO 后端。
//!
//! 使用 `esp-hal` 1.2.2 的 `Flex`（`unstable` 特性）统一运行时的输入/输出/
//! 上下拉/开漏切换；引脚延迟到首次 configure 时通过 `AnyPin::steal` 获取。
//!
//! # 安全不变量
//!
//! [`Esp32Gpio::new`] 不接收 `Peripherals`，而是保存延迟创建的
//! [`esp_hal::gpio::Flex`] 槽位；第一次配置某个引脚时才用
//! `unsafe { esp_hal::gpio::AnyPin::steal(number) }` 取得该引脚。因此调用方
//! 必须保证：
//!
//! 1. 驱动在 `esp_hal::init()` 之后、且在使用任何 `GPIOx` token 之前创建；
//! 2. 应用把 GPIO token 交给本后端后，不再直接使用 `Peripherals.GPIOx`；
//! 3. 每个引脚只会被 steal 一次（槽位为 `Some` 之后不再 steal）。
//!
//! 这段 unsafe 是本模块唯一的 unsafe 点；保留/不存在/能力不足的引脚会在
//! steal 之前被核心校验拒绝。

use esp_hal::gpio::{
    AnyPin, DriveMode, Flex, InputConfig, Level as HalLevel, OutputConfig, Pull as HalPull,
};

use crate::config::{self, Direction, Level, PinConfig, Pull};
use crate::family::{ChipFamily, FamilySpec};
use crate::pin::PinId;
use crate::port::{self, GpioError, GpioPort, PinDescription};

/// ESP32 classic 引脚槽位数（IO0..IO39）。
const SLOTS: usize = 40;

/// ESP32 classic GPIO 端口。
pub struct Esp32Gpio {
    spec: &'static FamilySpec,
    /// 已 steal 的引脚驱动；下标是引脚槽位（= IO 编号）。
    pins: [Option<Flex<'static>>; SLOTS],
    /// 每个引脚的当前配置；未配置为 `None`。
    configs: [Option<PinConfig>; SLOTS],
}

impl Esp32Gpio {
    /// 创建后端；必须在 `esp_hal::init` 之后调用，且应用不得再直接使用 GPIO token。
    pub fn new() -> Self {
        Self {
            spec: ChipFamily::Esp32.spec(),
            pins: core::array::from_fn(|_| None),
            configs: [None; SLOTS],
        }
    }

    /// 取得引脚槽位下标；ESP32 后端只接受 `IO<n>`。
    fn slot(pin: PinId) -> Result<usize, GpioError> {
        match pin {
            PinId::Io(number) => Ok(usize::from(number)),
            PinId::Port { .. } => Err(GpioError::invalid(alloc::format!(
                "引脚 {pin} 不属于 ESP32 家族"
            ))),
        }
    }

    /// 取得（必要时延迟创建）引脚驱动。
    fn flex(
        &mut self,
        spec: &'static crate::family::PinSpec,
    ) -> Result<&mut Flex<'static>, GpioError> {
        let slot = Self::slot(spec.id)?;
        if self.pins[slot].is_none() {
            // 安全：见模块文档的安全不变量——调用方保证在 esp_hal::init 之后
            // 创建本后端、独占 GPIO token，且每个槽位只 steal 一次。
            let number = match spec.id {
                PinId::Io(number) => number,
                PinId::Port { .. } => unreachable!("ESP32 家族表只包含 IO 引脚"),
            };
            let any = unsafe { AnyPin::steal(number) };
            self.pins[slot] = Some(Flex::new(any));
        }
        self.pins[slot]
            .as_mut()
            .ok_or_else(|| GpioError::backend(alloc::format!("引脚 {} 驱动不可用", spec.id)))
    }
}

impl Default for Esp32Gpio {
    fn default() -> Self {
        Self::new()
    }
}

/// 把核心电平映射到 esp-hal 电平。
const fn map_level(level: Level) -> HalLevel {
    match level {
        Level::Low => HalLevel::Low,
        Level::High => HalLevel::High,
    }
}

/// 把核心上下拉映射到 esp-hal 上下拉。
const fn map_pull(pull: Pull) -> HalPull {
    match pull {
        Pull::None => HalPull::None,
        Pull::Up => HalPull::Up,
        Pull::Down => HalPull::Down,
    }
}

impl GpioPort for Esp32Gpio {
    fn spec(&self) -> &'static FamilySpec {
        self.spec
    }

    fn configure(&mut self, pin: PinId, config: PinConfig) -> Result<(), GpioError> {
        let spec = port::lookup(self.spec, pin)?;
        // 保留/能力/方向字段在触碰硬件前统一校验。
        config::validate_config(spec, &config)?;

        let slot = Self::slot(pin)?;
        let flex = self.flex(spec)?;
        match config.direction {
            Direction::Input => {
                flex.set_output_enable(false);
                flex.set_input_enable(true);
                flex.apply_input_config(&InputConfig::default().with_pull(map_pull(config.pull)));
            }
            Direction::Output => {
                flex.set_level(map_level(config.level));
                flex.apply_output_config(&OutputConfig::default().with_drive_mode(
                    if config.open_drain {
                        DriveMode::OpenDrain
                    } else {
                        DriveMode::PushPull
                    },
                ));
                flex.set_output_enable(true);
                flex.set_input_enable(false);
            }
        }
        self.configs[slot] = Some(config);
        Ok(())
    }

    fn read(&mut self, pin: PinId) -> Result<Level, GpioError> {
        let spec = port::lookup(self.spec, pin)?;
        let slot = Self::slot(pin)?;
        let config = port::ensure_input_mode(spec, self.configs[slot])?;
        debug_assert_eq!(config.direction, Direction::Input);
        let flex = self.flex(spec)?;
        Ok(Level::from_bool(flex.level() == HalLevel::High))
    }

    fn write(&mut self, pin: PinId, level: Level) -> Result<(), GpioError> {
        let spec = port::lookup(self.spec, pin)?;
        let slot = Self::slot(pin)?;
        let mut config = port::ensure_output_mode(spec, self.configs[slot])?;
        debug_assert_eq!(config.direction, Direction::Output);

        let flex = self.flex(spec)?;
        flex.set_level(map_level(level));

        // 记录最新锁存电平，供 gpio.info 的 describe 使用。
        config.level = level;
        self.configs[slot] = Some(config);
        Ok(())
    }

    fn describe(&mut self, pin: PinId) -> Result<PinDescription, GpioError> {
        let spec = port::lookup(self.spec, pin)?;
        let slot = Self::slot(pin)?;
        let config = self.configs[slot];
        let level = if let Some(config) = config {
            let flex = self.flex(spec)?;
            Some(match config.direction {
                Direction::Input => Level::from_bool(flex.level() == HalLevel::High),
                Direction::Output => Level::from_bool(flex.output_level() == HalLevel::High),
            })
        } else {
            None
        };
        Ok(PinDescription {
            spec,
            config,
            level,
        })
    }

    fn configured_count(&self) -> usize {
        self.configs
            .iter()
            .filter(|config| config.is_some())
            .count()
    }
}

#[cfg(test)]
mod tests {
    use super::SLOTS;
    use crate::family::ChipFamily;
    use crate::pin::PinId;

    /// 槽位下标必须是 IO 编号，且 IO 编号必须落在槽位数组内。
    #[test]
    fn esp32_slots_fit_io_numbers() {
        for pin in ChipFamily::Esp32.spec().pins {
            match pin.id {
                PinId::Io(number) => {
                    assert!(usize::from(number) < SLOTS, "IO{number} 超出槽位");
                    assert_eq!(pin.slot, u16::from(number));
                }
                PinId::Port { .. } => panic!("ESP32 表出现 STM32 引脚"),
            }
        }
    }
}
