//! CONNECT 报文。

use alloc::string::String;
use alloc::vec::Vec;

use crate::mqtt::codec::{Reader, Sink, write_binary, write_u8, write_u16, write_utf8};
use crate::mqtt::error::{DecodeError, EncodeError};
use crate::mqtt::property::{Properties, PropertyContext};
use crate::mqtt::qos::QoS;
use crate::mqtt::topic::TopicName;

/// CONNECT 协议名。
const PROTOCOL_NAME: &str = "MQTT";
/// MQTT 5.0 协议版本号。
const PROTOCOL_VERSION: u8 = 5;

/// Will 消息（遗嘱）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Will {
    pub topic: TopicName,
    pub payload: Vec<u8>,
    pub qos: QoS,
    pub retain: bool,
    pub properties: Properties,
}

/// CONNECT 报文。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Connect {
    pub clean_start: bool,
    pub keep_alive: u16,
    pub client_id: String,
    pub will: Option<Will>,
    pub username: Option<String>,
    pub password: Option<Vec<u8>>,
    pub properties: Properties,
}

impl Connect {
    /// 解码 CONNECT 报文体。
    ///
    /// 说明：空 Client ID + Clean Start = 0 属于角色相关规则，codec 允许解码，
    /// 由客户端状态机在发送前拒绝。
    pub(crate) fn decode_body(body: &[u8]) -> Result<Self, DecodeError> {
        let mut reader = Reader::new(body);

        let protocol_name = reader.utf8()?;
        if protocol_name != PROTOCOL_NAME {
            return Err(DecodeError::Malformed("协议名必须为 MQTT"));
        }
        let protocol_version = reader.u8()?;
        if protocol_version != PROTOCOL_VERSION {
            return Err(DecodeError::UnsupportedProtocolVersion(protocol_version));
        }

        let flags = reader.u8()?;
        if flags & 0x01 != 0 {
            return Err(DecodeError::Malformed("CONNECT 标志保留位必须为 0"));
        }
        let clean_start = flags & 0x02 != 0;
        let will_flag = flags & 0x04 != 0;
        let will_qos_bits = (flags >> 3) & 0x03;
        let will_retain = flags & 0x20 != 0;
        let password_flag = flags & 0x40 != 0;
        let username_flag = flags & 0x80 != 0;
        if will_flag {
            if will_qos_bits == 3 {
                return Err(DecodeError::Malformed("Will QoS 不能为 3"));
            }
        } else if will_qos_bits != 0 || will_retain {
            return Err(DecodeError::Malformed(
                "未设置 Will 标志时 Will QoS/Retain 必须为 0",
            ));
        }

        let keep_alive = reader.u16()?;
        let properties = Properties::decode(&mut reader, PropertyContext::Connect)?;
        let client_id = reader.utf8()?;

        let will = if will_flag {
            let properties = Properties::decode(&mut reader, PropertyContext::Will)?;
            let topic = TopicName::decode(&mut reader)?;
            let payload = reader.binary()?;
            let qos = QoS::from_u8(will_qos_bits).ok_or(DecodeError::Malformed("Will QoS 非法"))?;
            Some(Will {
                topic,
                payload,
                qos,
                retain: will_retain,
                properties,
            })
        } else {
            None
        };

        let username = if username_flag {
            Some(reader.utf8()?)
        } else {
            None
        };
        let password = if password_flag {
            Some(reader.binary()?)
        } else {
            None
        };

        reader.finish()?;
        Ok(Self {
            clean_start,
            keep_alive,
            client_id,
            will,
            username,
            password,
            properties,
        })
    }

    /// 编码 CONNECT 报文体。
    pub(crate) fn encode_body<S: Sink + ?Sized>(&self, sink: &mut S) -> Result<(), EncodeError> {
        write_utf8(sink, PROTOCOL_NAME)?;
        write_u8(sink, PROTOCOL_VERSION);

        let mut flags = 0u8;
        if self.clean_start {
            flags |= 0x02;
        }
        if let Some(will) = &self.will {
            flags |= 0x04;
            flags |= (will.qos.as_u8() & 0x03) << 3;
            if will.retain {
                flags |= 0x20;
            }
        }
        if self.password.is_some() {
            flags |= 0x40;
        }
        if self.username.is_some() {
            flags |= 0x80;
        }
        write_u8(sink, flags);
        write_u16(sink, self.keep_alive);
        self.properties
            .encode_into(sink, PropertyContext::Connect)?;
        write_utf8(sink, &self.client_id)?;

        if let Some(will) = &self.will {
            will.properties.encode_into(sink, PropertyContext::Will)?;
            will.topic.encode_into(sink)?;
            write_binary(sink, &will.payload)?;
        }
        if let Some(username) = &self.username {
            write_utf8(sink, username)?;
        }
        if let Some(password) = &self.password {
            write_binary(sink, password)?;
        }
        Ok(())
    }
}
