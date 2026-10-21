//! PUBLISH 报文。

use alloc::vec::Vec;

use crate::mqtt::codec::{Reader, Sink, decode_utf8, write_packet_id, write_u16};
use crate::mqtt::error::{DecodeError, EncodeError};
use crate::mqtt::property::{Properties, PropertyContext};
use crate::mqtt::qos::QoS;
use crate::mqtt::topic::TopicName;

use super::decode_packet_id;

/// PUBLISH 报文。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Publish {
    pub dup: bool,
    pub qos: QoS,
    pub retain: bool,
    /// `None` 表示零长度主题，此时必须配合非零 Topic Alias 属性。
    pub topic: Option<TopicName>,
    pub packet_id: Option<u16>,
    pub properties: Properties,
    pub payload: Vec<u8>,
}

impl Publish {
    /// 固定头低 4 位：`DUP(3) | QoS(2..1) | RETAIN(0)`。
    pub(crate) fn flags_byte(&self) -> u8 {
        (u8::from(self.dup) << 3) | (self.qos.as_u8() << 1) | u8::from(self.retain)
    }

    pub(crate) fn decode_body(flags: u8, body: &[u8]) -> Result<Self, DecodeError> {
        let dup = flags & 0x08 != 0;
        let qos = QoS::from_u8((flags >> 1) & 0x03)
            .ok_or(DecodeError::Malformed("PUBLISH QoS 不能为 3"))?;
        let retain = flags & 0x01 != 0;
        if qos == QoS::AtMostOnce && dup {
            return Err(DecodeError::Malformed("QoS 0 的 PUBLISH 不能设置 DUP"));
        }

        let mut reader = Reader::new(body);
        let raw_topic = reader.string_bytes()?;
        let topic = if raw_topic.is_empty() {
            None
        } else {
            let text = decode_utf8(raw_topic)?;
            Some(TopicName::new(text).map_err(DecodeError::ProtocolError)?)
        };

        let packet_id = if qos == QoS::AtMostOnce {
            None
        } else {
            Some(decode_packet_id(&mut reader)?)
        };

        let properties = Properties::decode(&mut reader, PropertyContext::Publish)?;
        if topic.is_none() && properties.topic_alias.is_none() {
            return Err(DecodeError::ProtocolError(
                "零长度主题的 PUBLISH 必须携带 Topic Alias",
            ));
        }

        let payload_len = reader.remaining();
        let raw_payload = reader.take(payload_len)?;
        let mut payload = Vec::new();
        payload
            .try_reserve_exact(payload_len)
            .map_err(|_| DecodeError::AllocationFailed)?;
        payload.extend_from_slice(raw_payload);

        reader.finish()?;
        Ok(Self {
            dup,
            qos,
            retain,
            topic,
            packet_id,
            properties,
            payload,
        })
    }

    pub(crate) fn encode_body<S: Sink + ?Sized>(&self, sink: &mut S) -> Result<(), EncodeError> {
        if self.qos == QoS::AtMostOnce {
            if self.dup {
                return Err(EncodeError::Invalid("QoS 0 的 PUBLISH 不能设置 DUP"));
            }
            if self.packet_id.is_some() {
                return Err(EncodeError::Invalid("QoS 0 的 PUBLISH 不能携带包 ID"));
            }
        } else if self.packet_id.is_none() {
            return Err(EncodeError::Invalid("QoS > 0 的 PUBLISH 必须携带包 ID"));
        }

        match &self.topic {
            Some(topic) => topic.encode_into(sink)?,
            None => {
                if self.properties.topic_alias.is_none() {
                    return Err(EncodeError::Invalid(
                        "零长度主题的 PUBLISH 必须携带 Topic Alias",
                    ));
                }
                write_u16(sink, 0);
            }
        }

        if let Some(packet_id) = self.packet_id {
            write_packet_id(sink, packet_id)?;
        }
        self.properties
            .encode_into(sink, PropertyContext::Publish)?;
        sink.write(&self.payload);
        Ok(())
    }
}
