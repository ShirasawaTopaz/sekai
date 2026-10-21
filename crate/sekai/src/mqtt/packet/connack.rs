//! CONNACK 报文。

use crate::mqtt::codec::{Reader, Sink, write_u8};
use crate::mqtt::error::{DecodeError, EncodeError};
use crate::mqtt::header::PacketType;
use crate::mqtt::property::{Properties, PropertyContext};
use crate::mqtt::reason::ReasonCode;

use super::{decode_reason_code, encode_reason_code};

/// CONNACK 报文。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Connack {
    pub session_present: bool,
    pub reason_code: ReasonCode,
    pub properties: Properties,
}

impl Connack {
    pub(crate) fn decode_body(body: &[u8]) -> Result<Self, DecodeError> {
        let mut reader = Reader::new(body);
        let ack_flags = reader.u8()?;
        if ack_flags & 0xFE != 0 {
            return Err(DecodeError::Malformed("CONNACK 应答标志保留位必须为 0"));
        }
        let session_present = ack_flags & 0x01 != 0;
        let reason_code = decode_reason_code(reader.u8()?, PacketType::Connack)?;
        if session_present && reason_code != ReasonCode::Success {
            return Err(DecodeError::ProtocolError(
                "CONNACK 非成功原因码不能设置 Session Present",
            ));
        }
        let properties = Properties::decode(&mut reader, PropertyContext::Connack)?;
        reader.finish()?;
        Ok(Self {
            session_present,
            reason_code,
            properties,
        })
    }

    pub(crate) fn encode_body<S: Sink + ?Sized>(&self, sink: &mut S) -> Result<(), EncodeError> {
        encode_reason_code(self.reason_code, PacketType::Connack)?;
        if self.session_present && self.reason_code != ReasonCode::Success {
            return Err(EncodeError::Invalid(
                "CONNACK 非成功原因码不能设置 Session Present",
            ));
        }
        write_u8(sink, if self.session_present { 0x01 } else { 0x00 });
        write_u8(sink, self.reason_code.as_u8());
        self.properties
            .encode_into(sink, PropertyContext::Connack)?;
        Ok(())
    }
}
