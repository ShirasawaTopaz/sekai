//! MQTT 5.0 原因码（Reason Code）。
//!
//! 原因码用于 CONNACK、PUBACK/PUBREC/PUBREL/PUBCOMP、SUBACK、UNSUBACK、
//! DISCONNECT 与 AUTH 等报文的返回值；[ReasonCode::allowed_in] 描述
//! 原因码与报文类型的合法组合。

use core::fmt;

use crate::mqtt::header::PacketType;

/// MQTT 5.0 原因码。
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ReasonCode {
    /// 成功。
    Success = 0x00,
    /// 订阅成功且最大 QoS 为 1。
    GrantedQos1 = 0x01,
    /// 订阅成功且最大 QoS 为 2。
    GrantedQos2 = 0x02,
    /// 服务端断开连接并发送遗嘱消息。
    DisconnectWithWillMessage = 0x04,
    /// 没有匹配的订阅者。
    NoMatchingSubscribers = 0x10,
    /// 取消订阅时订阅原本不存在。
    NoSubscriptionExisted = 0x11,
    /// 继续增强认证。
    ContinueAuthentication = 0x18,
    /// 重新认证。
    ReAuthenticate = 0x19,
    /// 未指明错误。
    UnspecifiedError = 0x80,
    /// 报文格式非法。
    MalformedPacket = 0x81,
    /// 协议错误。
    ProtocolError = 0x82,
    /// 实现相关错误。
    ImplementationSpecificError = 0x83,
    /// 不支持的协议版本。
    UnsupportedProtocolVersion = 0x84,
    /// 客户端标识符非法。
    ClientIdentifierNotValid = 0x85,
    /// 用户名或密码错误。
    BadUserNameOrPassword = 0x86,
    /// 未授权。
    NotAuthorized = 0x87,
    /// 服务端不可用。
    ServerUnavailable = 0x88,
    /// 服务端忙。
    ServerBusy = 0x89,
    /// 被禁止连接。
    Banned = 0x8A,
    /// 服务端正在关闭。
    ServerShuttingDown = 0x8B,
    /// 认证方式错误。
    BadAuthenticationMethod = 0x8C,
    /// 保活超时。
    KeepAliveTimeout = 0x8D,
    /// 会话被接管。
    SessionTakenOver = 0x8E,
    /// 主题过滤器非法。
    TopicFilterInvalid = 0x8F,
    /// 主题名非法。
    TopicNameInvalid = 0x90,
    /// 包标识符已被使用。
    PacketIdentifierInUse = 0x91,
    /// 包标识符不存在。
    PacketIdentifierNotFound = 0x92,
    /// 超过接收上限。
    ReceiveMaximumExceeded = 0x93,
    /// 主题别名非法。
    TopicAliasInvalid = 0x94,
    /// 报文过大。
    PacketTooLarge = 0x95,
    /// 消息速率过高。
    MessageRateTooHigh = 0x96,
    /// 配额超限。
    QuotaExceeded = 0x97,
    /// 管理操作断开连接。
    AdministrativeAction = 0x98,
    /// 负载格式非法。
    PayloadFormatInvalid = 0x99,
    /// 不支持保留消息。
    RetainNotSupported = 0x9A,
    /// 不支持该 QoS。
    QosNotSupported = 0x9B,
    /// 使用另一台服务端。
    UseAnotherServer = 0x9C,
    /// 服务端已迁移。
    ServerMoved = 0x9D,
    /// 不支持共享订阅。
    SharedSubscriptionsNotSupported = 0x9E,
    /// 连接速率超限。
    ConnectionRateExceeded = 0x9F,
    /// 超过最大连接时间。
    MaximumConnectTime = 0xA0,
    /// 不支持订阅标识符。
    SubscriptionIdentifiersNotSupported = 0xA1,
    /// 不支持通配符订阅。
    WildcardSubscriptionsNotSupported = 0xA2,
}

impl ReasonCode {
    /// 转换为线上字节值。
    pub fn as_u8(self) -> u8 {
        self as u8
    }

    /// 是否为错误原因码（>= 0x80）。
    pub fn is_error(self) -> bool {
        self.as_u8() >= 0x80
    }

    /// 由线上字节值构造；非法原因码返回 `None`。
    pub fn try_from_u8(value: u8) -> Option<Self> {
        Some(match value {
            0x00 => Self::Success,
            0x01 => Self::GrantedQos1,
            0x02 => Self::GrantedQos2,
            0x04 => Self::DisconnectWithWillMessage,
            0x10 => Self::NoMatchingSubscribers,
            0x11 => Self::NoSubscriptionExisted,
            0x18 => Self::ContinueAuthentication,
            0x19 => Self::ReAuthenticate,
            0x80 => Self::UnspecifiedError,
            0x81 => Self::MalformedPacket,
            0x82 => Self::ProtocolError,
            0x83 => Self::ImplementationSpecificError,
            0x84 => Self::UnsupportedProtocolVersion,
            0x85 => Self::ClientIdentifierNotValid,
            0x86 => Self::BadUserNameOrPassword,
            0x87 => Self::NotAuthorized,
            0x88 => Self::ServerUnavailable,
            0x89 => Self::ServerBusy,
            0x8A => Self::Banned,
            0x8B => Self::ServerShuttingDown,
            0x8C => Self::BadAuthenticationMethod,
            0x8D => Self::KeepAliveTimeout,
            0x8E => Self::SessionTakenOver,
            0x8F => Self::TopicFilterInvalid,
            0x90 => Self::TopicNameInvalid,
            0x91 => Self::PacketIdentifierInUse,
            0x92 => Self::PacketIdentifierNotFound,
            0x93 => Self::ReceiveMaximumExceeded,
            0x94 => Self::TopicAliasInvalid,
            0x95 => Self::PacketTooLarge,
            0x96 => Self::MessageRateTooHigh,
            0x97 => Self::QuotaExceeded,
            0x98 => Self::AdministrativeAction,
            0x99 => Self::PayloadFormatInvalid,
            0x9A => Self::RetainNotSupported,
            0x9B => Self::QosNotSupported,
            0x9C => Self::UseAnotherServer,
            0x9D => Self::ServerMoved,
            0x9E => Self::SharedSubscriptionsNotSupported,
            0x9F => Self::ConnectionRateExceeded,
            0xA0 => Self::MaximumConnectTime,
            0xA1 => Self::SubscriptionIdentifiersNotSupported,
            0xA2 => Self::WildcardSubscriptionsNotSupported,
            _ => return None,
        })
    }

    /// 该原因码是否允许出现在指定报文类型中。
    ///
    /// 未在下表出现的报文类型一律返回 `false`。
    pub fn allowed_in(self, packet_type: PacketType) -> bool {
        match packet_type {
            PacketType::Connack => matches!(
                self.as_u8(),
                0x00 | 0x80
                    | 0x81
                    | 0x82
                    | 0x83
                    | 0x84
                    | 0x85
                    | 0x86
                    | 0x87
                    | 0x88
                    | 0x89
                    | 0x8A
                    | 0x8C
                    | 0x90
                    | 0x95
                    | 0x97
                    | 0x99
                    | 0x9A
                    | 0x9B
                    | 0x9C
                    | 0x9D
                    | 0x9F
            ),
            PacketType::Puback | PacketType::Pubrec => matches!(
                self.as_u8(),
                0x00 | 0x10 | 0x80 | 0x83 | 0x87 | 0x90 | 0x91 | 0x97 | 0x99
            ),
            PacketType::Pubrel | PacketType::Pubcomp => {
                matches!(self.as_u8(), 0x00 | 0x92)
            }
            PacketType::Suback => matches!(
                self.as_u8(),
                0x00 | 0x01 | 0x02 | 0x80 | 0x83 | 0x87 | 0x8F | 0x91 | 0x97 | 0x9E | 0xA1 | 0xA2
            ),
            PacketType::Unsuback => {
                matches!(self.as_u8(), 0x00 | 0x11 | 0x80 | 0x83 | 0x87 | 0x8F | 0x91)
            }
            PacketType::Disconnect => matches!(
                self.as_u8(),
                0x00 | 0x04
                    | 0x80
                    | 0x81
                    | 0x82
                    | 0x83
                    | 0x87
                    | 0x89
                    | 0x8B
                    | 0x8D
                    | 0x8E
                    | 0x8F
                    | 0x90
                    | 0x93
                    | 0x94
                    | 0x95
                    | 0x96
                    | 0x97
                    | 0x98
                    | 0x99
                    | 0x9A
                    | 0x9B
                    | 0x9C
                    | 0x9D
                    | 0x9E
                    | 0x9F
                    | 0xA0
                    | 0xA1
                    | 0xA2
            ),
            PacketType::Auth => matches!(self.as_u8(), 0x00 | 0x18 | 0x19),
            _ => false,
        }
    }

    /// 原因码的规范英文名称。
    fn name(self) -> &'static str {
        match self {
            Self::Success => "Success",
            Self::GrantedQos1 => "Granted QoS 1",
            Self::GrantedQos2 => "Granted QoS 2",
            Self::DisconnectWithWillMessage => "Disconnect with Will Message",
            Self::NoMatchingSubscribers => "No matching subscribers",
            Self::NoSubscriptionExisted => "No subscription existed",
            Self::ContinueAuthentication => "Continue authentication",
            Self::ReAuthenticate => "Re-authenticate",
            Self::UnspecifiedError => "Unspecified error",
            Self::MalformedPacket => "Malformed Packet",
            Self::ProtocolError => "Protocol Error",
            Self::ImplementationSpecificError => "Implementation specific error",
            Self::UnsupportedProtocolVersion => "Unsupported Protocol Version",
            Self::ClientIdentifierNotValid => "Client Identifier not valid",
            Self::BadUserNameOrPassword => "Bad User Name or Password",
            Self::NotAuthorized => "Not authorized",
            Self::ServerUnavailable => "Server unavailable",
            Self::ServerBusy => "Server busy",
            Self::Banned => "Banned",
            Self::ServerShuttingDown => "Server shutting down",
            Self::BadAuthenticationMethod => "Bad authentication method",
            Self::KeepAliveTimeout => "Keep Alive timeout",
            Self::SessionTakenOver => "Session taken over",
            Self::TopicFilterInvalid => "Topic Filter invalid",
            Self::TopicNameInvalid => "Topic Name invalid",
            Self::PacketIdentifierInUse => "Packet Identifier in use",
            Self::PacketIdentifierNotFound => "Packet Identifier not found",
            Self::ReceiveMaximumExceeded => "Receive Maximum exceeded",
            Self::TopicAliasInvalid => "Topic Alias invalid",
            Self::PacketTooLarge => "Packet too large",
            Self::MessageRateTooHigh => "Message rate too high",
            Self::QuotaExceeded => "Quota exceeded",
            Self::AdministrativeAction => "Administrative action",
            Self::PayloadFormatInvalid => "Payload format invalid",
            Self::RetainNotSupported => "Retain not supported",
            Self::QosNotSupported => "QoS not supported",
            Self::UseAnotherServer => "Use another server",
            Self::ServerMoved => "Server moved",
            Self::SharedSubscriptionsNotSupported => "Shared Subscriptions not supported",
            Self::ConnectionRateExceeded => "Connection rate exceeded",
            Self::MaximumConnectTime => "Maximum connect time",
            Self::SubscriptionIdentifiersNotSupported => "Subscription Identifiers not supported",
            Self::WildcardSubscriptionsNotSupported => "Wildcard Subscriptions not supported",
        }
    }
}

impl fmt::Display for ReasonCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} (0x{:02X})", self.name(), self.as_u8())
    }
}

#[cfg(test)]
mod tests {
    use super::ReasonCode;
    use crate::mqtt::header::PacketType;
    use alloc::string::ToString;

    /// 全部 43 个原因码。
    const ALL: [ReasonCode; 43] = [
        ReasonCode::Success,
        ReasonCode::GrantedQos1,
        ReasonCode::GrantedQos2,
        ReasonCode::DisconnectWithWillMessage,
        ReasonCode::NoMatchingSubscribers,
        ReasonCode::NoSubscriptionExisted,
        ReasonCode::ContinueAuthentication,
        ReasonCode::ReAuthenticate,
        ReasonCode::UnspecifiedError,
        ReasonCode::MalformedPacket,
        ReasonCode::ProtocolError,
        ReasonCode::ImplementationSpecificError,
        ReasonCode::UnsupportedProtocolVersion,
        ReasonCode::ClientIdentifierNotValid,
        ReasonCode::BadUserNameOrPassword,
        ReasonCode::NotAuthorized,
        ReasonCode::ServerUnavailable,
        ReasonCode::ServerBusy,
        ReasonCode::Banned,
        ReasonCode::ServerShuttingDown,
        ReasonCode::BadAuthenticationMethod,
        ReasonCode::KeepAliveTimeout,
        ReasonCode::SessionTakenOver,
        ReasonCode::TopicFilterInvalid,
        ReasonCode::TopicNameInvalid,
        ReasonCode::PacketIdentifierInUse,
        ReasonCode::PacketIdentifierNotFound,
        ReasonCode::ReceiveMaximumExceeded,
        ReasonCode::TopicAliasInvalid,
        ReasonCode::PacketTooLarge,
        ReasonCode::MessageRateTooHigh,
        ReasonCode::QuotaExceeded,
        ReasonCode::AdministrativeAction,
        ReasonCode::PayloadFormatInvalid,
        ReasonCode::RetainNotSupported,
        ReasonCode::QosNotSupported,
        ReasonCode::UseAnotherServer,
        ReasonCode::ServerMoved,
        ReasonCode::SharedSubscriptionsNotSupported,
        ReasonCode::ConnectionRateExceeded,
        ReasonCode::MaximumConnectTime,
        ReasonCode::SubscriptionIdentifiersNotSupported,
        ReasonCode::WildcardSubscriptionsNotSupported,
    ];

    #[test]
    fn as_u8_and_try_from_u8_roundtrip() {
        for reason in ALL {
            assert_eq!(
                ReasonCode::try_from_u8(reason.as_u8()),
                Some(reason),
                "reason = {reason}"
            );
        }
    }

    #[test]
    fn try_from_u8_rejects_unknown_values() {
        for value in [
            0x03u8, 0x05, 0x0F, 0x12, 0x14, 0x1B, 0x1C, 0x1E, 0x1F, 0xA3, 0xFF,
        ] {
            assert_eq!(ReasonCode::try_from_u8(value), None, "value = {value}");
        }
    }

    #[test]
    fn is_error_boundary() {
        for reason in ALL {
            assert_eq!(
                reason.is_error(),
                reason.as_u8() >= 0x80,
                "reason = {reason}"
            );
        }
        assert!(!ReasonCode::Success.is_error());
        assert!(!ReasonCode::GrantedQos2.is_error());
        assert!(ReasonCode::UnspecifiedError.is_error());
        assert!(ReasonCode::WildcardSubscriptionsNotSupported.is_error());
    }

    #[test]
    fn allowed_in_table() {
        use PacketType::{
            Auth, Connack, Disconnect, Puback, Pubcomp, Pubrec, Pubrel, Suback, Unsuback,
        };

        const CONNACK: &[u8] = &[
            0x00, 0x80, 0x81, 0x82, 0x83, 0x84, 0x85, 0x86, 0x87, 0x88, 0x89, 0x8A, 0x8C, 0x90,
            0x95, 0x97, 0x99, 0x9A, 0x9B, 0x9C, 0x9D, 0x9F,
        ];
        const PUBACK: &[u8] = &[0x00, 0x10, 0x80, 0x83, 0x87, 0x90, 0x91, 0x97, 0x99];
        const PUBREL: &[u8] = &[0x00, 0x92];
        const SUBACK: &[u8] = &[
            0x00, 0x01, 0x02, 0x80, 0x83, 0x87, 0x8F, 0x91, 0x97, 0x9E, 0xA1, 0xA2,
        ];
        const UNSUBACK: &[u8] = &[0x00, 0x11, 0x80, 0x83, 0x87, 0x8F, 0x91];
        const DISCONNECT: &[u8] = &[
            0x00, 0x04, 0x80, 0x81, 0x82, 0x83, 0x87, 0x89, 0x8B, 0x8D, 0x8E, 0x8F, 0x90, 0x93,
            0x94, 0x95, 0x96, 0x97, 0x98, 0x99, 0x9A, 0x9B, 0x9C, 0x9D, 0x9E, 0x9F, 0xA0, 0xA1,
            0xA2,
        ];
        const AUTH: &[u8] = &[0x00, 0x18, 0x19];

        let table: [(PacketType, &[u8]); 9] = [
            (Connack, CONNACK),
            (Puback, PUBACK),
            (Pubrec, PUBACK),
            (Pubrel, PUBREL),
            (Pubcomp, PUBREL),
            (Suback, SUBACK),
            (Unsuback, UNSUBACK),
            (Disconnect, DISCONNECT),
            (Auth, AUTH),
        ];
        for (packet_type, allowed) in table {
            for reason in ALL {
                let expected = allowed.contains(&reason.as_u8());
                assert_eq!(
                    reason.allowed_in(packet_type),
                    expected,
                    "{reason} 在 {packet_type}"
                );
            }
        }

        // 其余报文类型一律不允许任何原因码。
        for packet_type in [
            PacketType::Connect,
            PacketType::Publish,
            PacketType::Subscribe,
            PacketType::Unsubscribe,
            PacketType::Pingreq,
            PacketType::Pingresp,
        ] {
            for reason in ALL {
                assert!(
                    !reason.allowed_in(packet_type),
                    "{reason} 不应允许出现在 {packet_type}"
                );
            }
        }
    }

    #[test]
    fn display_form() {
        assert_eq!(ReasonCode::Success.to_string(), "Success (0x00)");
        assert_eq!(
            ReasonCode::ProtocolError.to_string(),
            "Protocol Error (0x82)"
        );
        assert_eq!(
            ReasonCode::PacketTooLarge.to_string(),
            "Packet too large (0x95)"
        );
        assert_eq!(
            ReasonCode::WildcardSubscriptionsNotSupported.to_string(),
            "Wildcard Subscriptions not supported (0xA2)"
        );
    }

    #[test]
    fn granted_qos_0_documented_via_success() {
        // SUBACK 中 0x00 同时表示 Granted QoS 0，即 Success 变体。
        assert_eq!(ReasonCode::Success.as_u8(), 0x00);
        assert!(ReasonCode::Success.allowed_in(PacketType::Suback));
        assert!(ReasonCode::Success.allowed_in(PacketType::Connack));
    }
}
