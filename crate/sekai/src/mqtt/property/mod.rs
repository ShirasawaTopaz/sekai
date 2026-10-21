//! MQTT 5.0 属性（Properties）。
//!
//! 属性以“属性段”形式出现在报文中：先是一个 Variable Byte Integer 表示属性段
//! 内容的字节数，随后是若干“属性 ID（VarInt）+ 值”的序列。不同报文类型允许
//! 出现的属性集合不同（见 [PropertyContext] 的允许集规则）。

use alloc::string::String;
use alloc::vec::Vec;

use crate::mqtt::codec::{
    self, CountSink, Reader, Sink, write_u8, write_u16, write_u32, write_varint,
};
use crate::mqtt::error::{DecodeError, EncodeError};
use crate::mqtt::topic::TopicName;
use crate::mqtt::varint;

/// MQTT 5.0 属性标识符。
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PropertyId {
    /// 负载格式指示（0x01）。
    PayloadFormatIndicator = 0x01,
    /// 消息过期时间（0x02）。
    MessageExpiryInterval = 0x02,
    /// 内容类型（0x03）。
    ContentType = 0x03,
    /// 响应主题（0x08）。
    ResponseTopic = 0x08,
    /// 关联数据（0x09）。
    CorrelationData = 0x09,
    /// 订阅标识符（0x0B）。
    SubscriptionIdentifier = 0x0B,
    /// 会话过期时间（0x11）。
    SessionExpiryInterval = 0x11,
    /// 服务端分配的客户端标识符（0x12）。
    AssignedClientIdentifier = 0x12,
    /// 服务端保活时间（0x13）。
    ServerKeepAlive = 0x13,
    /// 认证方式（0x15）。
    AuthenticationMethod = 0x15,
    /// 认证数据（0x16）。
    AuthenticationData = 0x16,
    /// 请求问题信息（0x17）。
    RequestProblemInformation = 0x17,
    /// 遗嘱延迟时间（0x18）。
    WillDelayInterval = 0x18,
    /// 请求响应信息（0x19）。
    RequestResponseInformation = 0x19,
    /// 响应信息（0x1A）。
    ResponseInformation = 0x1A,
    /// 服务端参考（0x1C）。
    ServerReference = 0x1C,
    /// 原因字符串（0x1F）。
    ReasonString = 0x1F,
    /// 接收上限（0x21）。
    ReceiveMaximum = 0x21,
    /// 主题别名上限（0x22）。
    TopicAliasMaximum = 0x22,
    /// 主题别名（0x23）。
    TopicAlias = 0x23,
    /// 最大 QoS（0x24）。
    MaximumQos = 0x24,
    /// 是否支持保留消息（0x25）。
    RetainAvailable = 0x25,
    /// 用户属性（0x26）。
    UserProperty = 0x26,
    /// 最大报文大小（0x27）。
    MaximumPacketSize = 0x27,
    /// 是否支持通配符订阅（0x28）。
    WildcardSubscriptionAvailable = 0x28,
    /// 是否支持订阅标识符（0x29）。
    SubscriptionIdentifierAvailable = 0x29,
    /// 是否支持共享订阅（0x2A）。
    SharedSubscriptionAvailable = 0x2A,
}

impl PropertyId {
    /// 转换为线上标识符数值。
    pub fn as_u32(self) -> u32 {
        self as u32
    }

    /// 由线上标识符数值构造；未知属性 ID 返回 `None`。
    pub fn try_from_u32(value: u32) -> Option<Self> {
        Some(match value {
            0x01 => Self::PayloadFormatIndicator,
            0x02 => Self::MessageExpiryInterval,
            0x03 => Self::ContentType,
            0x08 => Self::ResponseTopic,
            0x09 => Self::CorrelationData,
            0x0B => Self::SubscriptionIdentifier,
            0x11 => Self::SessionExpiryInterval,
            0x12 => Self::AssignedClientIdentifier,
            0x13 => Self::ServerKeepAlive,
            0x15 => Self::AuthenticationMethod,
            0x16 => Self::AuthenticationData,
            0x17 => Self::RequestProblemInformation,
            0x18 => Self::WillDelayInterval,
            0x19 => Self::RequestResponseInformation,
            0x1A => Self::ResponseInformation,
            0x1C => Self::ServerReference,
            0x1F => Self::ReasonString,
            0x21 => Self::ReceiveMaximum,
            0x22 => Self::TopicAliasMaximum,
            0x23 => Self::TopicAlias,
            0x24 => Self::MaximumQos,
            0x25 => Self::RetainAvailable,
            0x26 => Self::UserProperty,
            0x27 => Self::MaximumPacketSize,
            0x28 => Self::WildcardSubscriptionAvailable,
            0x29 => Self::SubscriptionIdentifierAvailable,
            0x2A => Self::SharedSubscriptionAvailable,
            _ => return None,
        })
    }
}

/// 属性出现场景；PUBACK/PUBREC/PUBREL/PUBCOMP 各有独立变体以便应用允许集。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PropertyContext {
    /// CONNECT 报文。
    Connect,
    /// CONNECT 中的遗嘱属性。
    Will,
    /// CONNACK 报文。
    Connack,
    /// PUBLISH 报文。
    Publish,
    /// PUBACK 报文。
    Puback,
    /// PUBREC 报文。
    Pubrec,
    /// PUBREL 报文。
    Pubrel,
    /// PUBCOMP 报文。
    Pubcomp,
    /// SUBSCRIBE 报文。
    Subscribe,
    /// SUBACK 报文。
    Suback,
    /// UNSUBSCRIBE 报文。
    Unsubscribe,
    /// UNSUBACK 报文。
    Unsuback,
    /// DISCONNECT 报文。
    Disconnect,
    /// AUTH 报文。
    Auth,
}

/// 各场景允许出现的属性 ID 集合。
fn allowed_ids(context: PropertyContext) -> &'static [PropertyId] {
    use PropertyId::{
        AssignedClientIdentifier, AuthenticationData, AuthenticationMethod, ContentType,
        CorrelationData, MaximumPacketSize, MaximumQos, MessageExpiryInterval,
        PayloadFormatIndicator, ReasonString, ReceiveMaximum, RequestProblemInformation,
        RequestResponseInformation, ResponseInformation, ResponseTopic, RetainAvailable,
        ServerKeepAlive, ServerReference, SessionExpiryInterval, SharedSubscriptionAvailable,
        SubscriptionIdentifier, SubscriptionIdentifierAvailable, TopicAlias, TopicAliasMaximum,
        UserProperty, WildcardSubscriptionAvailable, WillDelayInterval,
    };
    match context {
        PropertyContext::Connect => &[
            SessionExpiryInterval,
            ReceiveMaximum,
            MaximumPacketSize,
            TopicAliasMaximum,
            RequestResponseInformation,
            RequestProblemInformation,
            UserProperty,
            AuthenticationMethod,
            AuthenticationData,
        ],
        PropertyContext::Will => &[
            WillDelayInterval,
            PayloadFormatIndicator,
            MessageExpiryInterval,
            ContentType,
            ResponseTopic,
            CorrelationData,
            UserProperty,
        ],
        PropertyContext::Connack => &[
            SessionExpiryInterval,
            ReceiveMaximum,
            MaximumQos,
            RetainAvailable,
            MaximumPacketSize,
            AssignedClientIdentifier,
            TopicAliasMaximum,
            ReasonString,
            UserProperty,
            WildcardSubscriptionAvailable,
            SubscriptionIdentifierAvailable,
            SharedSubscriptionAvailable,
            ServerKeepAlive,
            ResponseInformation,
            ServerReference,
            AuthenticationMethod,
            AuthenticationData,
        ],
        PropertyContext::Publish => &[
            PayloadFormatIndicator,
            MessageExpiryInterval,
            TopicAlias,
            ResponseTopic,
            CorrelationData,
            UserProperty,
            SubscriptionIdentifier,
            ContentType,
        ],
        PropertyContext::Puback
        | PropertyContext::Pubrec
        | PropertyContext::Pubrel
        | PropertyContext::Pubcomp => &[ReasonString, UserProperty],
        PropertyContext::Subscribe => &[SubscriptionIdentifier, UserProperty],
        PropertyContext::Suback => &[ReasonString, UserProperty],
        PropertyContext::Unsubscribe => &[UserProperty],
        PropertyContext::Unsuback => &[ReasonString, UserProperty],
        PropertyContext::Disconnect => &[
            SessionExpiryInterval,
            ReasonString,
            UserProperty,
            ServerReference,
        ],
        PropertyContext::Auth => &[
            AuthenticationMethod,
            AuthenticationData,
            ReasonString,
            UserProperty,
        ],
    }
}

/// 属性是否允许出现在指定场景。
fn is_allowed(id: PropertyId, context: PropertyContext) -> bool {
    allowed_ids(context).contains(&id)
}

/// 校验属性允许集，返回中文静态描述。
fn check_allowed(id: PropertyId, context: PropertyContext) -> Result<(), &'static str> {
    if is_allowed(id, context) {
        Ok(())
    } else {
        Err("属性不允许出现在该报文中")
    }
}

/// 编码前校验属性允许集。
fn require_allowed(id: PropertyId, context: PropertyContext) -> Result<(), EncodeError> {
    check_allowed(id, context).map_err(EncodeError::Invalid)
}

/// 写入属性 ID（VarInt）。
fn write_id<S: Sink + ?Sized>(sink: &mut S, id: PropertyId) {
    write_varint(sink, id.as_u32());
}

/// 读取布尔属性值：只接受 0/1。
fn decode_bool(reader: &mut Reader<'_>) -> Result<bool, DecodeError> {
    match reader.u8()? {
        0 => Ok(false),
        1 => Ok(true),
        _ => Err(DecodeError::ProtocolError("布尔属性取值必须为 0 或 1")),
    }
}

/// 读取非零 u16 属性值。
fn decode_nonzero_u16(reader: &mut Reader<'_>, message: &'static str) -> Result<u16, DecodeError> {
    let value = reader.u16()?;
    if value == 0 {
        return Err(DecodeError::ProtocolError(message));
    }
    Ok(value)
}

/// 读取非零 u32 属性值。
fn decode_nonzero_u32(reader: &mut Reader<'_>, message: &'static str) -> Result<u32, DecodeError> {
    let value = reader.u32()?;
    if value == 0 {
        return Err(DecodeError::ProtocolError(message));
    }
    Ok(value)
}

/// 写入单值属性槽位：重复出现时返回协议错误。
fn set_once<T>(slot: &mut Option<T>, value: T) -> Result<(), DecodeError> {
    if slot.is_some() {
        return Err(DecodeError::ProtocolError("属性重复出现"));
    }
    *slot = Some(value);
    Ok(())
}

/// MQTT 5.0 属性集合。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Properties {
    /// 0x01 负载格式指示。
    pub payload_format_indicator: Option<bool>,
    /// 0x02 消息过期时间。
    pub message_expiry_interval: Option<u32>,
    /// 0x03 内容类型。
    pub content_type: Option<String>,
    /// 0x08 响应主题。
    pub response_topic: Option<TopicName>,
    /// 0x09 关联数据。
    pub correlation_data: Option<Vec<u8>>,
    /// 0x0B 订阅标识符（PUBLISH 可多个，SUBSCRIBE 至多 1 个）。
    pub subscription_identifier: Vec<u32>,
    /// 0x11 会话过期时间。
    pub session_expiry_interval: Option<u32>,
    /// 0x12 服务端分配的客户端标识符（非空）。
    pub assigned_client_identifier: Option<String>,
    /// 0x13 服务端保活时间。
    pub server_keep_alive: Option<u16>,
    /// 0x15 认证方式。
    pub authentication_method: Option<String>,
    /// 0x16 认证数据。
    pub authentication_data: Option<Vec<u8>>,
    /// 0x17 请求问题信息。
    pub request_problem_information: Option<bool>,
    /// 0x18 遗嘱延迟时间。
    pub will_delay_interval: Option<u32>,
    /// 0x19 请求响应信息。
    pub request_response_information: Option<bool>,
    /// 0x1A 响应信息。
    pub response_information: Option<String>,
    /// 0x1C 服务端参考。
    pub server_reference: Option<String>,
    /// 0x1F 原因字符串。
    pub reason_string: Option<String>,
    /// 0x21 接收上限（1..=65535）。
    pub receive_maximum: Option<u16>,
    /// 0x22 主题别名上限。
    pub topic_alias_maximum: Option<u16>,
    /// 0x23 主题别名（1..=65535）。
    pub topic_alias: Option<u16>,
    /// 0x24 最大 QoS（0/1）。
    pub maximum_qos: Option<u8>,
    /// 0x25 是否支持保留消息。
    pub retain_available: Option<bool>,
    /// 0x26 用户属性（可重复）。
    pub user_property: Vec<(String, String)>,
    /// 0x27 最大报文大小（1..=u32::MAX）。
    pub maximum_packet_size: Option<u32>,
    /// 0x28 是否支持通配符订阅。
    pub wildcard_subscription_available: Option<bool>,
    /// 0x29 是否支持订阅标识符。
    pub subscription_identifier_available: Option<bool>,
    /// 0x2A 是否支持共享订阅。
    pub shared_subscription_available: Option<bool>,
}

impl Properties {
    /// 创建空属性集合。
    pub fn new() -> Self {
        Self::default()
    }

    /// 是否不含任何属性。
    pub fn is_empty(&self) -> bool {
        self == &Self::default()
    }

    /// 读取属性段（先读 property length VarInt 再读内容），必须恰好消费完属性段。
    pub(crate) fn decode(
        reader: &mut Reader<'_>,
        context: PropertyContext,
    ) -> Result<Self, DecodeError> {
        let content_length = reader.varint()? as usize;
        let content = reader.take(content_length)?;
        let mut sub = Reader::new(content);
        let mut properties = Self::new();
        while !sub.is_empty() {
            let raw_id = sub.varint()?;
            let id =
                PropertyId::try_from_u32(raw_id).ok_or(DecodeError::Malformed("未知属性 ID"))?;
            check_allowed(id, context).map_err(DecodeError::ProtocolError)?;
            match id {
                PropertyId::PayloadFormatIndicator => {
                    set_once(
                        &mut properties.payload_format_indicator,
                        decode_bool(&mut sub)?,
                    )?;
                }
                PropertyId::MessageExpiryInterval => {
                    set_once(&mut properties.message_expiry_interval, sub.u32()?)?;
                }
                PropertyId::ContentType => {
                    set_once(&mut properties.content_type, sub.utf8()?)?;
                }
                PropertyId::ResponseTopic => {
                    set_once(&mut properties.response_topic, TopicName::decode(&mut sub)?)?;
                }
                PropertyId::CorrelationData => {
                    set_once(&mut properties.correlation_data, sub.binary()?)?;
                }
                PropertyId::SubscriptionIdentifier => {
                    let value = sub.varint()?;
                    if value == 0 {
                        return Err(DecodeError::ProtocolError(
                            "Subscription Identifier 不能为 0",
                        ));
                    }
                    // PUBLISH 中可重复，SUBSCRIBE 中至多一次（其余场景不允许出现）。
                    if !matches!(context, PropertyContext::Publish)
                        && !properties.subscription_identifier.is_empty()
                    {
                        return Err(DecodeError::ProtocolError("属性重复出现"));
                    }
                    properties
                        .subscription_identifier
                        .try_reserve(1)
                        .map_err(|_| DecodeError::AllocationFailed)?;
                    properties.subscription_identifier.push(value);
                }
                PropertyId::SessionExpiryInterval => {
                    set_once(&mut properties.session_expiry_interval, sub.u32()?)?;
                }
                PropertyId::AssignedClientIdentifier => {
                    let value = sub.utf8()?;
                    if value.is_empty() {
                        return Err(DecodeError::ProtocolError(
                            "Assigned Client Identifier 不能为空",
                        ));
                    }
                    set_once(&mut properties.assigned_client_identifier, value)?;
                }
                PropertyId::ServerKeepAlive => {
                    set_once(&mut properties.server_keep_alive, sub.u16()?)?;
                }
                PropertyId::AuthenticationMethod => {
                    set_once(&mut properties.authentication_method, sub.utf8()?)?;
                }
                PropertyId::AuthenticationData => {
                    set_once(&mut properties.authentication_data, sub.binary()?)?;
                }
                PropertyId::RequestProblemInformation => {
                    set_once(
                        &mut properties.request_problem_information,
                        decode_bool(&mut sub)?,
                    )?;
                }
                PropertyId::WillDelayInterval => {
                    set_once(&mut properties.will_delay_interval, sub.u32()?)?;
                }
                PropertyId::RequestResponseInformation => {
                    set_once(
                        &mut properties.request_response_information,
                        decode_bool(&mut sub)?,
                    )?;
                }
                PropertyId::ResponseInformation => {
                    set_once(&mut properties.response_information, sub.utf8()?)?;
                }
                PropertyId::ServerReference => {
                    set_once(&mut properties.server_reference, sub.utf8()?)?;
                }
                PropertyId::ReasonString => {
                    set_once(&mut properties.reason_string, sub.utf8()?)?;
                }
                PropertyId::ReceiveMaximum => {
                    set_once(
                        &mut properties.receive_maximum,
                        decode_nonzero_u16(&mut sub, "Receive Maximum 不能为 0")?,
                    )?;
                }
                PropertyId::TopicAliasMaximum => {
                    set_once(&mut properties.topic_alias_maximum, sub.u16()?)?;
                }
                PropertyId::TopicAlias => {
                    set_once(
                        &mut properties.topic_alias,
                        decode_nonzero_u16(&mut sub, "Topic Alias 不能为 0")?,
                    )?;
                }
                PropertyId::MaximumQos => {
                    let value = sub.u8()?;
                    if value > 1 {
                        return Err(DecodeError::ProtocolError("Maximum QoS 只能为 0 或 1"));
                    }
                    set_once(&mut properties.maximum_qos, value)?;
                }
                PropertyId::RetainAvailable => {
                    set_once(&mut properties.retain_available, decode_bool(&mut sub)?)?;
                }
                PropertyId::UserProperty => {
                    let key = sub.utf8()?;
                    let value = sub.utf8()?;
                    properties
                        .user_property
                        .try_reserve(1)
                        .map_err(|_| DecodeError::AllocationFailed)?;
                    properties.user_property.push((key, value));
                }
                PropertyId::MaximumPacketSize => {
                    set_once(
                        &mut properties.maximum_packet_size,
                        decode_nonzero_u32(&mut sub, "Maximum Packet Size 不能为 0")?,
                    )?;
                }
                PropertyId::WildcardSubscriptionAvailable => {
                    set_once(
                        &mut properties.wildcard_subscription_available,
                        decode_bool(&mut sub)?,
                    )?;
                }
                PropertyId::SubscriptionIdentifierAvailable => {
                    set_once(
                        &mut properties.subscription_identifier_available,
                        decode_bool(&mut sub)?,
                    )?;
                }
                PropertyId::SharedSubscriptionAvailable => {
                    set_once(
                        &mut properties.shared_subscription_available,
                        decode_bool(&mut sub)?,
                    )?;
                }
            }
        }
        // CONNECT/CONNACK 中认证数据必须与认证方式一起出现；AUTH 不强制（规范歧义 MQTT-537）。
        if matches!(context, PropertyContext::Connect | PropertyContext::Connack)
            && properties.authentication_data.is_some()
            && properties.authentication_method.is_none()
        {
            return Err(DecodeError::ProtocolError(
                "Authentication Data 必须与 Authentication Method 一起出现",
            ));
        }
        Ok(properties)
    }

    /// 属性段内容（不含 property length 前缀）的编码结果。
    fn encode_content<S: Sink + ?Sized>(
        &self,
        sink: &mut S,
        context: PropertyContext,
    ) -> Result<(), EncodeError> {
        // CONNECT/CONNACK 中认证数据必须与认证方式一起出现。
        if matches!(context, PropertyContext::Connect | PropertyContext::Connack)
            && self.authentication_data.is_some()
            && self.authentication_method.is_none()
        {
            return Err(EncodeError::Invalid(
                "Authentication Data 必须与 Authentication Method 一起出现",
            ));
        }

        if let Some(value) = self.payload_format_indicator {
            require_allowed(PropertyId::PayloadFormatIndicator, context)?;
            write_id(sink, PropertyId::PayloadFormatIndicator);
            write_u8(sink, u8::from(value));
        }
        if let Some(value) = self.message_expiry_interval {
            require_allowed(PropertyId::MessageExpiryInterval, context)?;
            write_id(sink, PropertyId::MessageExpiryInterval);
            write_u32(sink, value);
        }
        if let Some(value) = &self.content_type {
            require_allowed(PropertyId::ContentType, context)?;
            write_id(sink, PropertyId::ContentType);
            codec::write_utf8(sink, value)?;
        }
        if let Some(value) = &self.response_topic {
            require_allowed(PropertyId::ResponseTopic, context)?;
            write_id(sink, PropertyId::ResponseTopic);
            value.encode_into(sink)?;
        }
        if let Some(value) = &self.correlation_data {
            require_allowed(PropertyId::CorrelationData, context)?;
            write_id(sink, PropertyId::CorrelationData);
            codec::write_binary(sink, value)?;
        }
        if !self.subscription_identifier.is_empty() {
            require_allowed(PropertyId::SubscriptionIdentifier, context)?;
            if !matches!(context, PropertyContext::Publish)
                && self.subscription_identifier.len() > 1
            {
                return Err(EncodeError::Invalid(
                    "Subscription Identifier 在订阅报文中至多出现一次",
                ));
            }
            for &value in &self.subscription_identifier {
                if value == 0 {
                    return Err(EncodeError::Invalid("Subscription Identifier 不能为 0"));
                }
                write_id(sink, PropertyId::SubscriptionIdentifier);
                write_varint(sink, value);
            }
        }
        if let Some(value) = self.session_expiry_interval {
            require_allowed(PropertyId::SessionExpiryInterval, context)?;
            write_id(sink, PropertyId::SessionExpiryInterval);
            write_u32(sink, value);
        }
        if let Some(value) = &self.assigned_client_identifier {
            require_allowed(PropertyId::AssignedClientIdentifier, context)?;
            if value.is_empty() {
                return Err(EncodeError::Invalid("Assigned Client Identifier 不能为空"));
            }
            write_id(sink, PropertyId::AssignedClientIdentifier);
            codec::write_utf8(sink, value)?;
        }
        if let Some(value) = self.server_keep_alive {
            require_allowed(PropertyId::ServerKeepAlive, context)?;
            write_id(sink, PropertyId::ServerKeepAlive);
            write_u16(sink, value);
        }
        if let Some(value) = &self.authentication_method {
            require_allowed(PropertyId::AuthenticationMethod, context)?;
            write_id(sink, PropertyId::AuthenticationMethod);
            codec::write_utf8(sink, value)?;
        }
        if let Some(value) = &self.authentication_data {
            require_allowed(PropertyId::AuthenticationData, context)?;
            write_id(sink, PropertyId::AuthenticationData);
            codec::write_binary(sink, value)?;
        }
        if let Some(value) = self.request_problem_information {
            require_allowed(PropertyId::RequestProblemInformation, context)?;
            write_id(sink, PropertyId::RequestProblemInformation);
            write_u8(sink, u8::from(value));
        }
        if let Some(value) = self.will_delay_interval {
            require_allowed(PropertyId::WillDelayInterval, context)?;
            write_id(sink, PropertyId::WillDelayInterval);
            write_u32(sink, value);
        }
        if let Some(value) = self.request_response_information {
            require_allowed(PropertyId::RequestResponseInformation, context)?;
            write_id(sink, PropertyId::RequestResponseInformation);
            write_u8(sink, u8::from(value));
        }
        if let Some(value) = &self.response_information {
            require_allowed(PropertyId::ResponseInformation, context)?;
            write_id(sink, PropertyId::ResponseInformation);
            codec::write_utf8(sink, value)?;
        }
        if let Some(value) = &self.server_reference {
            require_allowed(PropertyId::ServerReference, context)?;
            write_id(sink, PropertyId::ServerReference);
            codec::write_utf8(sink, value)?;
        }
        if let Some(value) = &self.reason_string {
            require_allowed(PropertyId::ReasonString, context)?;
            write_id(sink, PropertyId::ReasonString);
            codec::write_utf8(sink, value)?;
        }
        if let Some(value) = self.receive_maximum {
            require_allowed(PropertyId::ReceiveMaximum, context)?;
            if value == 0 {
                return Err(EncodeError::Invalid("Receive Maximum 不能为 0"));
            }
            write_id(sink, PropertyId::ReceiveMaximum);
            write_u16(sink, value);
        }
        if let Some(value) = self.topic_alias_maximum {
            require_allowed(PropertyId::TopicAliasMaximum, context)?;
            write_id(sink, PropertyId::TopicAliasMaximum);
            write_u16(sink, value);
        }
        if let Some(value) = self.topic_alias {
            require_allowed(PropertyId::TopicAlias, context)?;
            if value == 0 {
                return Err(EncodeError::Invalid("Topic Alias 不能为 0"));
            }
            write_id(sink, PropertyId::TopicAlias);
            write_u16(sink, value);
        }
        if let Some(value) = self.maximum_qos {
            require_allowed(PropertyId::MaximumQos, context)?;
            if value > 1 {
                return Err(EncodeError::Invalid("Maximum QoS 只能为 0 或 1"));
            }
            write_id(sink, PropertyId::MaximumQos);
            write_u8(sink, value);
        }
        if let Some(value) = self.retain_available {
            require_allowed(PropertyId::RetainAvailable, context)?;
            write_id(sink, PropertyId::RetainAvailable);
            write_u8(sink, u8::from(value));
        }
        if !self.user_property.is_empty() {
            require_allowed(PropertyId::UserProperty, context)?;
            for (key, value) in &self.user_property {
                write_id(sink, PropertyId::UserProperty);
                codec::write_utf8(sink, key)?;
                codec::write_utf8(sink, value)?;
            }
        }
        if let Some(value) = self.maximum_packet_size {
            require_allowed(PropertyId::MaximumPacketSize, context)?;
            if value == 0 {
                return Err(EncodeError::Invalid("Maximum Packet Size 不能为 0"));
            }
            write_id(sink, PropertyId::MaximumPacketSize);
            write_u32(sink, value);
        }
        if let Some(value) = self.wildcard_subscription_available {
            require_allowed(PropertyId::WildcardSubscriptionAvailable, context)?;
            write_id(sink, PropertyId::WildcardSubscriptionAvailable);
            write_u8(sink, u8::from(value));
        }
        if let Some(value) = self.subscription_identifier_available {
            require_allowed(PropertyId::SubscriptionIdentifierAvailable, context)?;
            write_id(sink, PropertyId::SubscriptionIdentifierAvailable);
            write_u8(sink, u8::from(value));
        }
        if let Some(value) = self.shared_subscription_available {
            require_allowed(PropertyId::SharedSubscriptionAvailable, context)?;
            write_id(sink, PropertyId::SharedSubscriptionAvailable);
            write_u8(sink, u8::from(value));
        }
        Ok(())
    }

    /// 属性段内容（不含 property length 前缀）编码后的字节数。
    fn encoded_content_len(&self, context: PropertyContext) -> Result<usize, EncodeError> {
        let mut sink = CountSink::new();
        self.encode_content(&mut sink, context)?;
        Ok(sink.len())
    }

    /// 写属性段（先写 property length VarInt 再写内容）。
    pub(crate) fn encode_into<S: Sink + ?Sized>(
        &self,
        sink: &mut S,
        context: PropertyContext,
    ) -> Result<(), EncodeError> {
        let content_len = self.encoded_content_len(context)?;
        if content_len > varint::MAX as usize {
            return Err(EncodeError::ValueOutOfRange {
                field: "属性段长度",
                value: content_len as u32,
                max: varint::MAX,
            });
        }
        write_varint(sink, content_len as u32);
        self.encode_content(sink, context)
    }

    /// 属性段编码后总长度（含 property length 前缀）。
    ///
    /// 报文层统一用计数 sink 计算报文体长度，生产路径不直接调用该方法；
    /// 保留它作为属性段大小的查询接口（测试也用它做一致性断言）。
    #[allow(dead_code)]
    pub(crate) fn encoded_len(&self, context: PropertyContext) -> Result<usize, EncodeError> {
        let content_len = self.encoded_content_len(context)?;
        if content_len > varint::MAX as usize {
            return Err(EncodeError::ValueOutOfRange {
                field: "属性段长度",
                value: content_len as u32,
                max: varint::MAX,
            });
        }
        Ok(varint::encoded_len(content_len as u32) + content_len)
    }
}

#[cfg(test)]
mod tests {
    use super::{Properties, PropertyContext, PropertyId, is_allowed};
    use crate::mqtt::codec::{Reader, VecSink};
    use crate::mqtt::error::{DecodeError, EncodeError};
    use crate::mqtt::topic::TopicName;
    use crate::mqtt::varint;
    use alloc::string::String;
    use alloc::vec;
    use alloc::vec::Vec;

    const PROPERTY_IDS: [(PropertyId, u32); 27] = [
        (PropertyId::PayloadFormatIndicator, 0x01),
        (PropertyId::MessageExpiryInterval, 0x02),
        (PropertyId::ContentType, 0x03),
        (PropertyId::ResponseTopic, 0x08),
        (PropertyId::CorrelationData, 0x09),
        (PropertyId::SubscriptionIdentifier, 0x0B),
        (PropertyId::SessionExpiryInterval, 0x11),
        (PropertyId::AssignedClientIdentifier, 0x12),
        (PropertyId::ServerKeepAlive, 0x13),
        (PropertyId::AuthenticationMethod, 0x15),
        (PropertyId::AuthenticationData, 0x16),
        (PropertyId::RequestProblemInformation, 0x17),
        (PropertyId::WillDelayInterval, 0x18),
        (PropertyId::RequestResponseInformation, 0x19),
        (PropertyId::ResponseInformation, 0x1A),
        (PropertyId::ServerReference, 0x1C),
        (PropertyId::ReasonString, 0x1F),
        (PropertyId::ReceiveMaximum, 0x21),
        (PropertyId::TopicAliasMaximum, 0x22),
        (PropertyId::TopicAlias, 0x23),
        (PropertyId::MaximumQos, 0x24),
        (PropertyId::RetainAvailable, 0x25),
        (PropertyId::UserProperty, 0x26),
        (PropertyId::MaximumPacketSize, 0x27),
        (PropertyId::WildcardSubscriptionAvailable, 0x28),
        (PropertyId::SubscriptionIdentifierAvailable, 0x29),
        (PropertyId::SharedSubscriptionAvailable, 0x2A),
    ];

    fn all_ids() -> [PropertyId; 27] {
        PROPERTY_IDS.map(|(id, _)| id)
    }

    fn all_contexts() -> [PropertyContext; 14] {
        [
            PropertyContext::Connect,
            PropertyContext::Will,
            PropertyContext::Connack,
            PropertyContext::Publish,
            PropertyContext::Puback,
            PropertyContext::Pubrec,
            PropertyContext::Pubrel,
            PropertyContext::Pubcomp,
            PropertyContext::Subscribe,
            PropertyContext::Suback,
            PropertyContext::Unsubscribe,
            PropertyContext::Unsuback,
            PropertyContext::Disconnect,
            PropertyContext::Auth,
        ]
    }

    /// 按规范独立写出各场景允许集，用于表驱动校验。
    fn expected_allowed(context: PropertyContext, id: PropertyId) -> bool {
        use PropertyId::*;
        let allowed: &[PropertyId] = match context {
            PropertyContext::Connect => &[
                SessionExpiryInterval,
                ReceiveMaximum,
                MaximumPacketSize,
                TopicAliasMaximum,
                RequestResponseInformation,
                RequestProblemInformation,
                UserProperty,
                AuthenticationMethod,
                AuthenticationData,
            ],
            PropertyContext::Will => &[
                WillDelayInterval,
                PayloadFormatIndicator,
                MessageExpiryInterval,
                ContentType,
                ResponseTopic,
                CorrelationData,
                UserProperty,
            ],
            PropertyContext::Connack => &[
                SessionExpiryInterval,
                ReceiveMaximum,
                MaximumQos,
                RetainAvailable,
                MaximumPacketSize,
                AssignedClientIdentifier,
                TopicAliasMaximum,
                ReasonString,
                UserProperty,
                WildcardSubscriptionAvailable,
                SubscriptionIdentifierAvailable,
                SharedSubscriptionAvailable,
                ServerKeepAlive,
                ResponseInformation,
                ServerReference,
                AuthenticationMethod,
                AuthenticationData,
            ],
            PropertyContext::Publish => &[
                PayloadFormatIndicator,
                MessageExpiryInterval,
                TopicAlias,
                ResponseTopic,
                CorrelationData,
                UserProperty,
                SubscriptionIdentifier,
                ContentType,
            ],
            PropertyContext::Puback
            | PropertyContext::Pubrec
            | PropertyContext::Pubrel
            | PropertyContext::Pubcomp => &[ReasonString, UserProperty],
            PropertyContext::Subscribe => &[SubscriptionIdentifier, UserProperty],
            PropertyContext::Suback => &[ReasonString, UserProperty],
            PropertyContext::Unsubscribe => &[UserProperty],
            PropertyContext::Unsuback => &[ReasonString, UserProperty],
            PropertyContext::Disconnect => &[
                SessionExpiryInterval,
                ReasonString,
                UserProperty,
                ServerReference,
            ],
            PropertyContext::Auth => &[
                AuthenticationMethod,
                AuthenticationData,
                ReasonString,
                UserProperty,
            ],
        };
        allowed.contains(&id)
    }

    fn encode(properties: &Properties, context: PropertyContext) -> Result<Vec<u8>, EncodeError> {
        let mut out = Vec::new();
        {
            let mut sink = VecSink::new(&mut out);
            properties.encode_into(&mut sink, context)?;
        }
        assert_eq!(properties.encoded_len(context)?, out.len());
        Ok(out)
    }

    fn encode_ok(properties: &Properties, context: PropertyContext) -> Vec<u8> {
        encode(properties, context).unwrap()
    }

    fn decode(bytes: &[u8], context: PropertyContext) -> Result<Properties, DecodeError> {
        let mut reader = Reader::new(bytes);
        let properties = Properties::decode(&mut reader, context)?;
        reader.finish().unwrap();
        Ok(properties)
    }

    /// 构造属性段字节（含 property length 前缀）。
    fn section(content: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        varint::encode(content.len() as u32, &mut out).unwrap();
        out.extend_from_slice(content);
        out
    }

    /// 单条属性字节（属性 ID + 值）。
    fn prop(id: PropertyId, value: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        varint::encode(id.as_u32(), &mut out).unwrap();
        out.extend_from_slice(value);
        out
    }

    /// 各属性类型的一个合法值编码（不含属性 ID）。
    fn valid_value(id: PropertyId) -> Vec<u8> {
        use PropertyId::*;
        let value: &[u8] = match id {
            PayloadFormatIndicator
            | RequestProblemInformation
            | RequestResponseInformation
            | RetainAvailable
            | WildcardSubscriptionAvailable
            | SubscriptionIdentifierAvailable
            | SharedSubscriptionAvailable
            | MaximumQos => &[0x01],
            ServerKeepAlive | ReceiveMaximum | TopicAliasMaximum | TopicAlias => &[0x00, 0x01],
            MessageExpiryInterval
            | SessionExpiryInterval
            | WillDelayInterval
            | MaximumPacketSize => &[0x00, 0x00, 0x00, 0x01],
            SubscriptionIdentifier => &[0x01],
            ContentType
            | AssignedClientIdentifier
            | AuthenticationMethod
            | ResponseInformation
            | ServerReference
            | ReasonString
            | ResponseTopic => &[0x00, 0x01, b'x'],
            CorrelationData | AuthenticationData => &[0x00, 0x01, 0x01],
            UserProperty => &[0x00, 0x01, b'k', 0x00, 0x01, b'v'],
        };
        value.to_vec()
    }

    #[test]
    fn property_id_roundtrip() {
        for (id, value) in PROPERTY_IDS {
            assert_eq!(id.as_u32(), value);
            assert_eq!(PropertyId::try_from_u32(value), Some(id));
        }
        for value in [
            0u32,
            0x04,
            0x05,
            0x06,
            0x07,
            0x0A,
            0x0C,
            0x10,
            0x14,
            0x1B,
            0x1D,
            0x1E,
            0x20,
            0x2B,
            u32::MAX,
        ] {
            assert_eq!(PropertyId::try_from_u32(value), None, "value = {value}");
        }
    }

    #[test]
    fn allowed_sets_match_spec() {
        for context in all_contexts() {
            for id in all_ids() {
                assert_eq!(
                    is_allowed(id, context),
                    expected_allowed(context, id),
                    "{id:?} 在 {context:?}"
                );
            }
        }
    }

    #[test]
    fn all_27_ids_roundtrip() {
        let cases: [(PropertyContext, Properties); 27] = [
            (
                PropertyContext::Publish,
                Properties {
                    payload_format_indicator: Some(true),
                    ..Properties::new()
                },
            ),
            (
                PropertyContext::Publish,
                Properties {
                    message_expiry_interval: Some(60),
                    ..Properties::new()
                },
            ),
            (
                PropertyContext::Publish,
                Properties {
                    content_type: Some(String::from("text/plain")),
                    ..Properties::new()
                },
            ),
            (
                PropertyContext::Publish,
                Properties {
                    response_topic: Some(TopicName::new("reply/topic").unwrap()),
                    ..Properties::new()
                },
            ),
            (
                PropertyContext::Publish,
                Properties {
                    correlation_data: Some(vec![1, 2, 3]),
                    ..Properties::new()
                },
            ),
            (
                PropertyContext::Publish,
                Properties {
                    subscription_identifier: vec![1, 268_435_455],
                    ..Properties::new()
                },
            ),
            (
                PropertyContext::Connect,
                Properties {
                    session_expiry_interval: Some(300),
                    ..Properties::new()
                },
            ),
            (
                PropertyContext::Connack,
                Properties {
                    assigned_client_identifier: Some(String::from("client-1")),
                    ..Properties::new()
                },
            ),
            (
                PropertyContext::Connack,
                Properties {
                    server_keep_alive: Some(30),
                    ..Properties::new()
                },
            ),
            (
                PropertyContext::Connect,
                Properties {
                    authentication_method: Some(String::from("SCRAM-SHA-256")),
                    ..Properties::new()
                },
            ),
            (
                PropertyContext::Connect,
                Properties {
                    authentication_method: Some(String::from("SCRAM-SHA-256")),
                    authentication_data: Some(vec![0xAA, 0xBB]),
                    ..Properties::new()
                },
            ),
            (
                PropertyContext::Connect,
                Properties {
                    request_problem_information: Some(false),
                    ..Properties::new()
                },
            ),
            (
                PropertyContext::Will,
                Properties {
                    will_delay_interval: Some(5),
                    ..Properties::new()
                },
            ),
            (
                PropertyContext::Connect,
                Properties {
                    request_response_information: Some(true),
                    ..Properties::new()
                },
            ),
            (
                PropertyContext::Connack,
                Properties {
                    response_information: Some(String::from("info")),
                    ..Properties::new()
                },
            ),
            (
                PropertyContext::Disconnect,
                Properties {
                    server_reference: Some(String::from("server2")),
                    ..Properties::new()
                },
            ),
            (
                PropertyContext::Puback,
                Properties {
                    reason_string: Some(String::from("原因")),
                    ..Properties::new()
                },
            ),
            (
                PropertyContext::Connect,
                Properties {
                    receive_maximum: Some(10),
                    ..Properties::new()
                },
            ),
            (
                PropertyContext::Connect,
                Properties {
                    topic_alias_maximum: Some(5),
                    ..Properties::new()
                },
            ),
            (
                PropertyContext::Publish,
                Properties {
                    topic_alias: Some(7),
                    ..Properties::new()
                },
            ),
            (
                PropertyContext::Connack,
                Properties {
                    maximum_qos: Some(1),
                    ..Properties::new()
                },
            ),
            (
                PropertyContext::Connack,
                Properties {
                    retain_available: Some(false),
                    ..Properties::new()
                },
            ),
            (
                PropertyContext::Puback,
                Properties {
                    user_property: vec![
                        (String::from("k"), String::from("v")),
                        (String::from("k2"), String::from("v2")),
                    ],
                    ..Properties::new()
                },
            ),
            (
                PropertyContext::Connect,
                Properties {
                    maximum_packet_size: Some(u32::MAX),
                    ..Properties::new()
                },
            ),
            (
                PropertyContext::Connack,
                Properties {
                    wildcard_subscription_available: Some(true),
                    ..Properties::new()
                },
            ),
            (
                PropertyContext::Connack,
                Properties {
                    subscription_identifier_available: Some(false),
                    ..Properties::new()
                },
            ),
            (
                PropertyContext::Connack,
                Properties {
                    shared_subscription_available: Some(true),
                    ..Properties::new()
                },
            ),
        ];
        for (context, properties) in cases {
            let bytes = encode_ok(&properties, context);
            let decoded = decode(&bytes, context).unwrap();
            assert_eq!(decoded, properties, "context = {context:?}");
        }
    }

    #[test]
    fn combined_properties_roundtrip() {
        let properties = Properties {
            payload_format_indicator: Some(true),
            message_expiry_interval: Some(30),
            content_type: Some(String::from("application/json")),
            response_topic: Some(TopicName::new("reply/1").unwrap()),
            correlation_data: Some(vec![7, 8]),
            subscription_identifier: vec![3, 9],
            topic_alias: Some(2),
            user_property: vec![(String::from("trace"), String::from("abc"))],
            ..Properties::new()
        };
        let bytes = encode_ok(&properties, PropertyContext::Publish);
        assert_eq!(
            decode(&bytes, PropertyContext::Publish).unwrap(),
            properties
        );
    }

    #[test]
    fn empty_properties_roundtrip() {
        let properties = Properties::new();
        assert!(properties.is_empty());
        assert_eq!(properties.encoded_len(PropertyContext::Publish).unwrap(), 1);
        let bytes = encode_ok(&properties, PropertyContext::Publish);
        assert_eq!(bytes, vec![0x00]);
        assert_eq!(
            decode(&bytes, PropertyContext::Publish).unwrap(),
            properties
        );
        assert!(
            !Properties {
                session_expiry_interval: Some(1),
                ..Properties::new()
            }
            .is_empty()
        );
    }

    #[test]
    fn boolean_property_encoding() {
        for value in [false, true] {
            let properties = Properties {
                payload_format_indicator: Some(value),
                ..Properties::new()
            };
            let bytes = encode_ok(&properties, PropertyContext::Publish);
            assert_eq!(bytes, vec![0x02, 0x01, u8::from(value)]);
            assert_eq!(
                decode(&bytes, PropertyContext::Publish).unwrap(),
                properties
            );
        }
    }

    #[test]
    fn decode_rejects_forbidden_properties() {
        for context in all_contexts() {
            for id in all_ids() {
                if is_allowed(id, context) {
                    continue;
                }
                let mut content = Vec::new();
                varint::encode(id.as_u32(), &mut content).unwrap();
                content.extend_from_slice(&valid_value(id));
                let bytes = section(&content);
                assert_eq!(
                    decode(&bytes, context),
                    Err(DecodeError::ProtocolError("属性不允许出现在该报文中")),
                    "{id:?} 在 {context:?}"
                );
            }
        }
    }

    #[test]
    fn decode_rejects_duplicate_properties() {
        // 非 User Property 重复出现。
        let mut content = Vec::new();
        content.extend(prop(PropertyId::SessionExpiryInterval, &[0, 0, 0, 1]));
        content.extend(prop(PropertyId::SessionExpiryInterval, &[0, 0, 0, 2]));
        assert_eq!(
            decode(&section(&content), PropertyContext::Connect),
            Err(DecodeError::ProtocolError("属性重复出现"))
        );

        // Subscription Identifier 在 PUBLISH 中可重复。
        let mut content = Vec::new();
        content.extend(prop(PropertyId::SubscriptionIdentifier, &[0x01]));
        content.extend(prop(PropertyId::SubscriptionIdentifier, &[0x02]));
        let properties = decode(&section(&content), PropertyContext::Publish).unwrap();
        assert_eq!(properties.subscription_identifier, vec![1, 2]);

        // Subscription Identifier 在 SUBSCRIBE 中至多一次。
        assert_eq!(
            decode(&section(&content), PropertyContext::Subscribe),
            Err(DecodeError::ProtocolError("属性重复出现"))
        );

        // User Property 可重复。
        let mut content = Vec::new();
        content.extend(prop(
            PropertyId::UserProperty,
            &[0x00, 0x01, b'k', 0x00, 0x01, b'v'],
        ));
        content.extend(prop(
            PropertyId::UserProperty,
            &[0x00, 0x01, b'k', 0x00, 0x01, b'v'],
        ));
        let properties = decode(&section(&content), PropertyContext::Disconnect).unwrap();
        assert_eq!(properties.user_property.len(), 2);
    }

    #[test]
    fn decode_rejects_invalid_values() {
        let cases: [(PropertyContext, Vec<u8>, &str); 8] = [
            (
                PropertyContext::Connect,
                prop(PropertyId::ReceiveMaximum, &[0x00, 0x00]),
                "Receive Maximum 不能为 0",
            ),
            (
                PropertyContext::Connect,
                prop(PropertyId::MaximumPacketSize, &[0x00, 0x00, 0x00, 0x00]),
                "Maximum Packet Size 不能为 0",
            ),
            (
                PropertyContext::Publish,
                prop(PropertyId::TopicAlias, &[0x00, 0x00]),
                "Topic Alias 不能为 0",
            ),
            (
                PropertyContext::Publish,
                prop(PropertyId::SubscriptionIdentifier, &[0x00]),
                "Subscription Identifier 不能为 0",
            ),
            (
                PropertyContext::Connack,
                prop(PropertyId::MaximumQos, &[0x02]),
                "Maximum QoS 只能为 0 或 1",
            ),
            (
                PropertyContext::Publish,
                prop(PropertyId::PayloadFormatIndicator, &[0x02]),
                "布尔属性取值必须为 0 或 1",
            ),
            (
                PropertyContext::Connack,
                prop(PropertyId::AssignedClientIdentifier, &[0x00, 0x00]),
                "Assigned Client Identifier 不能为空",
            ),
            (
                PropertyContext::Publish,
                prop(PropertyId::ResponseTopic, &[0x00, 0x03, b'a', b'/', b'#']),
                "主题名不能包含通配符 '#'",
            ),
        ];
        for (context, content, message) in cases {
            assert_eq!(
                decode(&section(&content), context),
                Err(DecodeError::ProtocolError(message)),
                "context = {context:?}"
            );
        }
        // Response Topic 为空主题名。
        let content = prop(PropertyId::ResponseTopic, &[0x00, 0x00]);
        assert_eq!(
            decode(&section(&content), PropertyContext::Publish),
            Err(DecodeError::ProtocolError("主题名不能为空"))
        );
    }

    #[test]
    fn authentication_data_requires_method() {
        // CONNACK 中只有 Authentication Data 非法。
        let content = prop(PropertyId::AuthenticationData, &[0x00, 0x01, 0xAA]);
        assert_eq!(
            decode(&section(&content), PropertyContext::Connack),
            Err(DecodeError::ProtocolError(
                "Authentication Data 必须与 Authentication Method 一起出现"
            ))
        );
        // AUTH 中不强制。
        let properties = decode(&section(&content), PropertyContext::Auth).unwrap();
        assert_eq!(properties.authentication_data, Some(vec![0xAA]));
        // CONNECT 中同时出现合法。
        let mut content = prop(PropertyId::AuthenticationMethod, &[0x00, 0x01, b'm']);
        content.extend(prop(PropertyId::AuthenticationData, &[0x00, 0x01, 0xAA]));
        let properties = decode(&section(&content), PropertyContext::Connect).unwrap();
        assert_eq!(properties.authentication_method.as_deref(), Some("m"));
        assert_eq!(properties.authentication_data, Some(vec![0xAA]));
    }

    #[test]
    fn decode_rejects_unknown_property_id() {
        assert_eq!(
            decode(&section(&[0x05]), PropertyContext::Connect),
            Err(DecodeError::Malformed("未知属性 ID"))
        );
        assert_eq!(
            decode(&section(&[0x00]), PropertyContext::Connect),
            Err(DecodeError::Malformed("未知属性 ID"))
        );
    }

    #[test]
    fn decode_rejects_bad_lengths() {
        // property length 超出剩余字节。
        assert_eq!(
            decode(&[0x05], PropertyContext::Connect),
            Err(DecodeError::Malformed("字段长度超出报文剩余长度"))
        );
        // property length VarInt 本身不完整。
        assert_eq!(
            decode(&[], PropertyContext::Connect),
            Err(DecodeError::Malformed("Variable Byte Integer 未终止"))
        );
        // 属性段内的值不足。
        assert_eq!(
            decode(&section(&[0x11, 0x00]), PropertyContext::Connect),
            Err(DecodeError::Malformed("字段长度超出报文剩余长度"))
        );
        // 属性段内有多余字节。
        assert_eq!(
            decode(&section(&[0x17, 0x01, 0x00]), PropertyContext::Connect),
            Err(DecodeError::Malformed("未知属性 ID"))
        );
    }

    #[test]
    fn encode_rejects_invalid() {
        let connect_forbidden = Properties {
            payload_format_indicator: Some(true),
            ..Properties::new()
        };
        assert_eq!(
            encode(&connect_forbidden, PropertyContext::Connect),
            Err(EncodeError::Invalid("属性不允许出现在该报文中"))
        );

        let zero_receive_maximum = Properties {
            receive_maximum: Some(0),
            ..Properties::new()
        };
        assert_eq!(
            encode(&zero_receive_maximum, PropertyContext::Connect),
            Err(EncodeError::Invalid("Receive Maximum 不能为 0"))
        );

        let zero_maximum_packet_size = Properties {
            maximum_packet_size: Some(0),
            ..Properties::new()
        };
        assert_eq!(
            encode(&zero_maximum_packet_size, PropertyContext::Connect),
            Err(EncodeError::Invalid("Maximum Packet Size 不能为 0"))
        );

        let zero_topic_alias = Properties {
            topic_alias: Some(0),
            ..Properties::new()
        };
        assert_eq!(
            encode(&zero_topic_alias, PropertyContext::Publish),
            Err(EncodeError::Invalid("Topic Alias 不能为 0"))
        );

        let invalid_maximum_qos = Properties {
            maximum_qos: Some(2),
            ..Properties::new()
        };
        assert_eq!(
            encode(&invalid_maximum_qos, PropertyContext::Connack),
            Err(EncodeError::Invalid("Maximum QoS 只能为 0 或 1"))
        );

        let empty_identifier = Properties {
            assigned_client_identifier: Some(String::new()),
            ..Properties::new()
        };
        assert_eq!(
            encode(&empty_identifier, PropertyContext::Connack),
            Err(EncodeError::Invalid("Assigned Client Identifier 不能为空"))
        );

        let zero_subscription_identifier = Properties {
            subscription_identifier: vec![0],
            ..Properties::new()
        };
        assert_eq!(
            encode(&zero_subscription_identifier, PropertyContext::Publish),
            Err(EncodeError::Invalid("Subscription Identifier 不能为 0"))
        );

        let two_subscription_identifiers = Properties {
            subscription_identifier: vec![1, 2],
            ..Properties::new()
        };
        assert_eq!(
            encode(&two_subscription_identifiers, PropertyContext::Subscribe),
            Err(EncodeError::Invalid(
                "Subscription Identifier 在订阅报文中至多出现一次"
            ))
        );

        let data_without_method = Properties {
            authentication_data: Some(vec![1]),
            ..Properties::new()
        };
        assert_eq!(
            encode(&data_without_method, PropertyContext::Connect),
            Err(EncodeError::Invalid(
                "Authentication Data 必须与 Authentication Method 一起出现"
            ))
        );
    }

    #[test]
    fn encoded_len_matches_encoding() {
        let properties = Properties {
            reason_string: Some(String::from("错误")),
            user_property: vec![(String::from("a"), String::from("b"))],
            ..Properties::new()
        };
        let bytes = encode_ok(&properties, PropertyContext::Puback);
        assert_eq!(
            properties.encoded_len(PropertyContext::Puback).unwrap(),
            bytes.len()
        );
        // 2 字节长度前缀 + 0x1F 属性 + 原因字符串 + 用户属性。
        assert!(bytes.len() > 2);
    }
}
