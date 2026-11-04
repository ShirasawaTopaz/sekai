//! STM32F1/F4 真实 GPIO 后端（直接使用 PAC 寄存器）。
//!
//! 家族差异（端口类型、RCC 时钟位、GPIO 寄存器布局）封装在 `Stm32Family`，
//! 具体实现在 `f1` / `f4`（`stm32f1` / `stm32f4` 特性）；本模块只保留与寄存器
//! 无关的安全状态机：
//!
//! - `Stm32Gpio` 保存每个引脚最近一次成功下发的 `PinConfig`；
//! - 所有操作先查引脚表（`lookup`），再做配置/能力/模式校验
//!   （`validate_config`、`ensure_input_mode`、`ensure_output_mode`）；
//! - 校验失败直接返回错误，不调用家族寄存器函数，因此不会触碰硬件。
//!
//! `unsafe` 只出现在 `f1.rs` / `f4.rs` 的寄存器读改写；本模块是安全代码。
//! 真实寄存器路径只做交叉编译检查，硬件行为需要板级验证（计划 `7.7）。
//!
//! 构造方式见计划 `7.6：
//!
//! ```text
//! let mut gpio = Stm32Gpio::<f1::F1>::new(
//!     f1::F1Ports { gpioa: dp.GPIOA, gpiob: dp.GPIOB, gpioc: dp.GPIOC },
//!     &mut dp.RCC,
//! );
//! ```

#[cfg(feature = "stm32f1")]
pub mod f1;

#[cfg(feature = "stm32f4")]
pub mod f4;

use alloc::vec;
use alloc::vec::Vec;
use core::fmt;

use crate::config::{Direction, Level, PinConfig, validate_config};
use crate::family::{FamilySpec, PinSpec};
use crate::pin::PinId;
use crate::port::{
    GpioError, GpioPort, PinDescription, ensure_input_mode, ensure_output_mode, lookup,
};

/// STM32 家族差异：引脚表、端口集合、RCC 与寄存器访问。
///
/// 实现者负责真正的寄存器读写；`Stm32Gpio` 保证只有通过统一校验的操作
/// 才会调用这些函数，因此实现者不需要重复做保留/能力检查。
pub trait Stm32Family {
    /// 家族引脚表。
    const SPEC: &'static FamilySpec;
    /// GPIO 端口外设集合（F1：`F1Ports`；F4：`F4Ports`）。
    type Ports;
    /// RCC 外设。
    type Rcc;

    /// 使能该家族用到的 GPIO 端口时钟（读改写，幂等）。
    fn enable_clocks(rcc: &mut Self::Rcc, ports: &mut Self::Ports);

    /// 把一份**已校验**的配置写入寄存器。
    fn configure(ports: &mut Self::Ports, spec: &'static PinSpec, config: PinConfig);

    /// 读取引脚输入电平（IDR）。
    fn read(ports: &Self::Ports, spec: &'static PinSpec) -> Level;

    /// 写引脚输出电平（BSRR，置位/复位各占一半）。
    fn write(ports: &mut Self::Ports, spec: &'static PinSpec, level: Level);
}

/// 家族无关的 STM32 GPIO 后端。
///
/// 状态机只保存配置；硬件状态由寄存器保存。输出引脚的 `level` 记录最近一次
/// `configure`/`write` 写入 ODR 的值（与寄存器锁存值一致），`describe` 直接
/// 返回它，避免为读回输出锁存而增加额外的家族接口。
pub struct Stm32Gpio<F: Stm32Family> {
    ports: F::Ports,
    /// 每个槽位的当前配置；长度等于 `F::SPEC.slots`，未配置为 `None`。
    configs: Vec<Option<PinConfig>>,
}

impl<F: Stm32Family> Stm32Gpio<F> {
    /// 创建后端并使能 GPIO 时钟；`ports` 的所有权移交给后端。
    ///
    /// 板级工程必须保证 `ports` 中的外设 token 此后不再被其他代码使用。
    pub fn new(ports: F::Ports, rcc: &mut F::Rcc) -> Self {
        let mut ports = ports;
        F::enable_clocks(rcc, &mut ports);
        Self {
            ports,
            configs: vec![None; F::SPEC.slots],
        }
    }

    /// 端口外设引用（板级需要直接操作端口寄存器时使用）。
    pub fn ports(&self) -> &F::Ports {
        &self.ports
    }

    /// 应用一份完整配置；校验失败不会触碰寄存器。
    pub fn configure(&mut self, pin: PinId, config: PinConfig) -> Result<(), GpioError> {
        let spec = lookup(F::SPEC, pin)?;
        // 保留/能力检查在触碰硬件之前完成。
        validate_config(spec, &config)?;
        F::configure(&mut self.ports, spec, config);
        self.configs[usize::from(spec.slot)] = Some(config);
        Ok(())
    }

    /// 读取已配置为输入的引脚电平。
    pub fn read(&mut self, pin: PinId) -> Result<Level, GpioError> {
        let spec = lookup(F::SPEC, pin)?;
        ensure_input_mode(spec, self.configs[usize::from(spec.slot)])?;
        Ok(F::read(&self.ports, spec))
    }

    /// 写入已配置为输出的引脚电平。
    pub fn write(&mut self, pin: PinId, level: Level) -> Result<(), GpioError> {
        let spec = lookup(F::SPEC, pin)?;
        let slot = usize::from(spec.slot);
        let mut config = ensure_output_mode(spec, self.configs[slot])?;
        F::write(&mut self.ports, spec, level);
        // 记录最新 ODR 锁存值，供 gpio.info 的 describe 使用。
        config.level = level;
        self.configs[slot] = Some(config);
        Ok(())
    }

    /// 读取引脚描述；未配置/保留引脚也允许（`gpio.info` 使用）。
    pub fn describe(&mut self, pin: PinId) -> Result<PinDescription, GpioError> {
        let spec = lookup(F::SPEC, pin)?;
        let config = self.configs[usize::from(spec.slot)];
        let level = config.map(|config| match config.direction {
            Direction::Input => F::read(&self.ports, spec),
            Direction::Output => config.level,
        });
        Ok(PinDescription {
            spec,
            config,
            level,
        })
    }

    /// 已配置引脚数量。
    pub fn configured_count(&self) -> usize {
        self.configs
            .iter()
            .filter(|config| config.is_some())
            .count()
    }
}

impl<F: Stm32Family> GpioPort for Stm32Gpio<F> {
    fn spec(&self) -> &'static FamilySpec {
        F::SPEC
    }

    fn configure(&mut self, pin: PinId, config: PinConfig) -> Result<(), GpioError> {
        Stm32Gpio::configure(self, pin, config)
    }

    fn read(&mut self, pin: PinId) -> Result<Level, GpioError> {
        Stm32Gpio::read(self, pin)
    }

    fn write(&mut self, pin: PinId, level: Level) -> Result<(), GpioError> {
        Stm32Gpio::write(self, pin, level)
    }

    fn describe(&mut self, pin: PinId) -> Result<PinDescription, GpioError> {
        Stm32Gpio::describe(self, pin)
    }

    fn configured_count(&self) -> usize {
        Stm32Gpio::configured_count(self)
    }
}

impl<F: Stm32Family> fmt::Debug for Stm32Gpio<F> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Stm32Gpio")
            .field("chip", &F::SPEC.chip)
            .field("configured", &self.configured_count())
            .finish()
    }
}

/// 把 `width` 位宽的 `value` 写进 `current` 的 `shift` 位段，其余位保持。
///
/// F1 的 CRL/CRH（每引脚 4 bit）与 F4 的 MODER/PUPDR/OSPEEDR/OTYPER
/// （每引脚 1~2 bit）共用这个纯函数，把位运算从带 `unsafe` 的寄存器闭包里
/// 拿出来，便于宿主单元测试。
pub(crate) const fn insert_bits(current: u32, shift: u32, width: u32, value: u32) -> u32 {
    let mask = ((1u32 << width) - 1) << shift;
    (current & !mask) | ((value << shift) & mask)
}

#[cfg(test)]
mod tests {
    use super::{Stm32Family, Stm32Gpio, insert_bits};
    use crate::config::{Direction, Level, PinConfig, Pull};
    use crate::family::{FamilyKind, FamilySpec, PinCaps, PinFlags, PinSpec};
    use crate::pin::{PinId, Port};
    use crate::port::{BackendKind, GpioError, GpioPort};
    use alloc::rc::Rc;
    use core::cell::RefCell;

    use Direction::{Input, Output};
    use Level::{High, Low};
    use Pull::{Down, None as NoPull, Up};

    /// mock 端口数量（A/B/C）。
    const PORT_COUNT: usize = 3;
    /// mock 时钟位：三个端口各一位。
    const CLOCK_BITS: u32 = 0b111;

    /// mock 引脚表：覆盖普通双向、上下拉、输入专用与保留引脚。
    static MOCK_PINS: &[PinSpec] = &[
        PinSpec {
            id: PinId::port(Port::A, 5),
            slot: 5,
            caps: PinCaps::ALL,
            flags: PinFlags::NONE,
            note: "",
        },
        PinSpec {
            id: PinId::port(Port::B, 2),
            slot: 18,
            caps: PinCaps::ALL,
            flags: PinFlags::NONE,
            note: "",
        },
        PinSpec {
            id: PinId::port(Port::C, 13),
            slot: 45,
            caps: PinCaps::INPUT_ONLY,
            flags: PinFlags::NONE.with_input_only(),
            note: "",
        },
        PinSpec {
            id: PinId::port(Port::A, 13),
            slot: 13,
            caps: PinCaps::NONE,
            flags: PinFlags::RESERVED.with_swd(),
            note: "SWD",
        },
    ];

    static MOCK_SPEC: FamilySpec = FamilySpec {
        family: FamilyKind::Stm32,
        chip: "stm32-mock",
        pins: MOCK_PINS,
        slots: 48,
        ports: &[Port::A, Port::B, Port::C],
    };

    /// mock 寄存器组；每次配置/写/读都累加计数，用于断言“校验失败不触碰硬件”。
    #[derive(Debug, Clone, PartialEq, Eq)]
    struct MockRegs {
        clock_writes: usize,
        clocks: u32,
        moder: [u32; PORT_COUNT],
        otyper: [u32; PORT_COUNT],
        ospeedr: [u32; PORT_COUNT],
        pupdr: [u32; PORT_COUNT],
        odr: [u32; PORT_COUNT],
        idr: [u32; PORT_COUNT],
        configure_writes: usize,
        bsrr_writes: usize,
        idr_reads: usize,
        last_bsrr: u32,
    }

    impl Default for MockRegs {
        fn default() -> Self {
            Self {
                clock_writes: 0,
                clocks: 0,
                moder: [0; PORT_COUNT],
                otyper: [0; PORT_COUNT],
                ospeedr: [0; PORT_COUNT],
                pupdr: [0; PORT_COUNT],
                odr: [0; PORT_COUNT],
                idr: [0; PORT_COUNT],
                configure_writes: 0,
                bsrr_writes: 0,
                idr_reads: 0,
                last_bsrr: 0,
            }
        }
    }

    /// mock 端口集合：与真实后端一样由 `Stm32Gpio` 持有，测试侧通过 `Rc` 观察。
    #[derive(Debug, Clone)]
    struct MockPorts {
        regs: Rc<RefCell<MockRegs>>,
    }

    /// mock RCC：记录时钟掩码。
    type MockRcc = u32;

    struct MockFamily;

    fn mock_port_pin(id: PinId) -> Option<(usize, u8)> {
        let PinId::Port { port, pin } = id else {
            return None;
        };
        let index = match port {
            Port::A => 0,
            Port::B => 1,
            Port::C => 2,
            _ => return None,
        };
        Some((index, pin))
    }

    impl Stm32Family for MockFamily {
        const SPEC: &'static FamilySpec = &MOCK_SPEC;
        type Ports = MockPorts;
        type Rcc = MockRcc;

        fn enable_clocks(rcc: &mut MockRcc, ports: &mut MockPorts) {
            *rcc |= CLOCK_BITS;
            let mut regs = ports.regs.borrow_mut();
            regs.clock_writes += 1;
            regs.clocks |= CLOCK_BITS;
        }

        fn configure(ports: &mut MockPorts, spec: &'static PinSpec, config: PinConfig) {
            let Some((port, pin)) = mock_port_pin(spec.id) else {
                return;
            };
            let mut regs = ports.regs.borrow_mut();
            let shift = u32::from(pin) * 2;
            let is_output = u32::from(config.direction == Output);
            regs.moder[port] = insert_bits(regs.moder[port], shift, 2, is_output);
            regs.otyper[port] = insert_bits(
                regs.otyper[port],
                u32::from(pin),
                1,
                u32::from(config.open_drain),
            );
            regs.ospeedr[port] = insert_bits(regs.ospeedr[port], shift, 2, is_output);
            let pull = match config.pull {
                NoPull => 0,
                Up => 1,
                Down => 2,
            };
            regs.pupdr[port] = insert_bits(regs.pupdr[port], shift, 2, pull);
            // ODR：输出保存锁存电平；输入保存上下拉选择位（与 F1 的 ODR 位一致）。
            let latch = match config.direction {
                Output => config.level == High,
                Input => config.pull == Up,
            };
            regs.odr[port] = insert_bits(regs.odr[port], u32::from(pin), 1, u32::from(latch));
            regs.configure_writes += 1;
        }

        fn read(ports: &MockPorts, spec: &'static PinSpec) -> Level {
            let Some((port, pin)) = mock_port_pin(spec.id) else {
                return Level::Low;
            };
            let mut regs = ports.regs.borrow_mut();
            regs.idr_reads += 1;
            Level::from_bool(regs.idr[port] & (1u32 << pin) != 0)
        }

        fn write(ports: &mut MockPorts, spec: &'static PinSpec, level: Level) {
            let Some((port, pin)) = mock_port_pin(spec.id) else {
                return;
            };
            let mut regs = ports.regs.borrow_mut();
            let bit = 1u32 << pin;
            match level {
                High => {
                    regs.odr[port] |= bit;
                    regs.last_bsrr = bit;
                }
                Low => {
                    regs.odr[port] &= !bit;
                    regs.last_bsrr = bit << 16;
                }
            }
            regs.bsrr_writes += 1;
        }
    }

    fn new_gpio() -> (Stm32Gpio<MockFamily>, Rc<RefCell<MockRegs>>, u32) {
        let regs = Rc::new(RefCell::new(MockRegs::default()));
        let ports = MockPorts {
            regs: Rc::clone(&regs),
        };
        let mut rcc: MockRcc = 0;
        let gpio = Stm32Gpio::<MockFamily>::new(ports, &mut rcc);
        (gpio, regs, rcc)
    }

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

    fn set_idr(regs: &Rc<RefCell<MockRegs>>, port: Port, pin: u8, level: Level) {
        let mut regs = regs.borrow_mut();
        let index = match port {
            Port::A => 0,
            Port::B => 1,
            Port::C => 2,
            _ => return,
        };
        let bit = 1u32 << pin;
        match level {
            High => regs.idr[index] |= bit,
            Low => regs.idr[index] &= !bit,
        }
    }

    fn field(value: u32, shift: u32, width: u32) -> u32 {
        (value >> shift) & ((1u32 << width) - 1)
    }

    const PA5: PinId = PinId::port(Port::A, 5);
    const PA13: PinId = PinId::port(Port::A, 13);
    const PB2: PinId = PinId::port(Port::B, 2);
    const PC13: PinId = PinId::port(Port::C, 13);

    #[test]
    fn insert_bits_clears_and_sets_field() {
        assert_eq!(insert_bits(0xFFFF_FFFF, 0, 4, 0b0100), 0xFFFF_FFF4);
        assert_eq!(insert_bits(0, 8, 4, 0b1010), 0b1010 << 8);
        assert_eq!(insert_bits(0b1111_0000, 4, 4, 0), 0);
        assert_eq!(insert_bits(0xFFFF_FFFF, 28, 4, 0), 0x0FFF_FFFF);
        assert_eq!(insert_bits(0, 10, 2, 0b10), 0b10 << 10);
    }

    #[test]
    fn new_enables_clocks_and_initializes_state() {
        let (gpio, regs, rcc) = new_gpio();
        assert_eq!(rcc, CLOCK_BITS);
        assert_eq!(gpio.configured_count(), 0);
        assert_eq!(gpio.backend(), BackendKind::Hardware);
        assert_eq!(gpio.spec().chip, "stm32-mock");
        {
            let regs = regs.borrow();
            assert_eq!(regs.clock_writes, 1);
            assert_eq!(regs.clocks, CLOCK_BITS);
            assert_eq!(regs.configure_writes, 0);
            assert_eq!(regs.bsrr_writes, 0);
        }
        assert_eq!(gpio.ports().regs.borrow().clocks, CLOCK_BITS);
    }

    #[test]
    fn configure_output_writes_registers_and_describes() {
        let (mut gpio, regs, _) = new_gpio();
        let config = output(High, false);
        gpio.configure(PA5, config).unwrap();
        assert_eq!(gpio.configured_count(), 1);
        {
            let regs = regs.borrow();
            assert_eq!(regs.configure_writes, 1);
            assert_eq!(field(regs.moder[0], 10, 2), 0b01);
            assert_eq!(field(regs.otyper[0], 5, 1), 0);
            assert_eq!(field(regs.ospeedr[0], 10, 2), 0b01);
            assert_eq!(field(regs.pupdr[0], 10, 2), 0);
            assert_eq!(field(regs.odr[0], 5, 1), 1);
            // 同一端口其它引脚的位不受影响。
            assert_eq!(regs.moder[0] & !(0b11 << 10), 0);
        }
        let description = gpio.describe(PA5).unwrap();
        assert_eq!(description.spec.id, PA5);
        assert_eq!(description.config, Some(config));
        assert_eq!(description.level, Some(High));
        // describe 输出引脚不读 IDR。
        assert_eq!(regs.borrow().idr_reads, 0);
    }

    #[test]
    fn open_drain_output_sets_otyper() {
        let (mut gpio, regs, _) = new_gpio();
        gpio.configure(PA5, output(Low, true)).unwrap();
        let regs = regs.borrow();
        assert_eq!(field(regs.otyper[0], 5, 1), 1);
        assert_eq!(field(regs.odr[0], 5, 1), 0);
        assert_eq!(field(regs.moder[0], 10, 2), 0b01);
    }

    #[test]
    fn write_updates_odr_and_bsrr_counters() {
        let (mut gpio, regs, _) = new_gpio();
        gpio.configure(PA5, output(Low, false)).unwrap();

        gpio.write(PA5, High).unwrap();
        {
            let regs = regs.borrow();
            assert_eq!(regs.bsrr_writes, 1);
            assert_eq!(regs.last_bsrr, 1 << 5);
            assert_eq!(field(regs.odr[0], 5, 1), 1);
        }
        assert_eq!(gpio.describe(PA5).unwrap().level, Some(High));

        gpio.write(PA5, Low).unwrap();
        {
            let regs = regs.borrow();
            assert_eq!(regs.bsrr_writes, 2);
            assert_eq!(regs.last_bsrr, 1 << (5 + 16));
            assert_eq!(field(regs.odr[0], 5, 1), 0);
        }
        assert_eq!(gpio.describe(PA5).unwrap().level, Some(Low));
    }

    #[test]
    fn input_pull_configuration_and_read() {
        let (mut gpio, regs, _) = new_gpio();
        set_idr(&regs, Port::B, 2, High);
        gpio.configure(PB2, input(Up)).unwrap();
        {
            let regs = regs.borrow();
            assert_eq!(regs.configure_writes, 1);
            assert_eq!(field(regs.moder[1], 4, 2), 0);
            assert_eq!(field(regs.pupdr[1], 4, 2), 0b01);
            assert_eq!(field(regs.odr[1], 2, 1), 1);
        }
        assert_eq!(gpio.read(PB2).unwrap(), High);
        assert_eq!(regs.borrow().idr_reads, 1);
        let description = gpio.describe(PB2).unwrap();
        assert_eq!(description.level, Some(High));
        assert_eq!(description.config.unwrap().pull, Up);

        // 切换为下拉：PUPDR=10，ODR 选择位清零。
        gpio.configure(PB2, input(Down)).unwrap();
        {
            let regs = regs.borrow();
            assert_eq!(field(regs.pupdr[1], 4, 2), 0b10);
            assert_eq!(field(regs.odr[1], 2, 1), 0);
            assert_eq!(regs.configure_writes, 2);
        }
        assert_eq!(gpio.read(PB2).unwrap(), High);
    }

    #[test]
    fn switching_input_to_output_clears_pull() {
        let (mut gpio, regs, _) = new_gpio();
        gpio.configure(PB2, input(Up)).unwrap();
        gpio.configure(PB2, output(Low, false)).unwrap();
        {
            let regs = regs.borrow();
            assert_eq!(field(regs.moder[1], 4, 2), 0b01);
            assert_eq!(field(regs.pupdr[1], 4, 2), 0);
            assert_eq!(field(regs.odr[1], 2, 1), 0);
            assert_eq!(regs.configure_writes, 2);
        }
        // 配置是覆盖式的，数量不重复累计。
        assert_eq!(gpio.configured_count(), 1);
    }

    #[test]
    fn reserved_pin_never_touches_registers() {
        let (mut gpio, regs, _) = new_gpio();
        assert!(matches!(
            gpio.configure(PA13, output(High, false)),
            Err(GpioError::ReservedPin(_))
        ));
        assert!(matches!(gpio.read(PA13), Err(GpioError::ReservedPin(_))));
        assert!(matches!(
            gpio.write(PA13, High),
            Err(GpioError::ReservedPin(_))
        ));
        // describe 允许保留引脚（gpio.info 需要展示它）。
        let description = gpio.describe(PA13).unwrap();
        assert_eq!(description.config, None);
        assert_eq!(description.level, None);

        let regs = regs.borrow();
        assert_eq!(regs.configure_writes, 0);
        assert_eq!(regs.bsrr_writes, 0);
        assert_eq!(regs.idr_reads, 0);
        assert_eq!(regs.moder, [0; PORT_COUNT]);
        assert_eq!(regs.odr, [0; PORT_COUNT]);
    }

    #[test]
    fn capability_error_never_touches_registers() {
        let (mut gpio, regs, _) = new_gpio();
        // PC13 仅支持输入。
        assert!(matches!(
            gpio.configure(PC13, output(Low, false)),
            Err(GpioError::Unsupported(_))
        ));
        {
            let regs = regs.borrow();
            assert_eq!(regs.configure_writes, 0);
            assert_eq!(regs.moder, [0; PORT_COUNT]);
        }
        // 输入专用引脚可以正常配置为输入。
        gpio.configure(PC13, input(NoPull)).unwrap();
        {
            let regs = regs.borrow();
            assert_eq!(regs.configure_writes, 1);
            assert_eq!(field(regs.moder[2], 26, 2), 0);
        }
        assert_eq!(gpio.configured_count(), 1);
    }

    #[test]
    fn mode_errors_never_touch_registers() {
        let (mut gpio, regs, _) = new_gpio();
        // 未配置：读写都返回 NotConfigured，硬件计数不变。
        assert!(matches!(gpio.read(PA5), Err(GpioError::NotConfigured(_))));
        assert!(matches!(
            gpio.write(PA5, High),
            Err(GpioError::NotConfigured(_))
        ));
        {
            let regs = regs.borrow();
            assert_eq!(regs.idr_reads, 0);
            assert_eq!(regs.bsrr_writes, 0);
        }

        // 输入模式下写：WrongMode，不写 BSRR。
        gpio.configure(PA5, input(NoPull)).unwrap();
        assert!(matches!(
            gpio.write(PA5, High),
            Err(GpioError::WrongMode(_))
        ));
        assert_eq!(regs.borrow().bsrr_writes, 0);

        // 输出模式下读：WrongMode，不读 IDR。
        gpio.configure(PB2, output(High, false)).unwrap();
        let idr_before = regs.borrow().idr_reads;
        assert!(matches!(gpio.read(PB2), Err(GpioError::WrongMode(_))));
        assert_eq!(regs.borrow().idr_reads, idr_before);
    }

    #[test]
    fn unknown_and_foreign_pins_are_rejected() {
        let (mut gpio, regs, _) = new_gpio();
        // 不在 mock 表内。
        assert!(matches!(
            gpio.describe(PinId::port(Port::A, 0)),
            Err(GpioError::UnknownPin(_))
        ));
        // 表外的 ESP32 引脚。
        assert!(matches!(
            gpio.configure(PinId::io(4), input(NoPull)),
            Err(GpioError::UnknownPin(_))
        ));
        assert_eq!(regs.borrow().configure_writes, 0);
    }

    #[test]
    fn configured_count_counts_distinct_pins() {
        let (mut gpio, _, _) = new_gpio();
        assert_eq!(gpio.configured_count(), 0);
        gpio.configure(PA5, output(High, false)).unwrap();
        gpio.configure(PB2, input(Up)).unwrap();
        gpio.configure(PC13, input(NoPull)).unwrap();
        assert_eq!(gpio.configured_count(), 3);
        gpio.configure(PA5, output(Low, false)).unwrap();
        assert_eq!(gpio.configured_count(), 3);
    }
}
