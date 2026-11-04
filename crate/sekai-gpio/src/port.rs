//! GPIO 端口抽象与统一错误。
//!
//! [`GpioPort`] 是家族无关的后端接口：ESP32、STM32、虚拟后端都实现它。
//! 统一的错误类型 [`GpioError`] 会映射到协议错误码：参数类错误统一为
//! `InvalidArgument`，硬件内部异常为 `Internal`。

use alloc::format;
use alloc::string::{String, ToString};
use core::fmt;

use sekai::sekai_wire::{CommandError, ErrorCode};

use crate::config::{Direction, Level, PinConfig};
use crate::family::{FamilySpec, PinSpec};
use crate::pin::PinId;

/// 后端种类；`gpio.info` 会输出它。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendKind {
    /// 真实硬件（ESP32/STM32）。
    Hardware,
    /// 宿主虚拟后端。
    Virtual,
}

impl BackendKind {
    /// 工具输出使用的小写名称。
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Hardware => "hardware",
            Self::Virtual => "virtual",
        }
    }
}

/// 单个引脚的描述快照。
#[derive(Debug, Clone)]
pub struct PinDescription {
    /// 引脚表项。
    pub spec: &'static PinSpec,
    /// 当前配置；未配置为 `None`。
    pub config: Option<PinConfig>,
    /// 当前电平：输入读 IDR，输出读锁存/ODR；未配置为 `None`。
    pub level: Option<Level>,
}

/// 家族无关的 GPIO 端口。
pub trait GpioPort {
    /// 引脚表。
    fn spec(&self) -> &'static FamilySpec;

    /// 后端种类；默认是硬件。
    fn backend(&self) -> BackendKind {
        BackendKind::Hardware
    }

    /// 应用一份**已经校验过**的配置。
    ///
    /// 调用方应先用 [`crate::config::resolve_config`] 合并/校验；后端仍需在
    /// 触碰硬件前调用 [`crate::config::validate_config`] 做防御性检查。
    fn configure(&mut self, pin: PinId, config: PinConfig) -> Result<(), GpioError>;

    /// 读取输入电平；只允许已配置为输入的引脚。
    fn read(&mut self, pin: PinId) -> Result<Level, GpioError>;

    /// 写输出电平；只允许已配置为输出的引脚。
    fn write(&mut self, pin: PinId, level: Level) -> Result<(), GpioError>;

    /// 读取引脚描述；未配置的引脚也允许（`gpio.info` 使用）。
    fn describe(&mut self, pin: PinId) -> Result<PinDescription, GpioError>;

    /// 已配置引脚数量；用于 `gpio.info` 的 `configured` 字段。
    fn configured_count(&self) -> usize {
        0
    }
}

/// 统一 GPIO 错误。
///
/// 每个变体都携带**已经格式化**的中文消息，便于直接映射到 `CommandError`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GpioError {
    /// 引脚不在当前家族表内。
    UnknownPin(String),
    /// 引脚被保留（调试口/SPI flash 等）。
    ReservedPin(String),
    /// 引脚尚未配置。
    NotConfigured(String),
    /// 方向不匹配（例如对输出引脚调用 read）。
    WrongMode(String),
    /// 引脚能力不支持请求的配置。
    Unsupported(String),
    /// 参数/配置非法。
    InvalidArgument(String),
    /// 后端内部错误。
    Backend(String),
}

impl GpioError {
    /// 协议错误码：参数类错误统一 `InvalidArgument`，后端异常 `Internal`。
    pub fn code(&self) -> ErrorCode {
        match self {
            Self::Backend(_) => ErrorCode::Internal,
            _ => ErrorCode::InvalidArgument,
        }
    }

    /// 可读错误消息。
    pub fn message(&self) -> &str {
        match self {
            Self::UnknownPin(message)
            | Self::ReservedPin(message)
            | Self::NotConfigured(message)
            | Self::WrongMode(message)
            | Self::Unsupported(message)
            | Self::InvalidArgument(message)
            | Self::Backend(message) => message,
        }
    }

    /// 构造参数错误。
    pub fn invalid(message: impl Into<String>) -> Self {
        Self::InvalidArgument(message.into())
    }

    /// 构造后端错误。
    pub fn backend(message: impl Into<String>) -> Self {
        Self::Backend(message.into())
    }

    /// 构造“引脚不存在”错误。
    pub fn unknown_pin(pin: PinId) -> Self {
        Self::UnknownPin(format!("引脚 {pin} 不存在"))
    }

    /// 构造“引脚不存在”错误，保留用户输入的原始写法。
    pub fn unknown_pin_named(name: &str) -> Self {
        Self::UnknownPin(format!("引脚 {name} 不存在"))
    }

    /// 构造“保留引脚”错误；原因取引脚表的 `note`。
    pub fn reserved_pin(spec: &'static PinSpec) -> Self {
        let reason = spec.note.trim();
        let message = if reason.is_empty() {
            format!("引脚 {} 已保留", spec.id)
        } else {
            format!("引脚 {} 已保留（{}）", spec.id, reason)
        };
        Self::ReservedPin(message)
    }

    /// 构造“未配置”错误。
    pub fn not_configured(pin: PinId) -> Self {
        Self::NotConfigured(format!(
            "引脚 {pin} 未配置；请先 gpio.config {pin} in 或 gpio.config {pin} out"
        ))
    }

    /// 构造“需要输入模式”错误。
    pub fn wrong_mode_read(pin: PinId, config: PinConfig) -> Self {
        if config.direction == Direction::Output {
            Self::WrongMode(format!(
                "引脚 {pin} 处于输出模式，无法读取；请先 gpio.config {pin} in"
            ))
        } else {
            Self::not_configured(pin)
        }
    }

    /// 构造“需要输出模式”错误。
    pub fn wrong_mode_write(pin: PinId, config: PinConfig) -> Self {
        if config.direction == Direction::Input {
            Self::WrongMode(format!(
                "引脚 {pin} 处于输入模式，无法写入；请先 gpio.config {pin} out"
            ))
        } else {
            Self::not_configured(pin)
        }
    }

    /// 构造“能力不足”错误。
    pub fn unsupported(message: impl Into<String>) -> Self {
        Self::Unsupported(message.into())
    }
}

impl fmt::Display for GpioError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.message())
    }
}

impl core::error::Error for GpioError {}

impl From<GpioError> for CommandError {
    fn from(error: GpioError) -> Self {
        let code = error.code();
        Self::new(code, error.message().to_string())
    }
}

/// 在家族表中查找引脚；不存在时返回统一错误。
pub fn lookup(spec: &'static FamilySpec, pin: PinId) -> Result<&'static PinSpec, GpioError> {
    spec.by_id(pin).ok_or_else(|| GpioError::unknown_pin(pin))
}

/// 保留引脚在 read/write 前直接拒绝。
pub fn ensure_operable(spec: &'static PinSpec) -> Result<(), GpioError> {
    crate::config::ensure_not_reserved(spec)
}

/// 校验引脚已配置为输入；否则返回统一错误。
pub fn ensure_input_mode(
    spec: &'static PinSpec,
    config: Option<PinConfig>,
) -> Result<PinConfig, GpioError> {
    ensure_operable(spec)?;
    match config {
        None => Err(GpioError::not_configured(spec.id)),
        Some(config) if config.direction != Direction::Input => {
            Err(GpioError::wrong_mode_read(spec.id, config))
        }
        Some(config) => Ok(config),
    }
}

/// 校验引脚已配置为输出；否则返回统一错误。
pub fn ensure_output_mode(
    spec: &'static PinSpec,
    config: Option<PinConfig>,
) -> Result<PinConfig, GpioError> {
    ensure_operable(spec)?;
    match config {
        None => Err(GpioError::not_configured(spec.id)),
        Some(config) if config.direction != Direction::Output => {
            Err(GpioError::wrong_mode_write(spec.id, config))
        }
        Some(config) => Ok(config),
    }
}

#[cfg(test)]
mod tests {
    use super::{BackendKind, GpioError, ensure_input_mode, ensure_output_mode, lookup};
    use crate::config::{Direction, Level, PinConfig};
    use crate::family::ChipFamily;
    use crate::pin::PinId;
    use crate::port::GpioPort;
    use crate::port::PinDescription;
    use sekai::sekai_wire::ErrorCode;

    struct DummyPort {
        spec: &'static crate::family::FamilySpec,
    }

    impl GpioPort for DummyPort {
        fn spec(&self) -> &'static crate::family::FamilySpec {
            self.spec
        }

        fn backend(&self) -> BackendKind {
            BackendKind::Virtual
        }

        fn configure(&mut self, _pin: PinId, _config: PinConfig) -> Result<(), GpioError> {
            Ok(())
        }

        fn read(&mut self, _pin: PinId) -> Result<Level, GpioError> {
            Ok(Level::Low)
        }

        fn write(&mut self, _pin: PinId, _level: Level) -> Result<(), GpioError> {
            Ok(())
        }

        fn describe(&mut self, pin: PinId) -> Result<PinDescription, GpioError> {
            let spec = lookup(self.spec, pin)?;
            Ok(PinDescription {
                spec,
                config: None,
                level: None,
            })
        }
    }

    #[test]
    fn error_codes_map_to_protocol_codes() {
        assert_eq!(GpioError::invalid("x").code(), ErrorCode::InvalidArgument);
        assert_eq!(
            GpioError::unknown_pin(PinId::io(40)).code(),
            ErrorCode::InvalidArgument
        );
        assert_eq!(GpioError::backend("x").code(), ErrorCode::Internal);
        assert!(
            GpioError::unknown_pin(PinId::io(40))
                .message()
                .contains("IO40")
        );
        let command: sekai::sekai_wire::CommandError = GpioError::invalid("参数非法").into();
        assert_eq!(command.code, ErrorCode::InvalidArgument);
        assert_eq!(command.message, "参数非法");
    }

    #[test]
    fn mode_helpers_are_strict() {
        let spec = ChipFamily::Esp32.spec();
        let pin = lookup(spec, PinId::io(4)).unwrap();
        assert!(matches!(
            ensure_input_mode(pin, None),
            Err(GpioError::NotConfigured(_))
        ));
        assert!(matches!(
            ensure_input_mode(
                pin,
                Some(PinConfig {
                    direction: Direction::Output,
                    ..PinConfig::default()
                })
            ),
            Err(GpioError::WrongMode(_))
        ));
        assert!(matches!(
            ensure_output_mode(
                pin,
                Some(PinConfig {
                    direction: Direction::Input,
                    ..PinConfig::default()
                })
            ),
            Err(GpioError::WrongMode(_))
        ));
        assert!(
            ensure_input_mode(
                pin,
                Some(PinConfig {
                    direction: Direction::Input,
                    level: Level::High,
                    ..PinConfig::default()
                })
            )
            .is_ok()
        );
    }

    #[test]
    fn dummy_port_implements_trait() {
        let mut port = DummyPort {
            spec: ChipFamily::Stm32F1.spec(),
        };
        assert_eq!(port.backend(), BackendKind::Virtual);
        assert_eq!(port.spec().chip, "stm32f1");
        assert!(
            port.describe(PinId::io(1)).is_err(),
            "ESP32 引脚不属于 STM32 表"
        );
        assert!(
            port.describe(crate::pin::PinId::port(crate::pin::Port::A, 5))
                .is_ok()
        );
    }
}
