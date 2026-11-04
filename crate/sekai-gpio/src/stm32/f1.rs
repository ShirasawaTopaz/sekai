//! STM32F103C8（F1）PAC 寄存器后端。
//!
//! 直接使用 `stm32f1` PAC 的 GPIO 寄存器块：
//!
//! - 时钟：`RCC.apb2enr` 的 IOPAEN/IOPBEN/IOPCEN（bit2..bit4，读改写）；
//! - 配置：CRL（pin 0..7）/ CRH（pin 8..15），每引脚 4 bit `CNF[1:0] MODE[1:0]`；
//! - 输入浮空：`01_00`；输入上/下拉：`10_00` + ODR 对应位（1=上拉，0=下拉）；
//! - 输出推挽 2MHz：`00_10`；输出开漏 2MHz：`01_10`；
//! - 数据：BSRR 置位/复位（写高 `1 << pin`，写低 `1 << (pin + 16)`）；
//!   输入读 IDR，输出锁存值由上层状态机记录。
//!
//! `configure` 采用无毛刺顺序：输出引脚先切输入浮空关闭输出，再写 ODR 初始
//! 电平，最后写最终输出模式；输入引脚先写 CRL/CRH，再用 ODR 位选择上下拉。
//!
//! 本模块只做寄存器读写（唯一允许的 `unsafe` 位置）；保留/能力/模式校验由
//! `crate::stm32::Stm32Gpio` 在调用前统一完成，因此这里不再重复校验。

use stm32f1::stm32f103 as pac;

use super::{Stm32Family, insert_bits};
use crate::config::{Direction, Level, PinConfig, Pull};
use crate::family::{FamilySpec, PinSpec, STM32F1_SPEC};
use crate::pin::{PinId, Port};

/// F1 端口集合（GPIOA/GPIOB/GPIOC）。
#[derive(Debug)]
pub struct F1Ports {
    /// GPIOA（PA0..PA15）。
    pub gpioa: pac::GPIOA,
    /// GPIOB（PB0..PB15）。
    pub gpiob: pac::GPIOB,
    /// GPIOC（PC13..PC15）。
    pub gpioc: pac::GPIOC,
}

/// STM32F103C8（F1）家族标记。
#[derive(Debug)]
pub struct F1;

/// 输入浮空：CNF=01、MODE=00。
const CR_INPUT_FLOATING: u32 = 0b01_00;
/// 输入上/下拉：CNF=10、MODE=00；由 ODR 位选择上拉（1）/下拉（0）。
const CR_INPUT_PULL: u32 = 0b10_00;
/// 推挽输出 2MHz：CNF=00、MODE=10。
const CR_OUTPUT_PUSH_PULL: u32 = 0b00_10;
/// 开漏输出 2MHz：CNF=01、MODE=10。
const CR_OUTPUT_OPEN_DRAIN: u32 = 0b01_10;

/// APB2ENR 中 GPIOA/B/C 时钟位（IOPAEN=bit2、IOPBEN=bit3、IOPCEN=bit4）。
const APB2_GPIO_CLOCKS: u32 = (1 << 2) | (1 << 3) | (1 << 4);

impl Stm32Family for F1 {
    const SPEC: &'static FamilySpec = &STM32F1_SPEC;
    type Ports = F1Ports;
    type Rcc = pac::RCC;

    fn enable_clocks(rcc: &mut pac::RCC, _ports: &mut F1Ports) {
        // 只置位 GPIOA/B/C 时钟，保留板级已使能的其他 APB2 外设位。
        rcc.apb2enr()
            .modify(|r, w| unsafe { w.bits(r.bits() | APB2_GPIO_CLOCKS) });
    }

    fn configure(ports: &mut F1Ports, spec: &'static PinSpec, config: PinConfig) {
        let PinId::Port { port, pin } = spec.id else {
            return;
        };
        let Some(gpio) = port_ref(ports, port) else {
            return;
        };
        if config.direction == Direction::Output {
            // 无毛刺顺序：关闭输出 → 写 ODR 初始电平 → 启用输出。
            write_cr(gpio, pin, CR_INPUT_FLOATING);
            write_bsrr(gpio, pin, config.level);
            write_cr(gpio, pin, cr_bits(config));
        } else {
            // 输入：先写 CRL/CRH（浮空/上下拉），再用 ODR 位选择上拉或下拉。
            write_cr(gpio, pin, cr_bits(config));
            let pull_level = match config.pull {
                Pull::Up => Level::High,
                Pull::None | Pull::Down => Level::Low,
            };
            write_bsrr(gpio, pin, pull_level);
        }
    }

    fn read(ports: &F1Ports, spec: &'static PinSpec) -> Level {
        let PinId::Port { port, pin } = spec.id else {
            return Level::Low;
        };
        let Some(gpio) = port_ref(ports, port) else {
            return Level::Low;
        };
        Level::from_bool(gpio.idr().read().bits() & (1u32 << pin) != 0)
    }

    fn write(ports: &mut F1Ports, spec: &'static PinSpec, level: Level) {
        let PinId::Port { port, pin } = spec.id else {
            return;
        };
        let Some(gpio) = port_ref(ports, port) else {
            return;
        };
        write_bsrr(gpio, pin, level);
    }
}

/// 取指定端口的寄存器块；F1 的 GPIOA/B/C 共用 `gpioa::RegisterBlock` 类型。
fn port_ref(ports: &F1Ports, port: Port) -> Option<&pac::gpioa::RegisterBlock> {
    match port {
        Port::A => Some(&ports.gpioa),
        Port::B => Some(&ports.gpiob),
        Port::C => Some(&ports.gpioc),
        _ => None,
    }
}

/// 写 CRL（pin 0..7）或 CRH（pin 8..15）中某个引脚的 4 位配置。
fn write_cr(gpio: &pac::gpioa::RegisterBlock, pin: u8, bits: u32) {
    let shift = cr_shift(pin);
    if use_crh(pin) {
        gpio.crh()
            .modify(|r, w| unsafe { w.bits(insert_bits(r.bits(), shift, 4, bits)) });
    } else {
        gpio.crl()
            .modify(|r, w| unsafe { w.bits(insert_bits(r.bits(), shift, 4, bits)) });
    }
}

/// 用 BSRR 原子地置位（高）或复位（低）某个 ODR 位。
fn write_bsrr(gpio: &pac::gpioa::RegisterBlock, pin: u8, level: Level) {
    gpio.bsrr()
        .write(|w| unsafe { w.bits(bsrr_bits(pin, level)) });
}

/// CRL/CRH 的每引脚 4 位编码：`CNF[1:0] MODE[1:0]`。
fn cr_bits(config: PinConfig) -> u32 {
    match config.direction {
        Direction::Input => match config.pull {
            Pull::None => CR_INPUT_FLOATING,
            Pull::Up | Pull::Down => CR_INPUT_PULL,
        },
        Direction::Output if config.open_drain => CR_OUTPUT_OPEN_DRAIN,
        Direction::Output => CR_OUTPUT_PUSH_PULL,
    }
}

/// 每引脚 4 位在寄存器内的偏移：pin 0..7 用 CRL，8..15 用 CRH。
fn cr_shift(pin: u8) -> u32 {
    u32::from(pin % 8) * 4
}

/// pin 8..15 使用 CRH，其余使用 CRL。
fn use_crh(pin: u8) -> bool {
    pin >= 8
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
    use super::{F1, bsrr_bits, cr_bits, cr_shift, use_crh};
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
    fn spec_matches_f103c8_table() {
        assert_eq!(F1::SPEC.chip, "stm32f1");
        assert_eq!(F1::SPEC.pins.len(), 35);
        assert_eq!(F1::SPEC.slots, 48);
        assert_eq!(F1::SPEC.ports, &[Port::A, Port::B, Port::C]);
        assert!(core::ptr::eq(F1::SPEC, ChipFamily::Stm32F1.spec()));
        // 表内只有 PA0-PA15、PB0-PB15、PC13-PC15。
        assert!(F1::SPEC.by_id(PinId::port(Port::A, 0)).is_some());
        assert!(F1::SPEC.by_id(PinId::port(Port::B, 15)).is_some());
        assert!(F1::SPEC.by_id(PinId::port(Port::C, 13)).is_some());
        assert!(F1::SPEC.by_id(PinId::port(Port::C, 0)).is_none());
        assert!(F1::SPEC.by_id(PinId::port(Port::D, 0)).is_none());
        for (port, pin) in [
            (Port::A, 13),
            (Port::A, 14),
            (Port::A, 15),
            (Port::B, 3),
            (Port::B, 4),
        ] {
            let spec = F1::SPEC.by_id(PinId::port(port, pin)).unwrap();
            assert!(spec.flags.reserved, "P{port}{pin} 应保留");
        }
    }

    #[test]
    fn slots_are_port_16_plus_pin() {
        for spec in F1::SPEC.pins {
            let PinId::Port { port, pin } = spec.id else {
                panic!("F1 表出现 ESP32 引脚");
            };
            assert_eq!(spec.slot, u16::from(port.index()) * 16 + u16::from(pin));
        }
    }

    #[test]
    fn cr_encoding_matches_reference_manual() {
        // 输入浮空 01_00。
        assert_eq!(cr_bits(input(NoPull)), 0b01_00);
        // 输入上/下拉 10_00（ODR 位另行选择）。
        assert_eq!(cr_bits(input(Up)), 0b10_00);
        assert_eq!(cr_bits(input(Down)), 0b10_00);
        // 推挽输出 2MHz 00_10。
        assert_eq!(cr_bits(output(High, false)), 0b00_10);
        // 开漏输出 2MHz 01_10。
        assert_eq!(cr_bits(output(Low, true)), 0b01_10);
    }

    #[test]
    fn cr_shift_and_register_selection() {
        assert_eq!(cr_shift(0), 0);
        assert_eq!(cr_shift(7), 28);
        assert_eq!(cr_shift(8), 0);
        assert_eq!(cr_shift(15), 28);
        assert!(!use_crh(0));
        assert!(!use_crh(7));
        assert!(use_crh(8));
        assert!(use_crh(15));
    }

    #[test]
    fn bsrr_uses_set_and_reset_halves() {
        assert_eq!(bsrr_bits(0, High), 1);
        assert_eq!(bsrr_bits(5, High), 1 << 5);
        assert_eq!(bsrr_bits(15, High), 1 << 15);
        assert_eq!(bsrr_bits(0, Low), 1 << 16);
        assert_eq!(bsrr_bits(5, Low), 1 << (5 + 16));
        assert_eq!(bsrr_bits(15, Low), 1 << (15 + 16));
        // 置位与复位位不重叠。
        assert_eq!(bsrr_bits(5, High) & bsrr_bits(5, Low), 0);
    }
}
