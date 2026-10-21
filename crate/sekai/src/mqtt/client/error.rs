//! 客户端错误类型。

use core::fmt;

use crate::mqtt::error::{DecodeError, EncodeError};
use crate::mqtt::header::PacketType;
use crate::mqtt::qos::QoS;
use crate::mqtt::reason::ReasonCode;

/// 客户端状态机错误。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientError {
    /// 当前状态不允许该操作（例如未连接时发布）。
    NotConnected,
    /// 已经连接或正在连接，不能重复连接。
    AlreadyConnected,
    /// 等待 CONNACK 超时。
    ConnectTimeout,
    /// Keep Alive 超时：发出 PINGREQ 后未在限定时间内收到 PINGRESP。
    KeepAliveTimeout,
    /// 包标识符（1..=65535）已全部被占用。
    PacketIdExhausted,
    /// 当前状态收到了不该出现的报文。
    UnexpectedPacket(PacketType),
    /// 报文中出现未分配或已释放的包标识符。
    UnknownPacketId(u16),
    /// 报文超过允许的最大长度。
    PacketTooLarge {
        /// 实际（或将要编码的）长度。
        size: usize,
        /// 允许的最大长度。
        max: u32,
    },
    /// 服务端在 CONNACK 中声明不支持该 QoS。
    QosNotSupported(QoS),
    /// 服务端在 CONNACK 中声明不支持保留消息。
    RetainNotSupported,
    /// 服务端在 CONNACK 中声明不支持通配符订阅。
    WildcardSubscriptionNotSupported,
    /// 服务端在 CONNACK 中声明不支持共享订阅。
    SharedSubscriptionNotSupported,
    /// 服务端在 CONNACK 中声明不支持订阅标识符。
    SubscriptionIdentifiersNotSupported,
    /// 入站 QoS2 未确认报文数超过客户端声明的 Receive Maximum。
    ReceiveMaximumExceeded,
    /// 主题别名非法（为 0、超上限或引用不存在的映射）。
    TopicAliasInvalid,
    /// 客户端配置非法。
    InvalidConfig(&'static str),
    /// 协议语义错误。
    Protocol(&'static str),
    /// 报文编码失败。
    Encode(EncodeError),
    /// 报文解码失败。
    Decode(DecodeError),
    /// 内存分配失败。
    AllocationFailed,
}

impl ClientError {
    /// 映射到 DISCONNECT 报文的原因码；没有合适原因码时返回 `None`。
    ///
    /// 调用方可在出错后据此构造 DISCONNECT（若传输仍然可用）。
    pub fn reason_code(&self) -> Option<ReasonCode> {
        match self {
            ClientError::UnexpectedPacket(_) | ClientError::Protocol(_) => {
                Some(ReasonCode::ProtocolError)
            }
            ClientError::UnknownPacketId(_) => Some(ReasonCode::PacketIdentifierNotFound),
            ClientError::PacketTooLarge { .. } => Some(ReasonCode::PacketTooLarge),
            ClientError::QosNotSupported(_) => Some(ReasonCode::QosNotSupported),
            ClientError::RetainNotSupported => Some(ReasonCode::RetainNotSupported),
            ClientError::ReceiveMaximumExceeded => Some(ReasonCode::ReceiveMaximumExceeded),
            ClientError::TopicAliasInvalid => Some(ReasonCode::TopicAliasInvalid),
            ClientError::Decode(DecodeError::Malformed(_)) => Some(ReasonCode::MalformedPacket),
            ClientError::Decode(DecodeError::ProtocolError(_)) => Some(ReasonCode::ProtocolError),
            ClientError::Decode(DecodeError::UnsupportedProtocolVersion(_)) => {
                Some(ReasonCode::UnsupportedProtocolVersion)
            }
            _ => None,
        }
    }
}

impl From<EncodeError> for ClientError {
    fn from(value: EncodeError) -> Self {
        ClientError::Encode(value)
    }
}

impl From<DecodeError> for ClientError {
    fn from(value: DecodeError) -> Self {
        ClientError::Decode(value)
    }
}

impl fmt::Display for ClientError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ClientError::NotConnected => write!(f, "客户端尚未连接"),
            ClientError::AlreadyConnected => write!(f, "客户端已连接或正在连接"),
            ClientError::ConnectTimeout => write!(f, "等待 CONNACK 超时"),
            ClientError::KeepAliveTimeout => write!(f, "Keep Alive 超时：未在限期内收到 PINGRESP"),
            ClientError::PacketIdExhausted => write!(f, "包标识符已耗尽（1..=65535 全部被占用）"),
            ClientError::UnexpectedPacket(packet_type) => {
                write!(f, "当前状态不应收到 {} 报文", packet_type)
            }
            ClientError::UnknownPacketId(packet_id) => {
                write!(f, "包标识符 {} 未知或已释放", packet_id)
            }
            ClientError::PacketTooLarge { size, max } => {
                write!(f, "报文长度 {} 超过允许上限 {}", size, max)
            }
            ClientError::QosNotSupported(qos) => write!(f, "服务端不支持 {}", qos),
            ClientError::RetainNotSupported => write!(f, "服务端不支持保留消息"),
            ClientError::WildcardSubscriptionNotSupported => write!(f, "服务端不支持通配符订阅"),
            ClientError::SharedSubscriptionNotSupported => write!(f, "服务端不支持共享订阅"),
            ClientError::SubscriptionIdentifiersNotSupported => {
                write!(f, "服务端不支持订阅标识符")
            }
            ClientError::ReceiveMaximumExceeded => {
                write!(f, "入站未确认 QoS2 报文数超过 Receive Maximum")
            }
            ClientError::TopicAliasInvalid => write!(f, "主题别名非法"),
            ClientError::InvalidConfig(message) => write!(f, "客户端配置非法：{}", message),
            ClientError::Protocol(message) => write!(f, "协议错误：{}", message),
            ClientError::Encode(err) => write!(f, "报文编码失败：{}", err),
            ClientError::Decode(err) => write!(f, "报文解码失败：{}", err),
            ClientError::AllocationFailed => write!(f, "内存分配失败"),
        }
    }
}

impl core::error::Error for ClientError {}
