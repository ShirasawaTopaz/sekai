//! UNSUBACK 报文。

use alloc::vec::Vec;

use crate::mqtt::codec::{Reader, Sink, write_packet_id, write_u8};
use crate::mqtt::error::{DecodeError, EncodeError};
use crate::mqtt::header::PacketType;
use crate::mqtt::property::{Properties, PropertyContext};
use crate::mqtt::reason::ReasonCode;

use super::{decode_packet_id, decode_reason_code, encode_reason_code};

/// UNSUBACK 报文。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unsuback {
    pub packet_id: u16,
    pub properties: Properties,
    pub reason_codes: Vec<ReasonCode>,
}

impl Unsuback {
    pub(crate) fn decode_body(body: &[u8]) -> Result<Self, DecodeError> {
        let mut reader = Reader::new(body);
        let packet_id = decode_packet_id(&mut reader)?;
        let properties = Properties::decode(&mut reader, PropertyContext::Unsuback)?;

        let mut reason_codes = Vec::new();
        while !reader.is_empty() {
            let reason_code = decode_reason_code(reader.u8()?, PacketType::Unsuback)?;
            reason_codes
                .try_reserve(1)
                .map_err(|_| DecodeError::AllocationFailed)?;
            reason_codes.push(reason_code);
        }

        if reason_codes.is_empty() {
            return Err(DecodeError::ProtocolError("UNSUBACK 至少需要一个原因码"));
        }
        reader.finish()?;
        Ok(Self {
            packet_id,
            properties,
            reason_codes,
        })
    }

    pub(crate) fn encode_body<S: Sink + ?Sized>(&self, sink: &mut S) -> Result<(), EncodeError> {
        if self.reason_codes.is_empty() {
            return Err(EncodeError::Invalid("UNSUBACK 至少需要一个原因码"));
        }
        write_packet_id(sink, self.packet_id)?;
        self.properties
            .encode_into(sink, PropertyContext::Unsuback)?;
        for reason_code in &self.reason_codes {
            encode_reason_code(*reason_code, PacketType::Unsuback)?;
            write_u8(sink, reason_code.as_u8());
        }
        Ok(())
    }
}
