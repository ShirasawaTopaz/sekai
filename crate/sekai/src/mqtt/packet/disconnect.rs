//! DISCONNECT 报文。

use crate::mqtt::codec::{Reader, Sink};
use crate::mqtt::error::{DecodeError, EncodeError};
use crate::mqtt::header::PacketType;
use crate::mqtt::property::{Properties, PropertyContext};
use crate::mqtt::reason::ReasonCode;

use super::{decode_optional_reason_and_properties, encode_optional_reason_and_properties};

/// DISCONNECT 报文。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Disconnect {
    pub reason_code: ReasonCode,
    pub properties: Properties,
}

impl Disconnect {
    pub(crate) fn decode_body(body: &[u8]) -> Result<Self, DecodeError> {
        let mut reader = Reader::new(body);
        let (reason_code, properties) = decode_optional_reason_and_properties(
            &mut reader,
            PacketType::Disconnect,
            ReasonCode::Success,
            PropertyContext::Disconnect,
        )?;
        reader.finish()?;
        Ok(Self {
            reason_code,
            properties,
        })
    }

    pub(crate) fn encode_body<S: Sink + ?Sized>(&self, sink: &mut S) -> Result<(), EncodeError> {
        encode_optional_reason_and_properties(
            sink,
            self.reason_code,
            &self.properties,
            PacketType::Disconnect,
            PropertyContext::Disconnect,
        )
    }
}
