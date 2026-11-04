//! 引脚标识与命名解析。
//!
//! 家族无关的引脚表示：[PinId] 要么是 ESP32 的 `IO<n>`，要么是 STM32 的
//! `P<port><n>`。解析只做**语法**校验（大小写不敏感、拒绝前导零、拒绝越界
//! 编号）；某个引脚是否属于当前家族，由 [`crate::family::FamilySpec::resolve`]
//! 结合引脚表判断。

use alloc::string::String;
use core::fmt::{self, Write as _};

use crate::family::FamilySpec;

/// 引脚端口；STM32 使用 A..K，ESP32 不使用。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Port {
    /// 端口 A。
    A,
    /// 端口 B。
    B,
    /// 端口 C。
    C,
    /// 端口 D。
    D,
    /// 端口 E。
    E,
    /// 端口 F。
    F,
    /// 端口 G。
    G,
    /// 端口 H。
    H,
    /// 端口 I。
    I,
    /// 端口 J。
    J,
    /// 端口 K。
    K,
}

impl Port {
    /// 端口字符（大写）。
    pub const fn as_char(self) -> char {
        match self {
            Self::A => 'A',
            Self::B => 'B',
            Self::C => 'C',
            Self::D => 'D',
            Self::E => 'E',
            Self::F => 'F',
            Self::G => 'G',
            Self::H => 'H',
            Self::I => 'I',
            Self::J => 'J',
            Self::K => 'K',
        }
    }

    /// 解析端口字母（大小写不敏感）；不是 A..K 时返回 `None`。
    pub const fn from_ascii(byte: u8) -> Option<Self> {
        match byte {
            b'A' | b'a' => Some(Self::A),
            b'B' | b'b' => Some(Self::B),
            b'C' | b'c' => Some(Self::C),
            b'D' | b'd' => Some(Self::D),
            b'E' | b'e' => Some(Self::E),
            b'F' | b'f' => Some(Self::F),
            b'G' | b'g' => Some(Self::G),
            b'H' | b'h' => Some(Self::H),
            b'I' | b'i' => Some(Self::I),
            b'J' | b'j' => Some(Self::J),
            b'K' | b'k' => Some(Self::K),
            _ => None,
        }
    }

    /// 端口序号：A=0，B=1，……。
    pub const fn index(self) -> u8 {
        self.as_char() as u8 - b'A'
    }
}

impl fmt::Display for Port {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_char(self.as_char())
    }
}

/// 家族无关的引脚标识。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum PinId {
    /// ESP32 引脚：`IO0`..`IO39`。
    Io(u8),
    /// STM32 引脚：`PA5` 等。
    Port {
        /// 端口。
        port: Port,
        /// 端口内编号 0..=15。
        pin: u8,
    },
}

/// 规范化引脚标签，`Display` 输出 `IO4` / `PA5`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Label(PinId);

impl fmt::Display for Label {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

impl PinId {
    /// 构造 ESP32 引脚。
    pub const fn io(number: u8) -> Self {
        Self::Io(number)
    }

    /// 构造 STM32 引脚。
    pub const fn port(port: Port, pin: u8) -> Self {
        Self::Port { port, pin }
    }

    /// 规范化标签；`Display` 输出 `IO4` / `PA5`。
    pub const fn label(self) -> Label {
        Label(self)
    }

    /// 是否为 ESP32 形式的引脚。
    pub const fn is_io(self) -> bool {
        matches!(self, Self::Io(_))
    }

    /// 解析引脚名（大小写不敏感）。
    ///
    /// ESP32 形式：`IO4`、`GPIO4`、`4`；编号 0..=39，拒绝前导零
    /// （`IO04` 非法，`IO0` 合法）。
    ///
    /// STM32 形式：`P<A-K><n>`；编号 0..=15，拒绝前导零（`PA05` 非法）。
    ///
    /// 只校验语法；家族归属由 [`crate::family::FamilySpec::resolve`] 判断。
    pub fn parse(text: &str) -> Result<Self, PinParseError> {
        if text.is_empty() {
            return Err(PinParseError::Empty);
        }
        if text.len() > 8 {
            return Err(PinParseError::TooLong);
        }
        let bytes = text.as_bytes();
        if bytes.iter().all(u8::is_ascii_digit) {
            return parse_io_number(text);
        }
        if bytes.len() >= 3 && matches!(bytes[0], b'I' | b'i') && matches!(bytes[1], b'O' | b'o') {
            return parse_io_number(&text[2..]);
        }
        if bytes.len() >= 5
            && matches!(bytes[0], b'G' | b'g')
            && matches!(bytes[1], b'P' | b'p')
            && matches!(bytes[2], b'I' | b'i')
            && matches!(bytes[3], b'O' | b'o')
        {
            return parse_io_number(&text[4..]);
        }
        if matches!(bytes[0], b'P' | b'p') {
            return parse_stm32(text, bytes);
        }
        Err(PinParseError::InvalidSyntax)
    }

    /// 该引脚在当前家族表中的后端槽位；不属于该家族时返回 `None`。
    pub fn slot(self, spec: &FamilySpec) -> Option<u16> {
        spec.by_id(self).map(|pin| pin.slot)
    }
}

impl fmt::Display for PinId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(number) => write!(f, "IO{number}"),
            Self::Port { port, pin } => write!(f, "P{port}{pin}"),
        }
    }
}

/// 解析 ESP32 数字部分（`IO` 前缀之后或纯数字）。
fn parse_io_number(digits: &str) -> Result<PinId, PinParseError> {
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(PinParseError::InvalidSyntax);
    }
    if digits.len() > 1 && digits.as_bytes()[0] == b'0' {
        return Err(PinParseError::LeadingZero);
    }
    let mut value: u32 = 0;
    for byte in digits.bytes() {
        value = value
            .saturating_mul(10)
            .saturating_add(u32::from(byte - b'0'));
        if value > 39 {
            return Err(PinParseError::IoOutOfRange);
        }
    }
    Ok(PinId::Io(value as u8))
}

/// 解析 STM32 形式：`P<A-K><n>`。
fn parse_stm32(text: &str, bytes: &[u8]) -> Result<PinId, PinParseError> {
    if bytes.len() < 3 {
        return Err(PinParseError::InvalidSyntax);
    }
    let Some(port) = Port::from_ascii(bytes[1]) else {
        return Err(PinParseError::InvalidPort);
    };
    let digits = &text[2..];
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(PinParseError::InvalidSyntax);
    }
    if digits.len() > 1 && digits.as_bytes()[0] == b'0' {
        return Err(PinParseError::LeadingZero);
    }
    let mut value: u16 = 0;
    for byte in digits.bytes() {
        value = value
            .saturating_mul(10)
            .saturating_add(u16::from(byte - b'0'));
        if value > 15 {
            return Err(PinParseError::PinOutOfRange);
        }
    }
    Ok(PinId::Port {
        port,
        pin: value as u8,
    })
}

/// 引脚名语法错误；只描述语法本身，不描述家族归属。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PinParseError {
    /// 空字符串。
    Empty,
    /// 超过 8 字节。
    TooLong,
    /// 无法识别的语法（例如 `A5`、`P5`、`IO`）。
    InvalidSyntax,
    /// 端口字母不在 A..K。
    InvalidPort,
    /// 编号有前导零（`IO04`、`PA05`）。
    LeadingZero,
    /// ESP32 编号超过 39。
    IoOutOfRange,
    /// STM32 引脚编号超过 15。
    PinOutOfRange,
}

impl fmt::Display for PinParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => f.write_str("引脚名不能为空"),
            Self::TooLong => f.write_str("引脚名超过 8 字节"),
            Self::InvalidSyntax => f.write_str("无法识别的引脚名语法"),
            Self::InvalidPort => f.write_str("STM32 端口必须是 A..K"),
            Self::LeadingZero => f.write_str("引脚编号不能有前导零"),
            Self::IoOutOfRange => f.write_str("ESP32 引脚编号必须在 0..=39"),
            Self::PinOutOfRange => f.write_str("STM32 引脚编号必须在 0..=15"),
        }
    }
}

impl core::error::Error for PinParseError {}

/// 拼接引脚列表为范围压缩形式：`IO0-IO23,IO25-IO27,IO32-IO39`。
pub(crate) fn format_pin_ranges(pins: impl Iterator<Item = PinId>) -> String {
    fn consecutive(previous: PinId, next: PinId) -> bool {
        match (previous, next) {
            (PinId::Io(a), PinId::Io(b)) => b == a.saturating_add(1),
            (
                PinId::Port {
                    port: port_a,
                    pin: a,
                },
                PinId::Port {
                    port: port_b,
                    pin: b,
                },
            ) => port_a == port_b && b == a.saturating_add(1),
            _ => false,
        }
    }

    // 只有连续 3 个及以上的引脚才压缩为 `A-B`；两个连续引脚仍逐个列出，
    // 与文档中的 `PA13-PA15,PB3,PB4` 保持一致。
    fn render(result: &mut String, start: PinId, last: PinId, count: usize) {
        if !result.is_empty() {
            result.push(',');
        }
        if count >= 3 {
            let _ = write!(result, "{start}-{last}");
        } else if count == 2 {
            let _ = write!(result, "{start},{last}");
        } else {
            let _ = write!(result, "{start}");
        }
    }

    let mut result = String::new();
    let mut range: Option<(PinId, PinId, usize)> = None;
    for pin in pins {
        match range {
            None => range = Some((pin, pin, 1)),
            Some((start, last, count)) if consecutive(last, pin) => {
                range = Some((start, pin, count + 1));
            }
            Some((start, last, count)) => {
                render(&mut result, start, last, count);
                range = Some((pin, pin, 1));
            }
        }
    }
    if let Some((start, last, count)) = range {
        render(&mut result, start, last, count);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::{PinId, PinParseError, Port, format_pin_ranges};
    use alloc::string::ToString;

    #[test]
    fn parses_esp32_forms_case_insensitively() {
        for text in ["IO4", "GPIO4", "4", "io4", "gpio4"] {
            assert_eq!(PinId::parse(text), Ok(PinId::Io(4)), "text = {text:?}");
        }
        assert_eq!(PinId::parse("IO0"), Ok(PinId::Io(0)));
        assert_eq!(PinId::parse("39"), Ok(PinId::Io(39)));
        assert_eq!(PinId::parse("GPIO39"), Ok(PinId::Io(39)));
    }

    #[test]
    fn parses_stm32_forms_case_insensitively() {
        assert_eq!(PinId::parse("PA5"), Ok(PinId::port(Port::A, 5)));
        assert_eq!(PinId::parse("pa5"), Ok(PinId::port(Port::A, 5)));
        assert_eq!(PinId::parse("PJ0"), Ok(PinId::port(Port::J, 0)));
        assert_eq!(PinId::parse("PK15"), Ok(PinId::port(Port::K, 15)));
    }

    #[test]
    fn rejects_bad_syntax() {
        assert_eq!(PinId::parse(""), Err(PinParseError::Empty));
        assert_eq!(PinId::parse("A5"), Err(PinParseError::InvalidSyntax));
        assert_eq!(PinId::parse("P5"), Err(PinParseError::InvalidSyntax));
        assert_eq!(PinId::parse("IO"), Err(PinParseError::InvalidSyntax));
        assert_eq!(PinId::parse("P"), Err(PinParseError::InvalidSyntax));
        assert_eq!(PinId::parse("PA-1"), Err(PinParseError::InvalidSyntax));
        assert_eq!(PinId::parse("PAA1"), Err(PinParseError::InvalidSyntax));
        assert_eq!(PinId::parse("IO4x"), Err(PinParseError::InvalidSyntax));
        assert_eq!(PinId::parse("P#0"), Err(PinParseError::InvalidPort));
        assert_eq!(PinId::parse("P123456789"), Err(PinParseError::TooLong));
    }

    #[test]
    fn rejects_out_of_range_and_leading_zero() {
        assert_eq!(PinId::parse("IO04"), Err(PinParseError::LeadingZero));
        assert_eq!(PinId::parse("04"), Err(PinParseError::LeadingZero));
        assert_eq!(PinId::parse("PA05"), Err(PinParseError::LeadingZero));
        assert_eq!(PinId::parse("IO40"), Err(PinParseError::IoOutOfRange));
        assert_eq!(PinId::parse("999"), Err(PinParseError::IoOutOfRange));
        assert_eq!(PinId::parse("PA16"), Err(PinParseError::PinOutOfRange));
    }

    #[test]
    fn display_is_canonical() {
        assert_eq!(PinId::Io(4).to_string(), "IO4");
        assert_eq!(PinId::port(Port::A, 5).to_string(), "PA5");
        assert_eq!(PinId::Io(4).label().to_string(), "IO4");
        assert_eq!(Port::A.to_string(), "A");
        assert_eq!(Port::K.index(), 10);
        assert!(PinId::Io(1).is_io());
        assert!(!PinId::port(Port::B, 1).is_io());
    }

    #[test]
    fn ranges_are_compressed() {
        let pins: alloc::vec::Vec<PinId> = (0..=23)
            .chain(25..=27)
            .chain(32..=39)
            .map(PinId::io)
            .collect();
        assert_eq!(
            format_pin_ranges(pins.into_iter()),
            "IO0-IO23,IO25-IO27,IO32-IO39"
        );

        let stm32 = [
            PinId::port(Port::A, 13),
            PinId::port(Port::A, 14),
            PinId::port(Port::A, 15),
            PinId::port(Port::B, 3),
            PinId::port(Port::B, 4),
        ];
        assert_eq!(format_pin_ranges(stm32.into_iter()), "PA13-PA15,PB3,PB4");
        assert_eq!(format_pin_ranges(core::iter::empty()), "");
        assert_eq!(
            format_pin_ranges([PinId::port(Port::C, 13)].into_iter()),
            "PC13"
        );
    }
}
