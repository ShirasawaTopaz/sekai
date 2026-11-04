//! 引脚配置状态机。
//!
//! 所有后端（ESP32、STM32、虚拟）共享同一套配置语义：[`ResolveConfig`] 参数合并、
//! 方向/电平/上下拉/开漏的合法性校验，以及能力/保留引脚检查。这样 AI 在
//! 宿主虚拟后端上学到的行为与真实硬件一致。

use alloc::format;

use crate::family::PinSpec;
use crate::port::GpioError;

/// 引脚电平。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Level {
    /// 低电平。
    #[default]
    Low,
    /// 高电平。
    High,
}

impl Level {
    /// 由布尔值构造：`true` 为高。
    pub const fn from_bool(value: bool) -> Self {
        if value { Self::High } else { Self::Low }
    }

    /// 转换为布尔值：高为 `true`。
    pub const fn as_bool(self) -> bool {
        matches!(self, Self::High)
    }

    /// 线上的 0/1 文本。
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Low => "0",
            Self::High => "1",
        }
    }
}

/// 引脚方向。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Direction {
    /// 输入。
    #[default]
    Input,
    /// 输出。
    Output,
}

impl Direction {
    /// 工具输出/文档使用的小写名称。
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Input => "input",
            Self::Output => "output",
        }
    }
}

/// 内部上下拉。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Pull {
    /// 无上下拉（浮空）。
    #[default]
    None,
    /// 上拉。
    Up,
    /// 下拉。
    Down,
}

impl Pull {
    /// 工具输出/文档使用的小写名称。
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Up => "up",
            Self::Down => "down",
        }
    }
}

/// 一个引脚的完整配置。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PinConfig {
    /// 方向。
    pub direction: Direction,
    /// 输出初始/锁存电平；输入模式下恒为 [`Level::Low`]。
    pub level: Level,
    /// 输入模式下生效的上下拉。
    pub pull: Pull,
    /// 输出开漏；输入模式下恒为 `false`。
    pub open_drain: bool,
}

impl Default for PinConfig {
    fn default() -> Self {
        Self {
            direction: Direction::Input,
            level: Level::Low,
            pull: Pull::None,
            open_drain: false,
        }
    }
}

/// `gpio.config` 的部分更新请求；`None` 表示沿用当前值。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ConfigRequest {
    /// 目标方向。
    pub direction: Option<Direction>,
    /// 目标/初始电平。
    pub level: Option<Level>,
    /// 目标上下拉。
    pub pull: Option<Pull>,
    /// 目标开漏设置。
    pub open_drain: Option<bool>,
}

impl ConfigRequest {
    /// 空请求。
    pub const fn new() -> Self {
        Self {
            direction: None,
            level: None,
            pull: None,
            open_drain: None,
        }
    }

    /// 是否所有字段都为 `None`。
    pub const fn is_empty(&self) -> bool {
        self.direction.is_none()
            && self.level.is_none()
            && self.pull.is_none()
            && self.open_drain.is_none()
    }
}

/// 合并部分更新并校验，得到最终配置。
///
/// 合并规则（与文档、测试保持一致）：
/// 1. 起始配置 = `current.unwrap_or_default()`；
/// 2. 显式切换为输入：保留/应用 `pull`，强制 `open_drain=false`、`level=Low`；
/// 3. 显式切换为输出：`pull` 强制为 `none`，应用 `level`/`open_drain`；
/// 4. 未给方向时沿用当前方向与其余字段；
/// 5. `level` 只允许在最终方向为输出时出现；
/// 6. `pull` 只允许在最终方向为输入时出现；
/// 7. `open_drain=true` 只允许在最终方向为输出时出现；
/// 8. 能力校验（输入/输出/上下拉/开漏）与保留引脚检查在最后统一执行。
pub fn resolve_config(
    spec: &'static PinSpec,
    current: Option<PinConfig>,
    request: &ConfigRequest,
) -> Result<PinConfig, GpioError> {
    // 保留引脚在任何操作前直接拒绝。
    ensure_not_reserved(spec)?;

    let start = current.unwrap_or_default();
    let mut config = start;

    if let Some(direction) = request.direction {
        match direction {
            Direction::Input => {
                config.direction = Direction::Input;
                config.level = Level::Low;
                config.open_drain = false;
                if let Some(pull) = request.pull {
                    config.pull = pull;
                }
            }
            Direction::Output => {
                config.direction = Direction::Output;
                config.pull = Pull::None;
                if let Some(level) = request.level {
                    config.level = level;
                }
                if let Some(open_drain) = request.open_drain {
                    config.open_drain = open_drain;
                }
            }
        }
    } else {
        if let Some(level) = request.level {
            config.level = level;
        }
        if let Some(pull) = request.pull {
            config.pull = pull;
        }
        if let Some(open_drain) = request.open_drain {
            config.open_drain = open_drain;
        }
    }

    // 方向与字段的匹配规则：只看请求里显式出现的字段，避免沿用值误报。
    if request.level.is_some() && config.direction != Direction::Output {
        return Err(GpioError::invalid(format!(
            "引脚 {pin} 处于输入模式，不能设置 level；请先 gpio.config {pin} out",
            pin = spec.id
        )));
    }
    if request.pull.is_some() && config.direction != Direction::Input {
        return Err(GpioError::invalid(format!(
            "引脚 {pin} 处于输出模式，不能设置 pull；请先 gpio.config {pin} in",
            pin = spec.id
        )));
    }
    if request.open_drain == Some(true) && config.direction != Direction::Output {
        return Err(GpioError::invalid(format!(
            "引脚 {pin} 处于输入模式，不能启用 open_drain",
            pin = spec.id
        )));
    }

    validate_config(spec, &config)?;
    Ok(config)
}

/// 校验一份**完整**配置是否与引脚能力/保留标志兼容。
///
/// 后端在真正操作寄存器前调用；[resolve_config] 在合并完成后也会调用。
pub fn validate_config(spec: &'static PinSpec, config: &PinConfig) -> Result<(), GpioError> {
    ensure_not_reserved(spec)?;
    match config.direction {
        Direction::Input => {
            if !spec.caps.input {
                return Err(GpioError::unsupported(format!(
                    "引脚 {pin} 不支持输入",
                    pin = spec.id
                )));
            }
        }
        Direction::Output => {
            if !spec.caps.output {
                return Err(GpioError::unsupported(if spec.flags.input_only {
                    format!("引脚 {pin} 仅支持输入", pin = spec.id)
                } else {
                    format!("引脚 {pin} 不支持输出", pin = spec.id)
                }));
            }
        }
    }
    match config.pull {
        Pull::None => {}
        Pull::Up if !spec.caps.pull_up => {
            return Err(GpioError::unsupported(format!(
                "引脚 {pin} 不支持上拉",
                pin = spec.id
            )));
        }
        Pull::Down if !spec.caps.pull_down => {
            return Err(GpioError::unsupported(format!(
                "引脚 {pin} 不支持下拉",
                pin = spec.id
            )));
        }
        Pull::Up | Pull::Down => {}
    }
    if config.open_drain && !spec.caps.open_drain {
        return Err(GpioError::unsupported(format!(
            "引脚 {pin} 不支持开漏输出",
            pin = spec.id
        )));
    }
    Ok(())
}

/// 保留引脚在任何 configure/read/write 之前直接拒绝。
pub fn ensure_not_reserved(spec: &'static PinSpec) -> Result<(), GpioError> {
    if spec.flags.reserved {
        Err(GpioError::reserved_pin(spec))
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ConfigRequest, Direction, Level, PinConfig, Pull, resolve_config, validate_config,
    };
    use crate::family::ChipFamily;
    use crate::pin::{PinId, Port};
    use crate::port::GpioError;

    fn spec(family: ChipFamily, pin: PinId) -> &'static crate::family::PinSpec {
        family.spec().by_id(pin).expect("引脚应在表内")
    }

    fn request(
        direction: Option<Direction>,
        level: Option<Level>,
        pull: Option<Pull>,
        open_drain: Option<bool>,
    ) -> ConfigRequest {
        ConfigRequest {
            direction,
            level,
            pull,
            open_drain,
        }
    }

    #[test]
    fn default_is_safe_input() {
        let config = PinConfig::default();
        assert_eq!(config.direction, Direction::Input);
        assert_eq!(config.level, Level::Low);
        assert_eq!(config.pull, Pull::None);
        assert!(!config.open_drain);

        let resolved = resolve_config(
            spec(ChipFamily::Esp32, PinId::io(4)),
            None,
            &ConfigRequest::new(),
        )
        .unwrap();
        assert_eq!(resolved, PinConfig::default());
    }

    #[test]
    fn explicit_output_applies_level_and_resets_pull() {
        let pin = spec(ChipFamily::Esp32, PinId::io(4));
        let resolved = resolve_config(
            pin,
            Some(PinConfig {
                direction: Direction::Input,
                level: Level::Low,
                pull: Pull::Up,
                open_drain: false,
            }),
            &request(Some(Direction::Output), Some(Level::High), None, None),
        )
        .unwrap();
        assert_eq!(resolved.direction, Direction::Output);
        assert_eq!(resolved.level, Level::High);
        assert_eq!(resolved.pull, Pull::None);
    }

    #[test]
    fn explicit_input_forces_level_low_and_clears_open_drain() {
        let pin = spec(ChipFamily::Esp32, PinId::io(4));
        let resolved = resolve_config(
            pin,
            Some(PinConfig {
                direction: Direction::Output,
                level: Level::High,
                pull: Pull::None,
                open_drain: true,
            }),
            &request(Some(Direction::Input), None, Some(Pull::Down), None),
        )
        .unwrap();
        assert_eq!(resolved.direction, Direction::Input);
        assert_eq!(resolved.level, Level::Low);
        assert_eq!(resolved.pull, Pull::Down);
        assert!(!resolved.open_drain);
    }

    #[test]
    fn partial_update_without_direction_keeps_current_direction() {
        let pin = spec(ChipFamily::Esp32, PinId::io(4));
        let current = PinConfig {
            direction: Direction::Output,
            level: Level::High,
            pull: Pull::None,
            open_drain: false,
        };
        let resolved = resolve_config(
            pin,
            Some(current),
            &request(None, Some(Level::Low), None, None),
        )
        .unwrap();
        assert_eq!(resolved.direction, Direction::Output);
        assert_eq!(resolved.level, Level::Low);
    }

    #[test]
    fn direction_sensitive_fields_are_rejected() {
        let pin = spec(ChipFamily::Esp32, PinId::io(4));
        assert!(matches!(
            resolve_config(pin, None, &request(None, Some(Level::High), None, None)),
            Err(GpioError::InvalidArgument(_))
        ));
        assert!(resolve_config(pin, None, &request(None, None, Some(Pull::Up), None)).is_ok());
        assert!(matches!(
            resolve_config(
                pin,
                None,
                &request(Some(Direction::Output), None, Some(Pull::Up), None)
            ),
            Err(GpioError::InvalidArgument(_))
        ));
        assert!(matches!(
            resolve_config(
                pin,
                None,
                &request(Some(Direction::Input), None, None, Some(true))
            ),
            Err(GpioError::InvalidArgument(_))
        ));
    }

    #[test]
    fn input_only_pin_cannot_be_configured_as_output() {
        let pin = spec(ChipFamily::Esp32, PinId::io(34));
        let error = resolve_config(
            pin,
            None,
            &request(Some(Direction::Output), None, None, None),
        )
        .unwrap_err();
        assert!(matches!(error, GpioError::Unsupported(_)), "{error:?}");
        assert!(
            error.message().contains("仅支持输入"),
            "{}",
            error.message()
        );
    }

    #[test]
    fn reserved_pin_is_rejected_even_with_empty_request() {
        let pin = spec(ChipFamily::Esp32, PinId::io(6));
        let error = resolve_config(pin, None, &ConfigRequest::new()).unwrap_err();
        assert!(matches!(error, GpioError::ReservedPin(_)), "{error:?}");
        assert!(error.message().contains("IO6"), "{}", error.message());
    }

    #[test]
    fn validate_config_checks_capabilities() {
        let input_only = spec(ChipFamily::Esp32, PinId::io(35));
        let config = PinConfig {
            direction: Direction::Output,
            ..PinConfig::default()
        };
        assert!(matches!(
            validate_config(input_only, &config),
            Err(GpioError::Unsupported(_))
        ));

        let reserved = spec(ChipFamily::Esp32, PinId::io(7));
        assert!(matches!(
            validate_config(reserved, &PinConfig::default()),
            Err(GpioError::ReservedPin(_))
        ));
    }

    #[test]
    fn stm32_pins_share_the_same_semantics() {
        let pin = spec(ChipFamily::Stm32F1, PinId::port(Port::A, 5));
        let resolved = resolve_config(
            pin,
            None,
            &request(Some(Direction::Output), Some(Level::High), None, Some(true)),
        )
        .unwrap();
        assert_eq!(resolved.direction, Direction::Output);
        assert_eq!(resolved.level, Level::High);
        assert!(resolved.open_drain);

        let reserved = spec(ChipFamily::Stm32F1, PinId::port(Port::A, 13));
        assert!(matches!(
            resolve_config(reserved, None, &ConfigRequest::new()),
            Err(GpioError::ReservedPin(_))
        ));
    }
}
