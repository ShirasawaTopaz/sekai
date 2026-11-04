//! MQTT 连续字节流分帧。
//!
//! `Packet::decode` 本身支持半包（返回 `Incomplete`），但调用方需要一个
//! 有界缓冲把 TCP 读到的字节累积起来。本模块只做这一件事，并限制最大
//! 报文长度，避免异常长度导致无限增长。

use alloc::vec::Vec;

use sekai::mqtt::error::DecodeError;
use sekai::mqtt::packet::Packet;

/// 有界 MQTT 分帧器。
pub struct StreamDecoder {
    buffer: Vec<u8>,
    max_packet: u32,
    max_buffer: usize,
}

impl StreamDecoder {
    /// 用入站最大报文长度创建分帧器。
    pub fn new(max_packet: u32) -> Self {
        let max_packet = max_packet.max(2);
        let max_buffer = (max_packet as usize)
            .saturating_add(5)
            .saturating_add(4 * 1024);
        Self {
            buffer: Vec::new(),
            max_packet,
            max_buffer,
        }
    }

    /// 追加一批字节；缓冲超过安全上限时返回 `PacketTooLarge`。
    pub fn push(&mut self, bytes: &[u8]) -> Result<(), DecodeError> {
        if self.buffer.len().saturating_add(bytes.len()) > self.max_buffer {
            return Err(DecodeError::PacketTooLarge {
                size: self.buffer.len().saturating_add(bytes.len()),
                max: self.max_packet,
            });
        }
        self.buffer.extend_from_slice(bytes);
        Ok(())
    }

    /// 取出下一个完整报文；半包返回 `Ok(None)`。
    pub fn next(&mut self) -> Result<Option<Packet>, DecodeError> {
        match Packet::decode_with_limit(&self.buffer, self.max_packet) {
            Ok((packet, used)) => {
                self.buffer.drain(..used);
                Ok(Some(packet))
            }
            Err(DecodeError::Incomplete { needed }) => {
                if needed as u64 > u64::from(self.max_packet) {
                    Err(DecodeError::PacketTooLarge {
                        size: needed,
                        max: self.max_packet,
                    })
                } else {
                    Ok(None)
                }
            }
            Err(error) => Err(error),
        }
    }

    /// 当前缓冲字节数（测试/诊断用）。
    #[cfg(test)]
    pub fn buffered(&self) -> usize {
        self.buffer.len()
    }
}

#[cfg(test)]
mod tests {
    use super::StreamDecoder;
    use alloc::vec::Vec;
    use sekai::mqtt::packet::{Packet, Publish};
    use sekai::mqtt::property::Properties;
    use sekai::mqtt::qos::QoS;
    use sekai::mqtt::topic::TopicName;

    fn publish_frame(payload: &[u8]) -> Vec<u8> {
        Packet::Publish(Publish {
            dup: false,
            qos: QoS::AtMostOnce,
            retain: false,
            topic: Some(TopicName::new("a/b").unwrap()),
            packet_id: None,
            properties: Properties::new(),
            payload: payload.to_vec(),
        })
        .encode()
        .unwrap()
    }

    #[test]
    fn decodes_at_every_split_boundary() {
        let frame = publish_frame(b"hello");
        for split in 0..=frame.len() {
            let mut decoder = StreamDecoder::new(1024);
            decoder.push(&frame[..split]).unwrap();
            let first = decoder.next().unwrap();
            if split == frame.len() {
                assert!(first.is_some(), "split = {split}");
                assert!(decoder.next().unwrap().is_none());
            } else {
                assert!(first.is_none(), "split = {split}");
                decoder.push(&frame[split..]).unwrap();
                assert!(decoder.next().unwrap().is_some(), "split = {split}");
            }
        }
    }

    #[test]
    fn decodes_multiple_packets() {
        let mut bytes = publish_frame(b"one");
        bytes.extend_from_slice(&publish_frame(b"two"));
        let mut decoder = StreamDecoder::new(1024);
        decoder.push(&bytes).unwrap();
        assert!(decoder.next().unwrap().is_some());
        assert!(decoder.next().unwrap().is_some());
        assert!(decoder.next().unwrap().is_none());
        assert_eq!(decoder.buffered(), 0);
    }

    #[test]
    fn rejects_oversized_packet() {
        let frame = publish_frame(b"hello");
        let mut decoder = StreamDecoder::new(5);
        decoder.push(&frame).unwrap();
        assert!(decoder.next().is_err());
    }
}
