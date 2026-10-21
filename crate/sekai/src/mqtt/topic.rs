//! MQTT 主题名、主题过滤器与订阅选项。
//!
//! 主题名用于 PUBLISH 等报文，不允许通配符；主题过滤器用于 SUBSCRIBE/UNSUBSCRIBE，
//! 支持 `+`、`#` 通配符与 `$share/{ShareName}/{filter}` 共享订阅形式。
//! 类型在构造时完成校验，后续编码不再重复校验。

use alloc::string::String;

use crate::mqtt::codec::{self, Reader, Sink};
use crate::mqtt::error::{DecodeError, EncodeError};
use crate::mqtt::qos::QoS;

/// 主题名：非空、合法 UTF-8、无 U+0000、长度 <= 65535 字节、不含 '+'/'#'。
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TopicName(String);

/// 校验主题名是否合法，返回中文静态描述。
fn validate_topic_name(value: &str) -> Result<(), &'static str> {
    if value.is_empty() {
        return Err("主题名不能为空");
    }
    if value.as_bytes().contains(&0) {
        return Err("主题名不能包含 U+0000");
    }
    if value.contains('+') {
        return Err("主题名不能包含通配符 '+'");
    }
    if value.contains('#') {
        return Err("主题名不能包含通配符 '#'");
    }
    if value.len() > u16::MAX as usize {
        return Err("主题名长度超过 65535 字节");
    }
    Ok(())
}

impl TopicName {
    /// 构造主题名；校验失败返回中文静态描述。
    pub fn new(value: impl Into<String>) -> Result<Self, &'static str> {
        let value = value.into();
        validate_topic_name(&value)?;
        Ok(Self(value))
    }

    /// 主题名字符串。
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// 取出底层字符串。
    pub fn into_string(self) -> String {
        self.0
    }

    /// 从报文读取主题名（u16 长度前缀的 UTF-8 字符串）。
    ///
    /// UTF-8/长度问题为 `Malformed`（由 Reader 处理），主题语义非法为 `ProtocolError`。
    pub(crate) fn decode(reader: &mut Reader<'_>) -> Result<Self, DecodeError> {
        let value = reader.utf8()?;
        Self::new(value).map_err(DecodeError::ProtocolError)
    }

    /// 写出主题名（u16 长度前缀的 UTF-8 字符串）。
    pub(crate) fn encode_into<S: Sink + ?Sized>(&self, sink: &mut S) -> Result<(), EncodeError> {
        if let Err(message) = validate_topic_name(&self.0) {
            return Err(EncodeError::Invalid(message));
        }
        codec::write_utf8(sink, &self.0)
    }
}

/// 主题过滤器：非空、合法 UTF-8、无 U+0000、长度 <= 65535 字节；
/// '#' 只能独占最后一个层级；'+' 必须独占一个层级；支持 `$share/{ShareName}/{filter}`。
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TopicFilter(String);

/// 校验通配符层级规则。
fn validate_filter_levels(value: &str) -> Result<(), &'static str> {
    let mut levels = value.split('/').peekable();
    while let Some(level) = levels.next() {
        if level.contains('#') {
            if level != "#" {
                return Err("'#' 必须独占一个层级");
            }
            if levels.peek().is_some() {
                return Err("'#' 只能出现在过滤器的最后一级");
            }
        }
        if level.contains('+') && level != "+" {
            return Err("'+' 必须独占一个层级");
        }
    }
    Ok(())
}

/// 校验主题过滤器是否合法，返回中文静态描述。
fn validate_topic_filter(value: &str) -> Result<(), &'static str> {
    if value.is_empty() {
        return Err("主题过滤器不能为空");
    }
    if value.as_bytes().contains(&0) {
        return Err("主题过滤器不能包含 U+0000");
    }
    if value.len() > u16::MAX as usize {
        return Err("主题过滤器长度超过 65535 字节");
    }
    if let Some(rest) = value.strip_prefix("$share/") {
        let Some(index) = rest.find('/') else {
            return Err("共享订阅格式应为 $share/{ShareName}/{过滤器}");
        };
        let share_name = &rest[..index];
        let filter = &rest[index + 1..];
        if share_name.is_empty() {
            return Err("共享订阅 ShareName 不能为空");
        }
        if share_name.contains('+') || share_name.contains('#') {
            return Err("共享订阅 ShareName 不能包含通配符");
        }
        if filter.is_empty() {
            return Err("共享订阅的过滤器不能为空");
        }
        if filter.starts_with("$share/") {
            return Err("共享订阅的过滤器不能再以 $share/ 开头");
        }
        validate_filter_levels(filter)?;
    } else {
        validate_filter_levels(value)?;
    }
    Ok(())
}

impl TopicFilter {
    /// 构造主题过滤器；校验失败返回中文静态描述。
    pub fn new(value: impl Into<String>) -> Result<Self, &'static str> {
        let value = value.into();
        validate_topic_filter(&value)?;
        Ok(Self(value))
    }

    /// 主题过滤器字符串。
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// 取出底层字符串。
    pub fn into_string(self) -> String {
        self.0
    }

    /// 是否 `$share/` 共享订阅形式。
    pub fn is_shared(&self) -> bool {
        self.0.starts_with("$share/")
    }

    /// 是否包含 '+' 或 '#'（客户端做通配符能力检查用）。
    pub fn has_wildcard(&self) -> bool {
        self.0.contains('+') || self.0.contains('#')
    }

    /// 从报文读取主题过滤器。
    pub(crate) fn decode(reader: &mut Reader<'_>) -> Result<Self, DecodeError> {
        let value = reader.utf8()?;
        Self::new(value).map_err(DecodeError::ProtocolError)
    }

    /// 写出主题过滤器。
    pub(crate) fn encode_into<S: Sink + ?Sized>(&self, sink: &mut S) -> Result<(), EncodeError> {
        if let Err(message) = validate_topic_filter(&self.0) {
            return Err(EncodeError::Invalid(message));
        }
        codec::write_utf8(sink, &self.0)
    }
}

/// SUBSCRIBE 报文中单个订阅选项。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SubscriptionOptions {
    /// 最大 QoS。
    pub qos: QoS,
    /// 不接收自己发布的消息。
    pub no_local: bool,
    /// 保留发布时的 Retain 标志。
    pub retain_as_published: bool,
    /// Retain Handling：0/1/2；构造时校验，字段保持 u8。
    pub retain_handling: u8,
}

impl SubscriptionOptions {
    /// 构造订阅选项；retain_handling > 2 返回 `Err`。
    pub fn new(
        qos: QoS,
        no_local: bool,
        retain_as_published: bool,
        retain_handling: u8,
    ) -> Result<Self, &'static str> {
        if retain_handling > 2 {
            return Err("Retain Handling 必须为 0、1 或 2");
        }
        Ok(Self {
            qos,
            no_local,
            retain_as_published,
            retain_handling,
        })
    }

    /// 线上单字节：bit0..1 QoS，bit2 no_local，bit3 retain_as_published，
    /// bit4..5 retain_handling，bit6..7 = 0。
    pub fn to_byte(self) -> u8 {
        self.qos.as_u8()
            | (u8::from(self.no_local) << 2)
            | (u8::from(self.retain_as_published) << 3)
            | (self.retain_handling << 4)
    }
}

impl TryFrom<u8> for SubscriptionOptions {
    type Error = &'static str;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        if value & 0b1100_0000 != 0 {
            return Err("订阅选项保留位必须为 0");
        }
        let qos = QoS::from_u8(value & 0b11).ok_or("订阅选项 QoS 不能为 3")?;
        let retain_handling = (value >> 4) & 0b11;
        if retain_handling > 2 {
            return Err("订阅选项 Retain Handling 不能为 3");
        }
        Self::new(
            qos,
            value & 0b100 != 0,
            value & 0b1000 != 0,
            retain_handling,
        )
    }
}

/// SUBSCRIBE 报文中的一条订阅（过滤器 + 选项）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Subscription {
    /// 主题过滤器。
    pub filter: TopicFilter,
    /// 订阅选项。
    pub options: SubscriptionOptions,
}

#[cfg(test)]
mod tests {
    use super::{Subscription, SubscriptionOptions, TopicFilter, TopicName};
    use crate::mqtt::codec::{Reader, VecSink};
    use crate::mqtt::error::DecodeError;
    use crate::mqtt::qos::QoS;
    use alloc::format;
    use alloc::vec::Vec;

    fn encode_name(name: &TopicName) -> Vec<u8> {
        let mut out = Vec::new();
        {
            let mut sink = VecSink::new(&mut out);
            name.encode_into(&mut sink).unwrap();
        }
        out
    }

    fn decode_name(bytes: &[u8]) -> Result<TopicName, DecodeError> {
        let mut reader = Reader::new(bytes);
        let name = TopicName::decode(&mut reader)?;
        reader.finish().unwrap();
        Ok(name)
    }

    fn encode_filter(filter: &TopicFilter) -> Vec<u8> {
        let mut out = Vec::new();
        {
            let mut sink = VecSink::new(&mut out);
            filter.encode_into(&mut sink).unwrap();
        }
        out
    }

    fn decode_filter(bytes: &[u8]) -> Result<TopicFilter, DecodeError> {
        let mut reader = Reader::new(bytes);
        let filter = TopicFilter::decode(&mut reader)?;
        reader.finish().unwrap();
        Ok(filter)
    }

    #[test]
    fn topic_name_accepts_valid() {
        for text in [
            "a",
            "a/b",
            "/finance",
            "a//b",
            "sport/tennis",
            "主题/名称",
            "$SYS/broker",
        ] {
            let name = TopicName::new(text).unwrap();
            assert_eq!(name.as_str(), text);
            assert_eq!(name.clone().into_string(), text);
            let bytes = encode_name(&name);
            assert_eq!(bytes.len(), text.len() + 2);
            assert_eq!(decode_name(&bytes).unwrap(), name);
        }
    }

    #[test]
    fn topic_name_rejects_invalid() {
        let cases: [(&str, &str); 4] = [
            ("", "主题名不能为空"),
            ("a+b", "主题名不能包含通配符 '+'"),
            ("a#", "主题名不能包含通配符 '#'"),
            ("a\0b", "主题名不能包含 U+0000"),
        ];
        for (text, expected) in cases {
            assert_eq!(TopicName::new(text), Err(expected), "text = {text:?}");
        }
        let too_long = "a".repeat(65_536);
        assert_eq!(TopicName::new(too_long), Err("主题名长度超过 65535 字节"));
        let max_len = "a".repeat(65_535);
        assert_eq!(TopicName::new(max_len).unwrap().as_str().len(), 65_535);
    }

    #[test]
    fn topic_name_decode_errors() {
        // 语义非法（含通配符）映射为 ProtocolError。
        assert_eq!(
            decode_name(&[0x00, 0x03, b'a', b'+', b'b']),
            Err(DecodeError::ProtocolError("主题名不能包含通配符 '+'"))
        );
        // 空主题名为 ProtocolError。
        assert_eq!(
            decode_name(&[0x00, 0x00]),
            Err(DecodeError::ProtocolError("主题名不能为空"))
        );
        // 含 U+0000 的 UTF-8 字符串为 Malformed（由 Reader 处理）。
        assert_eq!(
            decode_name(&[0x00, 0x03, b'a', 0x00, b'b']),
            Err(DecodeError::Malformed("UTF-8 字符串包含 U+0000"))
        );
        // 长度不足为 Malformed。
        assert_eq!(
            decode_name(&[0x00, 0x05, b'a']),
            Err(DecodeError::Malformed("字段长度超出报文剩余长度"))
        );
    }

    #[test]
    fn topic_filter_accepts_valid() {
        for text in [
            "#",
            "+",
            "sport/#",
            "sport/tennis/#",
            "sport/+",
            "+/tennis/#",
            "+/+",
            "/finance",
            "a//b",
            "$share/group/sport/#",
            "$share/group/a/+",
            "$share/group/+",
            "$share/group/#",
        ] {
            let filter = TopicFilter::new(text).unwrap();
            assert_eq!(filter.as_str(), text);
            assert_eq!(filter.clone().into_string(), text);
            let bytes = encode_filter(&filter);
            assert_eq!(decode_filter(&bytes).unwrap(), filter);
        }
    }

    #[test]
    fn topic_filter_rejects_invalid() {
        let cases: [(&str, &str); 16] = [
            ("", "主题过滤器不能为空"),
            ("sport#", "'#' 必须独占一个层级"),
            ("#/x", "'#' 只能出现在过滤器的最后一级"),
            ("a/#/b", "'#' 只能出现在过滤器的最后一级"),
            ("##", "'#' 必须独占一个层级"),
            ("a+", "'+' 必须独占一个层级"),
            ("a/+b", "'+' 必须独占一个层级"),
            ("a/b+/c", "'+' 必须独占一个层级"),
            ("$share//x", "共享订阅 ShareName 不能为空"),
            (
                "$share/group",
                "共享订阅格式应为 $share/{ShareName}/{过滤器}",
            ),
            ("$share/group/", "共享订阅的过滤器不能为空"),
            (
                "$share/group/$share/other/x",
                "共享订阅的过滤器不能再以 $share/ 开头",
            ),
            ("$share/gr+oup/x", "共享订阅 ShareName 不能包含通配符"),
            ("$share/gr#oup/x", "共享订阅 ShareName 不能包含通配符"),
            ("a\0b", "主题过滤器不能包含 U+0000"),
            ("#/sport", "'#' 只能出现在过滤器的最后一级"),
        ];
        for (text, expected) in cases {
            assert_eq!(TopicFilter::new(text), Err(expected), "text = {text:?}");
        }
        let too_long = "a".repeat(65_536);
        assert_eq!(
            TopicFilter::new(too_long),
            Err("主题过滤器长度超过 65535 字节")
        );
    }

    #[test]
    fn topic_filter_shared_and_wildcard() {
        let shared = TopicFilter::new("$share/group/sport/#").unwrap();
        assert!(shared.is_shared());
        assert!(shared.has_wildcard());

        let plain = TopicFilter::new("sport/tennis").unwrap();
        assert!(!plain.is_shared());
        assert!(!plain.has_wildcard());

        let share_literal = TopicFilter::new("$share").unwrap();
        assert!(!share_literal.is_shared());
        assert!(!share_literal.has_wildcard());

        let wildcard = TopicFilter::new("+").unwrap();
        assert!(wildcard.has_wildcard());
        assert!(!wildcard.is_shared());
    }

    #[test]
    fn topic_filter_decode_errors() {
        assert_eq!(
            decode_filter(&[0x00, 0x03, b'a', b'#', b'b']),
            Err(DecodeError::ProtocolError("'#' 必须独占一个层级"))
        );
        assert_eq!(
            decode_filter(&[0x00, 0x00]),
            Err(DecodeError::ProtocolError("主题过滤器不能为空"))
        );
        assert_eq!(
            decode_filter(&[0x00, 0x03, b'a', 0x00, b'b']),
            Err(DecodeError::Malformed("UTF-8 字符串包含 U+0000"))
        );
        assert_eq!(
            decode_filter(&[0x00, 0x09, b'a']),
            Err(DecodeError::Malformed("字段长度超出报文剩余长度"))
        );
    }

    #[test]
    fn subscription_options_new_validates() {
        assert_eq!(
            SubscriptionOptions::new(QoS::AtMostOnce, false, false, 3),
            Err("Retain Handling 必须为 0、1 或 2")
        );
        let options = SubscriptionOptions::new(QoS::AtLeastOnce, true, true, 2).unwrap();
        assert_eq!(options.to_byte(), 0b0010_1101);
    }

    #[test]
    fn subscription_options_roundtrip() {
        for qos in [QoS::AtMostOnce, QoS::AtLeastOnce, QoS::ExactlyOnce] {
            for no_local in [false, true] {
                for retain_as_published in [false, true] {
                    for retain_handling in 0u8..=2 {
                        let options = SubscriptionOptions::new(
                            qos,
                            no_local,
                            retain_as_published,
                            retain_handling,
                        )
                        .unwrap();
                        let byte = options.to_byte();
                        let decoded: Result<SubscriptionOptions, &'static str> =
                            SubscriptionOptions::try_from(byte);
                        assert_eq!(decoded, Ok(options), "byte = {byte:#04X}");
                    }
                }
            }
        }
    }

    #[test]
    fn subscription_options_try_from_rejects_invalid() {
        // QoS = 3。
        assert_eq!(
            SubscriptionOptions::try_from(0b0000_0011),
            Err("订阅选项 QoS 不能为 3")
        );
        // Retain Handling = 3。
        assert_eq!(
            SubscriptionOptions::try_from(0b0011_0000),
            Err("订阅选项 Retain Handling 不能为 3")
        );
        // 保留位非 0（bit6 置位优先于 Retain Handling 检查）。
        for byte in [0b0100_0000u8, 0b1000_0000, 0b1100_0000] {
            assert_eq!(
                SubscriptionOptions::try_from(byte),
                Err("订阅选项保留位必须为 0")
            );
        }
    }

    #[test]
    fn subscription_struct() {
        let subscription = Subscription {
            filter: TopicFilter::new("sport/+").unwrap(),
            options: SubscriptionOptions::new(QoS::ExactlyOnce, false, true, 1).unwrap(),
        };
        assert_eq!(subscription.filter.as_str(), "sport/+");
        assert_eq!(subscription.options.qos, QoS::ExactlyOnce);
        assert_eq!(subscription.options.retain_handling, 1);
        assert!(format!("{subscription:?}").contains("sport/+"));
    }
}
