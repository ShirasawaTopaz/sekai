//! 宿主虚拟 GPIO 后端。
//!
//! 用于没有硬件时的完整端到端验证：AI 可以通过 MQTT ToolCall 操作它，
//! 观察与真实硬件一致的保留/能力/模式语义。虚拟后端只保存状态，不触碰硬件。
//!
//! 状态语义（计划 §4.6/§13）：
//! - 未配置引脚：read/write 返回 NotConfigured，describe 的 config/level 为 None；
//! - 输入模式：read 返回外部模拟电平（默认 Low），write 返回 WrongMode；
//! - 输出模式：write 更新输出锁存，describe 返回锁存电平，read 返回 WrongMode；
//! - 外部电平独立于配置保存：输出模式下设置它不影响 read/describe，
//!   切回输入模式后立即生效；
//! - 保留/不存在/仅输入引脚的拒绝语义与真实后端一致：先校验、后改状态。

use alloc::format;
use alloc::vec;
use alloc::vec::Vec;

use crate::config::{Direction, Level, PinConfig, validate_config};
use crate::family::{ChipFamily, FamilySpec, PinSpec};
use crate::pin::PinId;
use crate::port::{
    BackendKind, GpioError, GpioPort, PinDescription, ensure_input_mode, ensure_operable,
    ensure_output_mode, lookup,
};

/// 单个虚拟引脚的状态。
#[derive(Debug, Clone, Copy, Default)]
struct VirtualPinState {
    /// 已应用的配置；None 表示未配置。
    config: Option<PinConfig>,
    /// 输出锁存电平（write 或 configure 的输出初始电平更新它）。
    output_level: Level,
    /// 外部模拟输入电平（set_external_level 更新它，只在输入模式被读取）。
    external_level: Level,
}

/// 虚拟 GPIO 端口。
///
/// 所有校验（保留/能力/模式）与真实后端共用相同的辅助函数，保证 AI 在宿主上
/// 学到的行为与硬件一致。
#[derive(Debug)]
pub struct VirtualGpio {
    family: ChipFamily,
    spec: &'static FamilySpec,
    pins: Vec<VirtualPinState>,
}

impl VirtualGpio {
    /// 用指定家族的引脚表创建虚拟后端。
    pub fn new(family: ChipFamily) -> Self {
        let spec = family.spec();
        // 引脚表与状态数组长度的一致性由家族表保证；这里在调试构建中守护该不变量。
        debug_assert!(
            spec.pins
                .iter()
                .all(|pin| usize::from(pin.slot) < spec.slots),
            "{} 引脚表存在越界槽位",
            spec.chip
        );
        Self {
            family,
            spec,
            pins: vec![VirtualPinState::default(); spec.slots],
        }
    }

    /// 当前家族。
    pub fn family(&self) -> ChipFamily {
        self.family
    }

    /// 设置引脚的模拟外部输入电平。
    ///
    /// 与真实硬件一致：引脚必须存在于当前家族表中，保留引脚拒绝。该状态独立
    /// 于配置保存；输出模式下设置不会改变 describe/read 的结果，切回输入模式
    /// 后立即生效。
    pub fn set_external_level(&mut self, pin: PinId, level: Level) -> Result<(), GpioError> {
        let spec = lookup(self.spec, pin)?;
        ensure_operable(spec)?;
        self.state_mut(spec)?.external_level = level;
        Ok(())
    }

    /// 读取引脚的模拟外部输入电平。
    ///
    /// 测试/调试辅助接口：只做引脚存在性检查，**不**检查配置、方向或保留状态。
    pub fn external_level(&self, pin: PinId) -> Result<Level, GpioError> {
        let spec = lookup(self.spec, pin)?;
        Ok(self.state(spec)?.external_level)
    }

    /// 取得槽位状态；家族表损坏（槽位越界）时返回 Internal 错误而不是 panic。
    fn state(&self, spec: &'static PinSpec) -> Result<&VirtualPinState, GpioError> {
        self.pins
            .get(usize::from(spec.slot))
            .ok_or_else(|| slot_out_of_range(spec))
    }

    /// 取得可变的槽位状态；家族表损坏（槽位越界）时返回 Internal 错误。
    fn state_mut(&mut self, spec: &'static PinSpec) -> Result<&mut VirtualPinState, GpioError> {
        self.pins
            .get_mut(usize::from(spec.slot))
            .ok_or_else(|| slot_out_of_range(spec))
    }
}

/// 槽位越界属于后端内部错误（协议错误码 Internal）。
fn slot_out_of_range(spec: &'static PinSpec) -> GpioError {
    GpioError::backend(format!(
        "GPIO 后端错误：引脚 {} 的槽位 {} 越过状态数组",
        spec.id, spec.slot
    ))
}

impl GpioPort for VirtualGpio {
    fn spec(&self) -> &'static FamilySpec {
        self.spec
    }

    fn backend(&self) -> BackendKind {
        BackendKind::Virtual
    }

    fn configure(&mut self, pin: PinId, config: PinConfig) -> Result<(), GpioError> {
        let spec = lookup(self.spec, pin)?;
        // 防御性校验：保留/能力不满足时绝不修改状态（§16 不变量 4）。
        validate_config(spec, &config)?;
        let state = self.state_mut(spec)?;
        state.config = Some(config);
        // 输出锁存取配置中的初始电平；输入配置的 level 恒为 Low（由 resolve_config 保证）。
        state.output_level = config.level;
        Ok(())
    }

    fn read(&mut self, pin: PinId) -> Result<Level, GpioError> {
        let spec = lookup(self.spec, pin)?;
        let state = self.state(spec)?;
        ensure_input_mode(spec, state.config)?;
        Ok(state.external_level)
    }

    fn write(&mut self, pin: PinId, level: Level) -> Result<(), GpioError> {
        let spec = lookup(self.spec, pin)?;
        let state = self.state_mut(spec)?;
        ensure_output_mode(spec, state.config)?;
        state.output_level = level;
        Ok(())
    }

    fn describe(&mut self, pin: PinId) -> Result<PinDescription, GpioError> {
        let spec = lookup(self.spec, pin)?;
        let state = self.state(spec)?;
        // 输入模式取外部电平，输出模式取锁存电平；未配置为 None。
        let level = state.config.map(|config| match config.direction {
            Direction::Output => state.output_level,
            Direction::Input => state.external_level,
        });
        Ok(PinDescription {
            spec,
            config: state.config,
            level,
        })
    }

    fn configured_count(&self) -> usize {
        self.pins
            .iter()
            .filter(|state| state.config.is_some())
            .count()
    }
}

#[cfg(test)]
mod tests {
    use super::VirtualGpio;
    use crate::config::{ConfigRequest, Direction, Level, PinConfig, Pull, resolve_config};
    use crate::family::ChipFamily;
    use crate::pin::{PinId, Port};
    use crate::port::{BackendKind, GpioError, GpioPort};

    fn configure(port: &mut VirtualGpio, pin: PinId, request: ConfigRequest) {
        let spec = port.spec().by_id(pin).unwrap();
        let current = port.describe(pin).unwrap().config;
        let config = resolve_config(spec, current, &request).unwrap();
        port.configure(pin, config).unwrap();
    }

    #[test]
    fn output_write_then_describe() {
        let mut port = VirtualGpio::new(ChipFamily::Esp32);
        assert_eq!(port.backend(), BackendKind::Virtual);
        assert_eq!(port.configured_count(), 0);
        configure(
            &mut port,
            PinId::io(4),
            ConfigRequest {
                direction: Some(Direction::Output),
                level: Some(Level::Low),
                ..ConfigRequest::new()
            },
        );
        port.write(PinId::io(4), Level::High).unwrap();
        let description = port.describe(PinId::io(4)).unwrap();
        assert_eq!(description.config.unwrap().direction, Direction::Output);
        assert_eq!(description.level, Some(Level::High));
        assert_eq!(port.configured_count(), 1);
    }

    #[test]
    fn input_reads_external_level() {
        let mut port = VirtualGpio::new(ChipFamily::Esp32);
        port.set_external_level(PinId::io(34), Level::High).unwrap();
        configure(
            &mut port,
            PinId::io(34),
            ConfigRequest {
                direction: Some(Direction::Input),
                ..ConfigRequest::new()
            },
        );
        assert_eq!(port.read(PinId::io(34)).unwrap(), Level::High);
        assert_eq!(
            port.describe(PinId::io(34)).unwrap().level,
            Some(Level::High)
        );
    }

    #[test]
    fn output_read_and_input_write_are_mode_errors() {
        let mut port = VirtualGpio::new(ChipFamily::Esp32);
        let output = PinConfig {
            direction: Direction::Output,
            ..PinConfig::default()
        };
        port.configure(PinId::io(4), output).unwrap();
        assert!(matches!(
            port.read(PinId::io(4)),
            Err(GpioError::WrongMode(_))
        ));

        let input = PinConfig {
            direction: Direction::Input,
            ..PinConfig::default()
        };
        port.configure(PinId::io(5), input).unwrap();
        assert!(matches!(
            port.write(PinId::io(5), Level::High),
            Err(GpioError::WrongMode(_))
        ));
    }

    #[test]
    fn unconfigured_read_write_are_errors() {
        let mut port = VirtualGpio::new(ChipFamily::Esp32);
        assert!(matches!(
            port.read(PinId::io(4)),
            Err(GpioError::NotConfigured(_))
        ));
        assert!(matches!(
            port.write(PinId::io(4), Level::High),
            Err(GpioError::NotConfigured(_))
        ));
        assert_eq!(port.describe(PinId::io(4)).unwrap().config, None);
        assert_eq!(port.describe(PinId::io(4)).unwrap().level, None);
    }

    #[test]
    fn reserved_and_input_only_are_rejected_before_hardware() {
        let mut port = VirtualGpio::new(ChipFamily::Esp32);
        assert!(matches!(
            port.configure(
                PinId::io(6),
                PinConfig {
                    direction: Direction::Output,
                    ..PinConfig::default()
                }
            ),
            Err(GpioError::ReservedPin(_))
        ));
        assert!(matches!(
            port.set_external_level(PinId::io(6), Level::High),
            Err(GpioError::ReservedPin(_))
        ));
        assert!(matches!(
            port.configure(
                PinId::io(34),
                PinConfig {
                    direction: Direction::Output,
                    ..PinConfig::default()
                }
            ),
            Err(GpioError::Unsupported(_))
        ));
    }

    #[test]
    fn stm32_families_use_their_own_tables() {
        for family in [ChipFamily::Stm32F1, ChipFamily::Stm32F4] {
            let mut port = VirtualGpio::new(family);
            let pa5 = PinId::port(Port::A, 5);
            let config = PinConfig {
                direction: Direction::Output,
                level: Level::High,
                ..PinConfig::default()
            };
            port.configure(pa5, config).unwrap();
            port.write(pa5, Level::Low).unwrap();
            assert!(matches!(
                port.configure(
                    PinId::port(Port::B, 3),
                    PinConfig {
                        direction: Direction::Output,
                        ..PinConfig::default()
                    }
                ),
                Err(GpioError::ReservedPin(_))
            ));
            // ESP32 引脚不属于 STM32 家族。
            assert!(matches!(
                port.describe(PinId::io(4)),
                Err(GpioError::UnknownPin(_))
            ));
        }
        let mut f1 = VirtualGpio::new(ChipFamily::Stm32F1);
        assert!(matches!(
            f1.describe(PinId::port(Port::C, 0)),
            Err(GpioError::UnknownPin(_))
        ));
        let mut f4 = VirtualGpio::new(ChipFamily::Stm32F4);
        assert!(f4.describe(PinId::port(Port::E, 0)).is_ok());
    }

    #[test]
    fn pull_configuration_is_preserved() {
        let mut port = VirtualGpio::new(ChipFamily::Esp32);
        configure(
            &mut port,
            PinId::io(4),
            ConfigRequest {
                direction: Some(Direction::Input),
                pull: Some(Pull::Up),
                ..ConfigRequest::new()
            },
        );
        let description = port.describe(PinId::io(4)).unwrap();
        assert_eq!(description.config.unwrap().pull, Pull::Up);
    }

    #[test]
    fn external_level_is_ignored_while_output() {
        let mut port = VirtualGpio::new(ChipFamily::Esp32);
        configure(
            &mut port,
            PinId::io(4),
            ConfigRequest {
                direction: Some(Direction::Output),
                level: Some(Level::High),
                ..ConfigRequest::new()
            },
        );
        port.write(PinId::io(4), Level::High).unwrap();
        // 输出模式：外部电平只是保存，不影响 describe 的锁存值。
        port.set_external_level(PinId::io(4), Level::Low).unwrap();
        assert_eq!(port.external_level(PinId::io(4)).unwrap(), Level::Low);
        assert_eq!(
            port.describe(PinId::io(4)).unwrap().level,
            Some(Level::High)
        );
        // 切回输入后外部电平生效。
        configure(
            &mut port,
            PinId::io(4),
            ConfigRequest {
                direction: Some(Direction::Input),
                ..ConfigRequest::new()
            },
        );
        assert_eq!(port.read(PinId::io(4)).unwrap(), Level::Low);
    }

    #[test]
    fn every_family_table_fits_its_slot_array() {
        for family in [ChipFamily::Esp32, ChipFamily::Stm32F1, ChipFamily::Stm32F4] {
            let port = VirtualGpio::new(family);
            assert_eq!(port.pins.len(), family.spec().slots);
            for pin in family.spec().pins {
                assert!(usize::from(pin.slot) < port.pins.len());
            }
        }
    }

    #[test]
    fn reconfiguring_does_not_double_count() {
        let mut port = VirtualGpio::new(ChipFamily::Esp32);
        configure(
            &mut port,
            PinId::io(4),
            ConfigRequest {
                direction: Some(Direction::Input),
                ..ConfigRequest::new()
            },
        );
        assert_eq!(port.configured_count(), 1);
        configure(
            &mut port,
            PinId::io(4),
            ConfigRequest {
                direction: Some(Direction::Output),
                ..ConfigRequest::new()
            },
        );
        assert_eq!(port.configured_count(), 1);
    }
}
