//! SUBSCRIBE 报文。

use alloc::vec::Vec;

use crate::mqtt::codec::{Reader, Sink, write_packet_id, write_u8};
use crate::mqtt::error::{DecodeError, EncodeError};
use crate::mqtt::property::{Properties, PropertyContext};
use crate::mqtt::topic::{Subscription, SubscriptionOptions, TopicFilter};

use super::decode_packet_id;

/// SUBSCRIBE 报文。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Subscribe {
    pub packet_id: u16,
    pub properties: Properties,
    pub subscriptions: Vec<Subscription>,
}

impl Subscribe {
    pub(crate) fn decode_body(body: &[u8]) -> Result<Self, DecodeError> {
        let mut reader = Reader::new(body);
        let packet_id = decode_packet_id(&mut reader)?;
        let properties = Properties::decode(&mut reader, PropertyContext::Subscribe)?;

        let mut subscriptions = Vec::new();
        while !reader.is_empty() {
            let filter = TopicFilter::decode(&mut reader)?;
            let raw_options = reader.u8()?;
            if raw_options & 0xC0 != 0 {
                return Err(DecodeError::Malformed("SUBSCRIBE 订阅选项保留位必须为 0"));
            }
            let options =
                SubscriptionOptions::try_from(raw_options).map_err(DecodeError::ProtocolError)?;
            subscriptions
                .try_reserve(1)
                .map_err(|_| DecodeError::AllocationFailed)?;
            subscriptions.push(Subscription { filter, options });
        }

        if subscriptions.is_empty() {
            return Err(DecodeError::ProtocolError("SUBSCRIBE 至少需要一个订阅"));
        }
        reader.finish()?;
        Ok(Self {
            packet_id,
            properties,
            subscriptions,
        })
    }

    pub(crate) fn encode_body<S: Sink + ?Sized>(&self, sink: &mut S) -> Result<(), EncodeError> {
        if self.subscriptions.is_empty() {
            return Err(EncodeError::Invalid("SUBSCRIBE 至少需要一个订阅"));
        }
        write_packet_id(sink, self.packet_id)?;
        self.properties
            .encode_into(sink, PropertyContext::Subscribe)?;
        for subscription in &self.subscriptions {
            if subscription.options.retain_handling > 2 {
                return Err(EncodeError::Invalid(
                    "SUBSCRIBE retain_handling 必须在 0..=2 之间",
                ));
            }
            subscription.filter.encode_into(sink)?;
            write_u8(sink, subscription.options.to_byte());
        }
        Ok(())
    }
}
