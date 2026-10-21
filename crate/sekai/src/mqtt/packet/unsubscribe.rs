//! UNSUBSCRIBE 报文。

use alloc::vec::Vec;

use crate::mqtt::codec::{Reader, Sink, write_packet_id};
use crate::mqtt::error::{DecodeError, EncodeError};
use crate::mqtt::property::{Properties, PropertyContext};
use crate::mqtt::topic::TopicFilter;

use super::decode_packet_id;

/// UNSUBSCRIBE 报文。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unsubscribe {
    pub packet_id: u16,
    pub properties: Properties,
    pub filters: Vec<TopicFilter>,
}

impl Unsubscribe {
    pub(crate) fn decode_body(body: &[u8]) -> Result<Self, DecodeError> {
        let mut reader = Reader::new(body);
        let packet_id = decode_packet_id(&mut reader)?;
        let properties = Properties::decode(&mut reader, PropertyContext::Unsubscribe)?;

        let mut filters = Vec::new();
        while !reader.is_empty() {
            let filter = TopicFilter::decode(&mut reader)?;
            filters
                .try_reserve(1)
                .map_err(|_| DecodeError::AllocationFailed)?;
            filters.push(filter);
        }

        if filters.is_empty() {
            return Err(DecodeError::ProtocolError(
                "UNSUBSCRIBE 至少需要一个主题过滤器",
            ));
        }
        reader.finish()?;
        Ok(Self {
            packet_id,
            properties,
            filters,
        })
    }

    pub(crate) fn encode_body<S: Sink + ?Sized>(&self, sink: &mut S) -> Result<(), EncodeError> {
        if self.filters.is_empty() {
            return Err(EncodeError::Invalid("UNSUBSCRIBE 至少需要一个主题过滤器"));
        }
        write_packet_id(sink, self.packet_id)?;
        self.properties
            .encode_into(sink, PropertyContext::Unsubscribe)?;
        for filter in &self.filters {
            filter.encode_into(sink)?;
        }
        Ok(())
    }
}
