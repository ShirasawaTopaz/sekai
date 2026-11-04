//! STM32F407VG（F4）PAC 寄存器后端。
//!
//! 直接使用 `stm32f4` PAC 的 GPIO 寄存器块：
//!
//! - 时钟：`RCC.ahb1enr` 的 GPIOAEN..GPIOEEN（bit0..bit4，读改写）；
//! - MODER：每引脚 2 bit（`00` 输入、`01` 输出）；
//! - OTYPER：每引脚 1 bit（`0` 推挽、`1` 开漏）；
//! - PUPDR：每引脚 2 bit（`00` 无、`01` 上拉、`10` 下拉）；
//! - OSPEEDR：每引脚 2 bit（输出 medium=`01`）；
//! - 数据：BSRR 置位/复位（写高 `1 << pin`，写低 `1 << (pin + 16)`）；
//!   输入读 IDR，输出锁存值由上层状态机记录。
//!
//! `configure` 采用无毛刺顺序：先写 MODER 关闭输出（输入），再写
//! OTYPER/PUPDR/OSPEEDR，然后写 ODR 初始电平，最后 MODER=输出。
//!
//! 端口 A..E 的寄存器块是互不相同的 PAC 类型，因此用宏按端口展开寄存器
//! 访问；宏只在本文件内使用。本模块只做寄存器读写（唯一允许的 `unsafe`
//! 位置）；保留/能力/模式校验由 `crate::stm32::Stm32Gpio` 在调用前完成。

use stm32f4::stm32f407 as pac;

use super::{Stm32Family, insert_bits};
use crate::config::{Direction, Level, PinConfig, Pull};
use crate::family::{FamilySpec, PinSpec, STM32F4_SPEC};
use crate::pin::{PinId, Port};

/// F4 端口集合（GPIOA..GPIOE）。
#[derive(Debug)]
pub struct F4Ports {
    /// GPIOA（PA0..PA15）。
    pub gpioa: pac::GPIOA,
    /// GPIOB（PB0..PB15）。
    pub gpiob: pac::GPIOB,
    /// GPIOC（PC0..PC15）。
    pub gpioc: pac::GPIOC,
    /// GPIOD（PD0..PD15）。
    pub gpiod: pac::GPIOD,
    /// GPIOE（PE0..PE15）。
    pub gpioe: pac::GPIOE,
}

/// STM32F407VG（F4）家族标记。
#[derive(Debug)]
pub struct F4;

/// MODER 编码：输入。
const MODER_INPUT: u32 = 0b00;
/// MODER 编码：通用输出。
const MODER_OUTPUT: u32 = 0b01;
/// OSPEEDR 编码：medium（输入时写 00）。
const OSPEEDR_MEDIUM: u32 = 0b01;

/// AHB1ENR 中 GPIOA..GPIOE 时钟位（GPIOAEN=bit0 .. GPIOEEN=bit4）。
const AHB1_GPIO_CLOCKS: u32 = 0b1_1111;

/// 对指定端口寄存器块执行配置。
///
/// 端口 A..E 的 `RegisterBlock` 类型各自独立（PAC 生成代码），无法写成同一
/// 个泛型函数；宏把同一段寄存器序列在匹配到的端口上展开。
macro_rules! configure_port {
    ($gpio:expr, $pin:expr, $config:expr) => {{
        let gpio = $gpio;
        let pin: u8 = $pin;
        let config: PinConfig = $config;
        let shift = u32::from(pin) * 2;
        // 1) 先切输入，关闭输出。
        gpio.moder()
            .modify(|r, w| unsafe { w.bits(insert_bits(r.bits(), shift, 2, MODER_INPUT)) });
        // 2) 输出类型/速度/上下拉。
        gpio.otyper().modify(|r, w| unsafe {
            w.bits(insert_bits(
                r.bits(),
                u32::from(pin),
                1,
                u32::from(config.open_drain),
            ))
        });
        gpio.ospeedr().modify(|r, w| unsafe {
            w.bits(insert_bits(r.bits(), shift, 2, ospeedr_bits(config)))
        });
        gpio.pupdr().modify(|r, w| unsafe {
            w.bits(insert_bits(r.bits(), shift, 2, pupdr_bits(config.pull)))
        });
        if config.direction == Direction::Output {
            // 3) 先写 ODR 初始电平，4) 再启用输出，避免毛刺。
            gpio.bsrr()
                .write(|w| unsafe { w.bits(bsrr_bits(pin, config.level)) });
            gpio.moder()
                .modify(|r, w| unsafe { w.bits(insert_bits(r.bits(), shift, 2, MODER_OUTPUT)) });
        }
    }};
}

/// 读指定端口的 IDR。
macro_rules! read_port {
    ($gpio:expr, $pin:expr) => {{ Level::from_bool($gpio.idr().read().bits() & (1u32 << $pin) != 0) }};
}

/// 用指定端口的 BSRR 写输出电平。
macro_rules! write_port {
    ($gpio:expr, $pin:expr, $level:expr) => {{
        $gpio
            .bsrr()
            .write(|w| unsafe { w.bits(bsrr_bits($pin, $level)) });
    }};
}

impl Stm32Family for F4 {
    const SPEC: &'static FamilySpec = &STM32F4_SPEC;
    type Ports = F4Ports;
    type Rcc = pac::RCC;

    fn enable_clocks(rcc: &mut pac::RCC, _ports: &mut F4Ports) {
        // 只置位 GPIOA..GPIOE 时钟，保留板级已使能的其他 AHB1 外设位。
        rcc.ahb1enr()
            .modify(|r, w| unsafe { w.bits(r.bits() | AHB1_GPIO_CLOCKS) });
    }

    fn configure(ports: &mut F4Ports, spec: &'static PinSpec, config: PinConfig) {
        let PinId::Port { port, pin } = spec.id else {
            return;
        };
        match port {
            Port::A => configure_port!(&ports.gpioa, pin, config),
            Port::B => configure_port!(&ports.gpiob, pin, config),
            Port::C => configure_port!(&ports.gpioc, pin, config),
            Port::D => configure_port!(&ports.gpiod, pin, config),
            Port::E => configure_port!(&ports.gpioe, pin, config),
            _ => {}
        }
    }

    fn read(ports: &F4Ports, spec: &'static PinSpec) -> Level {
        let PinId::Port { port, pin } = spec.id else {
            return Level::Low;
        };
        match port {
            Port::A => read_port!(&ports.gpioa, pin),
            Port::B => read_port!(&ports.gpiob, pin),
            Port::C => read_port!(&ports.gpioc, pin),
            Port::D => read_port!(&ports.gpiod, pin),
            Port::E => read_port!(&ports.gpioe, pin),
            _ => Level::Low,
        }
    }

    fn write(ports: &mut F4Ports, spec: &'static PinSpec, level: Level) {
        let PinId::Port { port, pin } = spec.id else {
            return;
        };
        match port {
            Port::A => write_port!(&ports.gpioa, pin, level),
            Port::B => write_port!(&ports.gpiob, pin, level),
            Port::C => write_port!(&ports.gpioc, pin, level),
            Port::D => write_port!(&ports.gpiod, pin, level),
            Port::E => write_port!(&ports.gpioe, pin, level),
            _ => {}
        }
    }
}

/// OSPEEDR 编码：输出 medium（01），输入写 00。
fn ospeedr_bits(config: PinConfig) -> u32 {
    match config.direction {
        Direction::Output => OSPEEDR_MEDIUM,
        Direction::Input => 0b00,
    }
}

/// PUPDR 编码：无 00、上拉 01、下拉 10。
fn pupdr_bits(pull: Pull) -> u32 {
    match pull {
        Pull::None => 0b00,
        Pull::Up => 0b01,
        Pull::Down => 0b10,
    }
}

/// BSRR 位：写高用低 16 位（置位），写低用高 16 位（复位）。
fn bsrr_bits(pin: u8, level: Level) -> u32 {
    match level {
        Level::High => 1u32 << pin,
        Level::Low => 1u32 << (u32::from(pin) + 16),
    }
}

#[cfg(test)]
mod tests {
    use super::{F4, bsrr_bits, ospeedr_bits, pupdr_bits};
    use crate::config::{Direction, Level, PinConfig, Pull};
    use crate::family::ChipFamily;
    use crate::pin::{PinId, Port};
    use crate::stm32::Stm32Family;

    use Direction::{Input, Output};
    use Level::{High, Low};
    use Pull::{Down, None as NoPull, Up};

    fn input(pull: Pull) -> PinConfig {
        PinConfig {
            direction: Input,
            level: Low,
            pull,
            open_drain: false,
        }
    }

    fn output(level: Level, open_drain: bool) -> PinConfig {
        PinConfig {
            direction: Output,
            level,
            pull: NoPull,
            open_drain,
        }
    }

    #[test]
    fn spec_matches_f407vg_table() {
        assert_eq!(F4::SPEC.chip, "stm32f4");
        assert_eq!(F4::SPEC.pins.len(), 80);
        assert_eq!(F4::SPEC.slots, 80);
        assert_eq!(
            F4::SPEC.ports,
            &[Port::A, Port::B, Port::C, Port::D, Port::E]
        );
        assert!(core::ptr::eq(F4::SPEC, ChipFamily::Stm32F4.spec()));
        // PA0..PE15 都在表内。
        for port in [Port::A, Port::B, Port::C, Port::D, Port::E] {
            assert!(F4::SPEC.by_id(PinId::port(port, 0)).is_some());
            assert!(F4::SPEC.by_id(PinId::port(port, 15)).is_some());
        }
        // 端口 F..K 不在 F407VG 表内。
        assert!(F4::SPEC.by_id(PinId::port(Port::F, 0)).is_none());
        for (port, pin) in [
            (Port::A, 13),
            (Port::A, 14),
            (Port::A, 15),
            (Port::B, 3),
            (Port::B, 4),
        ] {
            let spec = F4::SPEC.by_id(PinId::port(port, pin)).unwrap();
            assert!(spec.flags.reserved, "P{port}{pin} 应保留");
        }
    }

    #[test]
    fn slots_are_port_16_plus_pin() {
        for spec in F4::SPEC.pins {
            let PinId::Port { port, pin } = spec.id else {
                panic!("F4 表出现 ESP32 引脚");
            };
            assert_eq!(spec.slot, u16::from(port.index()) * 16 + u16::from(pin));
        }
    }

    #[test]
    fn register_encodings_match_reference_manual() {
        // MODER：输出 01、输入 00。
        assert_eq!(ospeedr_bits(output(High, false)), 0b01);
        assert_eq!(ospeedr_bits(input(NoPull)), 0b00);
        // OSPEEDR：输出 medium=01，输入=00。
        assert_eq!(pupdr_bits(NoPull), 0b00);
        assert_eq!(pupdr_bits(Up), 0b01);
        assert_eq!(pupdr_bits(Down), 0b10);
        // OTYPER 编码直接来自 PinConfig::open_drain。
        assert_eq!(u32::from(output(High, true).open_drain), 1);
        assert_eq!(u32::from(output(High, false).open_drain), 0);
    }

    #[test]
    fn bsrr_uses_set_and_reset_halves() {
        assert_eq!(bsrr_bits(0, High), 1);
        assert_eq!(bsrr_bits(5, High), 1 << 5);
        assert_eq!(bsrr_bits(15, High), 1 << 15);
        assert_eq!(bsrr_bits(0, Low), 1 << 16);
        assert_eq!(bsrr_bits(5, Low), 1 << (5 + 16));
        assert_eq!(bsrr_bits(15, Low), 1 << (15 + 16));
        assert_eq!(bsrr_bits(5, High) & bsrr_bits(5, Low), 0);
    }
}
