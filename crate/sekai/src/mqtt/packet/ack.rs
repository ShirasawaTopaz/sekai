//! PUBACK / PUBREC / PUBREL / PUBCOMP 报文共用的确认结构。

use crate::mqtt::codec::{Reader, Sink, write_packet_id};
use crate::mqtt::error::{DecodeError, EncodeError};
use crate::mqtt::header::PacketType;
use crate::mqtt::property::Properties;
use crate::mqtt::reason::ReasonCode;

use super::{
    ack_property_context, decode_optional_reason_and_properties, decode_packet_id,
    encode_optional_reason_and_properties,
};

/// 确认类报文（PUBACK/PUBREC/PUBREL/PUBCOMP）共用的结构。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ack {
    pub packet_id: u16,
    pub reason_code: ReasonCode,
    pub properties: Properties,
}

impl Ack {
    pub(crate) fn decode_body(body: &[u8], packet_type: PacketType) -> Result<Self, DecodeError> {
        let mut reader = Reader::new(body);
        let packet_id = decode_packet_id(&mut reader)?;
        let (reason_code, properties) = decode_optional_reason_and_properties(
            &mut reader,
            packet_type,
            ReasonCode::Success,
            ack_property_context(packet_type),
        )?;
        reader.finish()?;
        Ok(Self {
            packet_id,
            reason_code,
            properties,
        })
    }

    pub(crate) fn encode_body<S: Sink + ?Sized>(
        &self,
        sink: &mut S,
        packet_type: PacketType,
    ) -> Result<(), EncodeError> {
        write_packet_id(sink, self.packet_id)?;
        encode_optional_reason_and_properties(
            sink,
            self.reason_code,
            &self.properties,
            packet_type,
            ack_property_context(packet_type),
        )
    }
}
