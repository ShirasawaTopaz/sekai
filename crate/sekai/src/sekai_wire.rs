//! sekai 控制平面协议的公共定义。
//!
//! 本模块不定义具体报文，只提供所有控制平面帧共享的协议版本、硬上限、
//! 解码/编码错误、命令错误码，以及名称、Worker id 和跳转目标的校验函数。
//! 具体帧的编解码见 [`crate::sekai_stdin`] 与 [`crate::sekai_stdout`]。
//!
//! 协议使用固定布局的大端二进制格式，不依赖 JSON 或第三方库，以便在
//! `no_std` + `alloc` 的固件环境中保持可预测的内存占用。

use alloc::string::String;
use core::fmt;

use crate::mqtt::error::{DecodeError, EncodeError};

/// 控制平面协议版本。
pub const PROTOCOL_VERSION: u8 = 1;

/// 单个 Stdin/Stdout 载荷的硬上限（字节）。
pub const MAX_PAYLOAD: usize = 4 * 1024;

/// 任务名/工具名/固件名等短名称的硬上限（字节）。
pub const MAX_NAME_LEN: usize = 48;

/// Worker id 的硬上限（字节）。
pub const MAX_DEVICE_ID_LEN: usize = 64;

/// 固件版本字符串的硬上限（字节）。
pub const MAX_FIRMWARE_LEN: usize = 64;

/// 跳转目标主机名的硬上限（字节）。
pub const MAX_HOST_LEN: usize = 64;

/// 跳转目标字符串的硬上限（字节）。
pub const MAX_TARGET_LEN: usize = 96;

/// 单次 Hello 中任务清单的硬上限。
pub const MAX_TASKS: usize = 16;

/// 单次 Hello 中工具清单的硬上限。
pub const MAX_TOOLS: usize = 16;

/// Worker列表的硬上限。
pub const MAX_DEVICES: usize = 8;

/// 单次 Hello 中活跃任务清单的硬上限。
pub const MAX_ACTIVE_TASKS: usize = 4;

/// `delay_ms` 的硬上限：24 小时。
pub const MAX_DELAY_MS: u32 = 86_400_000;

/// `ddl_ms` 的硬上限：5 分钟。
pub const MAX_DDL_MS: u32 = 300_000;

/// `ddl_ms == 0` 时使用的默认执行预算（毫秒）。
pub const DEFAULT_DDL_MS: u32 = 500;

/// 错误消息的最大字节数。
pub const MAX_ERROR_MESSAGE_LEN: usize = 256;

/// 控制平面帧解码错误。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProtocolError {
    /// 数据不足，无法完成解码。
    Truncated,
    /// 帧末尾存在多余字节。
    TrailingBytes,
    /// 不支持的协议版本。
    UnsupportedVersion(u8),
    /// 未知的 kind/phase/取值。
    UnknownTag(u8),
    /// 字符串不是合法 UTF-8。
    InvalidUtf8,
    /// 字段取值不合法；消息为中文静态描述。
    InvalidField(&'static str),
    /// 载荷长度超过上限。
    PayloadTooLarge { len: usize, max: usize },
    /// 内存分配失败。
    AllocationFailed,
    /// 底层编解码报告的结构错误。
    Malformed(&'static str),
}

impl fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Truncated => write!(f, "数据不完整"),
            Self::TrailingBytes => write!(f, "帧末尾存在多余字节"),
            Self::UnsupportedVersion(version) => write!(f, "不支持的协议版本：{version}"),
            Self::UnknownTag(value) => write!(f, "未知的类型标记：{value}"),
            Self::InvalidUtf8 => write!(f, "字符串不是合法 UTF-8"),
            Self::InvalidField(message) => write!(f, "字段非法：{message}"),
            Self::PayloadTooLarge { len, max } => {
                write!(f, "载荷过大：{len} 字节超过上限 {max} 字节")
            }
            Self::AllocationFailed => write!(f, "内存分配失败"),
            Self::Malformed(message) => write!(f, "帧结构非法：{message}"),
        }
    }
}

impl core::error::Error for ProtocolError {}

impl From<DecodeError> for ProtocolError {
    fn from(error: DecodeError) -> Self {
        match error {
            DecodeError::Incomplete { .. } => Self::Truncated,
            DecodeError::Malformed("字段长度超出报文剩余长度") => Self::Truncated,
            DecodeError::Malformed(message) => Self::Malformed(message),
            DecodeError::ProtocolError(message) => Self::InvalidField(message),
            DecodeError::UnsupportedProtocolVersion(version) => Self::UnsupportedVersion(version),
            DecodeError::PacketTooLarge { size, max } => Self::PayloadTooLarge {
                len: size,
                max: max as usize,
            },
            DecodeError::AllocationFailed => Self::AllocationFailed,
        }
    }
}

/// 控制平面帧编码错误。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProtocolEncodeError {
    /// 字符串/二进制字段超过协议允许的长度。
    FieldTooLong(&'static str),
    /// 编码后的帧超过载荷上限。
    TooLarge { len: usize, max: usize },
    /// 字段取值不合法。
    InvalidField(&'static str),
    /// 内存分配失败。
    AllocationFailed,
}

impl fmt::Display for ProtocolEncodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::FieldTooLong(field) => write!(f, "字段 {field} 过长"),
            Self::TooLarge { len, max } => {
                write!(f, "编码结果过大：{len} 字节超过上限 {max} 字节")
            }
            Self::InvalidField(message) => write!(f, "字段非法：{message}"),
            Self::AllocationFailed => write!(f, "内存分配失败"),
        }
    }
}

impl core::error::Error for ProtocolEncodeError {}

impl From<EncodeError> for ProtocolEncodeError {
    fn from(error: EncodeError) -> Self {
        match error {
            EncodeError::ValueOutOfRange { field, .. } => Self::InvalidField(field),
            EncodeError::StringTooLong { field, .. } => Self::FieldTooLong(field),
            EncodeError::PacketTooLarge { size } => Self::TooLarge {
                len: size,
                max: MAX_PAYLOAD,
            },
            EncodeError::Invalid(message) => Self::InvalidField(message),
            EncodeError::AllocationFailed => Self::AllocationFailed,
        }
    }
}

/// 控制平面命令错误码，写入 `Stdout` 的 `Failed` 载荷。
#[repr(u16)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorCode {
    /// 信封字段/版本/长度非法。
    InvalidCommand = 0x0001,
    /// 帧版本不支持。
    UnsupportedVersion = 0x0002,
    /// 载荷非法、名称非法或参数非法。
    InvalidArgument = 0x0003,
    /// 超过允许的命令年龄。
    Expired = 0x0004,
    /// 队列已满或正在跳转。
    Busy = 0x0005,
    /// Task 名称未注册。
    UnknownTask = 0x0006,
    /// Tool 名称未注册。
    UnknownTool = 0x0007,
    /// 超过 DDL。
    Timeout = 0x0008,
    /// 结果超过载荷上限。
    ResultTooLarge = 0x0009,
    /// 被取消。
    Cancelled = 0x000A,
    /// 跳转目标非法。
    JumpInvalidTarget = 0x000B,
    /// 跳转目标不可达，已回退。
    JumpUnreachable = 0x000C,
    /// 运行时内部错误。
    Internal = 0x000D,
    /// 预留能力/版本不支持。
    Unsupported = 0x000E,
}

impl ErrorCode {
    /// 线上 u16 值。
    pub const fn as_u16(self) -> u16 {
        self as u16
    }

    /// 由线上值构造；未知值返回 `None`。
    pub const fn from_u16(value: u16) -> Option<Self> {
        Some(match value {
            0x0001 => Self::InvalidCommand,
            0x0002 => Self::UnsupportedVersion,
            0x0003 => Self::InvalidArgument,
            0x0004 => Self::Expired,
            0x0005 => Self::Busy,
            0x0006 => Self::UnknownTask,
            0x0007 => Self::UnknownTool,
            0x0008 => Self::Timeout,
            0x0009 => Self::ResultTooLarge,
            0x000A => Self::Cancelled,
            0x000B => Self::JumpInvalidTarget,
            0x000C => Self::JumpUnreachable,
            0x000D => Self::Internal,
            0x000E => Self::Unsupported,
            _ => return None,
        })
    }

    /// 适合写入 `CommandError.message` 的中文默认描述。
    pub const fn description(self) -> &'static str {
        match self {
            Self::InvalidCommand => "命令帧非法",
            Self::UnsupportedVersion => "不支持的协议版本",
            Self::InvalidArgument => "命令参数非法",
            Self::Expired => "命令已过期",
            Self::Busy => "Worker忙",
            Self::UnknownTask => "任务未注册",
            Self::UnknownTool => "工具未注册",
            Self::Timeout => "执行超时",
            Self::ResultTooLarge => "结果过大",
            Self::Cancelled => "命令已取消",
            Self::JumpInvalidTarget => "跳转目标非法",
            Self::JumpUnreachable => "跳转目标不可达",
            Self::Internal => "内部错误",
            Self::Unsupported => "不支持的能力",
        }
    }
}

impl fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} (0x{:04X})", self.description(), self.as_u16())
    }
}

/// Task/Tool 处理器返回的错误。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandError {
    /// 错误码。
    pub code: ErrorCode,
    /// 可读的错误描述；编码为 `Failed` 时会截断到 256 字节。
    pub message: String,
}

impl CommandError {
    /// 构造一个命令错误。
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

impl From<ErrorCode> for CommandError {
    fn from(code: ErrorCode) -> Self {
        Self {
            code,
            message: String::from(code.description()),
        }
    }
}

impl fmt::Display for CommandError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl core::error::Error for CommandError {}

/// 把 UTF-8 字符串截断到不超过 `max` 字节，且不切断字符边界。
pub fn truncate_utf8(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut end = max;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// 校验任务名/工具名：1..=48 字节的 ASCII 小写字母、数字、`._-`。
pub fn validate_name(name: &str) -> Result<(), &'static str> {
    if name.is_empty() {
        return Err("名称不能为空");
    }
    if name.len() > MAX_NAME_LEN {
        return Err("名称超过 48 字节");
    }
    if !name.bytes().all(|byte| {
        byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'_' | b'-')
    }) {
        return Err("名称只能包含小写字母、数字和 ._-");
    }
    Ok(())
}

/// 校验Worker id：1..=64 字节的 ASCII 字母、数字、`._-`。
pub fn validate_worker_id(id: &str) -> Result<(), &'static str> {
    if id.is_empty() {
        return Err("Worker id 不能为空");
    }
    if id.len() > MAX_DEVICE_ID_LEN {
        return Err("Worker id 超过 64 字节");
    }
    if !id
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        return Err("Worker id 只能包含字母、数字和 ._-");
    }
    Ok(())
}

/// 校验主机名/IP：1..=64 字节的 ASCII 字母、数字、`._-`；v1 不支持 IPv6 字面量。
pub fn validate_host(host: &str) -> Result<(), &'static str> {
    if host.is_empty() {
        return Err("主机名不能为空");
    }
    if host.len() > MAX_HOST_LEN {
        return Err("主机名超过 64 字节");
    }
    if !host
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        return Err("主机名只能包含字母、数字和 ._-");
    }
    Ok(())
}

/// 校验跳转目标字符串：非空、≤96 字节、无空白/控制字符。
///
/// 具体是Worker名还是 `host:port` 由 [`crate::sekai_device_list::DeviceList::resolve`] 决定。
pub fn validate_target(target: &str) -> Result<(), &'static str> {
    if target.is_empty() {
        return Err("跳转目标不能为空");
    }
    if target.len() > MAX_TARGET_LEN {
        return Err("跳转目标超过 96 字节");
    }
    if target
        .bytes()
        .any(|byte| byte == 0 || byte.is_ascii_control() || byte.is_ascii_whitespace())
    {
        return Err("跳转目标不能包含空白或控制字符");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        CommandError, ErrorCode, MAX_NAME_LEN, ProtocolEncodeError, ProtocolError, truncate_utf8,
        validate_host, validate_name, validate_target, validate_worker_id,
    };
    use crate::mqtt::error::{DecodeError, EncodeError};
    use alloc::string::{String, ToString};

    #[test]
    fn error_code_roundtrip() {
        for value in 0x0001..=0x000E {
            let code = ErrorCode::from_u16(value).unwrap();
            assert_eq!(code.as_u16(), value);
            assert!(!code.description().is_empty());
        }
        assert_eq!(ErrorCode::from_u16(0), None);
        assert_eq!(ErrorCode::from_u16(0xFFFF), None);
    }

    #[test]
    fn decode_error_mapping() {
        assert_eq!(
            ProtocolError::from(DecodeError::Incomplete { needed: 3 }),
            ProtocolError::Truncated
        );
        assert_eq!(
            ProtocolError::from(DecodeError::Malformed("x")),
            ProtocolError::Malformed("x")
        );
        assert_eq!(
            ProtocolError::from(DecodeError::ProtocolError("y")),
            ProtocolError::InvalidField("y")
        );
        assert_eq!(
            ProtocolError::from(DecodeError::PacketTooLarge { size: 9, max: 8 }),
            ProtocolError::PayloadTooLarge { len: 9, max: 8 }
        );
        assert_eq!(
            ProtocolError::from(DecodeError::AllocationFailed),
            ProtocolError::AllocationFailed
        );
    }

    #[test]
    fn encode_error_mapping() {
        assert_eq!(
            ProtocolEncodeError::from(EncodeError::StringTooLong {
                field: "名称",
                len: 99,
            }),
            ProtocolEncodeError::FieldTooLong("名称")
        );
        assert_eq!(
            ProtocolEncodeError::from(EncodeError::AllocationFailed),
            ProtocolEncodeError::AllocationFailed
        );
    }

    #[test]
    fn name_validation() {
        for name in ["echo", "a", "sekai.info", "task_1", "a-b.c_d"] {
            assert_eq!(validate_name(name), Ok(()), "name = {name:?}");
        }
        for name in ["", "Echo", "echo!", "a b", "任务"] {
            assert!(validate_name(name).is_err(), "name = {name:?}");
        }
        let long = "a".repeat(MAX_NAME_LEN + 1);
        assert!(validate_name(&long).is_err());
    }

    #[test]
    fn worker_id_and_host_validation() {
        assert_eq!(validate_worker_id("esp32-3c71bf"), Ok(()));
        assert_eq!(validate_worker_id("Host_1.local"), Ok(()));
        assert_eq!(
            validate_worker_id("a/b"),
            Err("Worker id 只能包含字母、数字和 ._-")
        );
        assert_eq!(validate_host("192.168.1.5"), Ok(()));
        assert_eq!(validate_host("broker.local"), Ok(()));
        assert_eq!(validate_host("::1"), Err("主机名只能包含字母、数字和 ._-"));
    }

    #[test]
    fn target_validation() {
        assert_eq!(validate_target("peer-1"), Ok(()));
        assert_eq!(validate_target("192.168.1.5:1883"), Ok(()));
        assert_eq!(validate_target(""), Err("跳转目标不能为空"));
        assert_eq!(
            validate_target("a b"),
            Err("跳转目标不能包含空白或控制字符")
        );
    }

    #[test]
    fn truncation_respects_char_boundaries() {
        assert_eq!(truncate_utf8("hello", 10), "hello");
        assert_eq!(truncate_utf8("hello", 3), "hel");
        // “世”占 3 字节；上限 4 时只能保留一个完整字符。
        assert_eq!(truncate_utf8("世界", 4), "世");
        assert_eq!(truncate_utf8("世界", 0), "");
    }

    #[test]
    fn command_error_from_code_has_message() {
        let error = CommandError::from(ErrorCode::Timeout);
        assert_eq!(error.code, ErrorCode::Timeout);
        assert_eq!(error.message, String::from("执行超时"));
        assert!(error.to_string().contains("执行超时"));
    }
}
