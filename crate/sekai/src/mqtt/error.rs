//! MQTT 解码与编码错误类型。
//!
//! 解码错误区分“数据不足”（不是错误，只是需要更多字节）与真正的非法报文；
//! 编码错误描述字段取值、字符串长度或报文大小超出协议限制。

use core::fmt;

/// 解码错误。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecodeError {
    /// 数据不足，不是错误；`needed` 是至少需要多少字节才能完成解码。
    Incomplete { needed: usize },
    /// 结构/编码非法（长度不一致、保留 flags、未知类型/属性 ID、VarInt 非法等）。
    Malformed(&'static str),
    /// 语义违反协议（属性不允许/重复/取值非法、非法原因码、零包 ID、非法主题等）。
    ProtocolError(&'static str),
    /// 不支持的协议版本号。
    UnsupportedProtocolVersion(u8),
    /// 报文超过解码方允许的最大报文大小。
    PacketTooLarge { size: usize, max: u32 },
    /// 内存分配失败。
    AllocationFailed,
}

impl DecodeError {
    /// 映射到 MQTT 5.0 原因码。
    ///
    /// `Malformed` -> 0x81，`ProtocolError` -> 0x82，
    /// `UnsupportedProtocolVersion` -> 0x84，`PacketTooLarge` -> 0x95；
    /// `Incomplete` 与 `AllocationFailed` 不是协议错误，返回 `None`。
    pub fn reason_code(&self) -> Option<u8> {
        match self {
            Self::Malformed(_) => Some(0x81),
            Self::ProtocolError(_) => Some(0x82),
            Self::UnsupportedProtocolVersion(_) => Some(0x84),
            Self::PacketTooLarge { .. } => Some(0x95),
            Self::Incomplete { .. } | Self::AllocationFailed => None,
        }
    }
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Incomplete { needed } => write!(f, "数据不完整：至少还需要 {needed} 字节"),
            Self::Malformed(message) => write!(f, "报文结构非法：{message}"),
            Self::ProtocolError(message) => write!(f, "协议错误：{message}"),
            Self::UnsupportedProtocolVersion(version) => {
                write!(f, "不支持的协议版本：{version}")
            }
            Self::PacketTooLarge { size, max } => {
                write!(f, "报文过大：{size} 字节超过上限 {max} 字节")
            }
            Self::AllocationFailed => write!(f, "内存分配失败"),
        }
    }
}

impl core::error::Error for DecodeError {}

/// 编码错误。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EncodeError {
    /// 字段取值超出允许范围。
    ValueOutOfRange {
        field: &'static str,
        value: u32,
        max: u32,
    },
    /// 字符串或二进制数据长度超过 u16 上限。
    StringTooLong { field: &'static str, len: usize },
    /// 报文超过 MQTT 协议允许的最大报文大小。
    PacketTooLarge { size: usize },
    /// 字段取值非法（不允许的属性、重复属性、零值等）。
    Invalid(&'static str),
    /// 内存分配失败。
    AllocationFailed,
}

impl fmt::Display for EncodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ValueOutOfRange { field, value, max } => {
                write!(f, "字段 {field} 取值 {value} 超出范围（最大 {max}）")
            }
            Self::StringTooLong { field, len } => {
                write!(f, "字段 {field} 长度 {len} 超过 65535 字节")
            }
            Self::PacketTooLarge { size } => write!(f, "报文过大：{size} 字节"),
            Self::Invalid(message) => write!(f, "无法编码：{message}"),
            Self::AllocationFailed => write!(f, "内存分配失败"),
        }
    }
}

impl core::error::Error for EncodeError {}

#[cfg(test)]
mod tests {
    use super::{DecodeError, EncodeError};
    use alloc::string::ToString;

    /// 确认两种错误类型都实现了 core::error::Error（no_std 环境）。
    #[test]
    fn implements_core_error() {
        fn assert_error<E: core::error::Error>() {}
        assert_error::<DecodeError>();
        assert_error::<EncodeError>();
    }

    #[test]
    fn decode_reason_code_mapping() {
        assert_eq!(DecodeError::Malformed("x").reason_code(), Some(0x81));
        assert_eq!(DecodeError::ProtocolError("x").reason_code(), Some(0x82));
        assert_eq!(
            DecodeError::UnsupportedProtocolVersion(4).reason_code(),
            Some(0x84)
        );
        assert_eq!(
            DecodeError::PacketTooLarge { size: 10, max: 5 }.reason_code(),
            Some(0x95)
        );
        assert_eq!(DecodeError::Incomplete { needed: 1 }.reason_code(), None);
        assert_eq!(DecodeError::AllocationFailed.reason_code(), None);
    }

    #[test]
    fn decode_display_is_readable() {
        assert_eq!(
            DecodeError::Incomplete { needed: 3 }.to_string(),
            "数据不完整：至少还需要 3 字节"
        );
        assert_eq!(
            DecodeError::Malformed("长度不一致").to_string(),
            "报文结构非法：长度不一致"
        );
        assert_eq!(
            DecodeError::ProtocolError("属性重复").to_string(),
            "协议错误：属性重复"
        );
        assert_eq!(
            DecodeError::UnsupportedProtocolVersion(3).to_string(),
            "不支持的协议版本：3"
        );
        assert_eq!(
            DecodeError::PacketTooLarge { size: 9, max: 8 }.to_string(),
            "报文过大：9 字节超过上限 8 字节"
        );
        assert_eq!(DecodeError::AllocationFailed.to_string(), "内存分配失败");
    }

    #[test]
    fn encode_display_is_readable() {
        assert_eq!(
            EncodeError::ValueOutOfRange {
                field: "Variable Byte Integer",
                value: 268_435_456,
                max: 268_435_455,
            }
            .to_string(),
            "字段 Variable Byte Integer 取值 268435456 超出范围（最大 268435455）"
        );
        assert_eq!(
            EncodeError::StringTooLong {
                field: "主题名",
                len: 70_000,
            }
            .to_string(),
            "字段 主题名 长度 70000 超过 65535 字节"
        );
        assert_eq!(
            EncodeError::PacketTooLarge { size: 100 }.to_string(),
            "报文过大：100 字节"
        );
        assert_eq!(
            EncodeError::Invalid("属性重复").to_string(),
            "无法编码：属性重复"
        );
        assert_eq!(EncodeError::AllocationFailed.to_string(), "内存分配失败");
    }

    #[test]
    fn derives_are_usable() {
        // Copy / Clone / PartialEq / Eq 派生可用性检查。
        let error = DecodeError::Malformed("x");
        let copied = error;
        assert_eq!(error, copied);
        assert_ne!(DecodeError::Malformed("x"), DecodeError::ProtocolError("x"));
        let encode = EncodeError::Invalid("x");
        assert_eq!(encode, encode.clone());
    }
}
