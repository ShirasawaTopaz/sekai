//! MQTT 固定头（Fixed Header）与报文类型。
//!
//! 固定头由第一字节（高 4 位报文类型 + 低 4 位标志位）与
//! Variable Byte Integer 形式的 Remaining Length 组成。

use alloc::vec::Vec;
use core::fmt;

use crate::mqtt::error::{DecodeError, EncodeError};
use crate::mqtt::varint;

/// MQTT 5.0 控制报文类型（固定头第一字节高 4 位）。
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PacketType {
    /// 客户端请求连接服务端。
    Connect = 1,
    /// 服务端确认连接。
    Connack = 2,
    /// 发布消息。
    Publish = 3,
    /// 发布确认（QoS 1）。
    Puback = 4,
    /// 发布收到（QoS 2 第一步）。
    Pubrec = 5,
    /// 发布释放（QoS 2 第二步）。
    Pubrel = 6,
    /// 发布完成（QoS 2 第三步）。
    Pubcomp = 7,
    /// 客户端订阅。
    Subscribe = 8,
    /// 订阅确认。
    Suback = 9,
    /// 客户端取消订阅。
    Unsubscribe = 10,
    /// 取消订阅确认。
    Unsuback = 11,
    /// Ping 请求。
    Pingreq = 12,
    /// Ping 响应。
    Pingresp = 13,
    /// 断开连接。
    Disconnect = 14,
    /// 增强认证。
    Auth = 15,
}

impl PacketType {
    /// 由报文类型数值构造；0 与 16..=255 返回 `None`。
    pub fn from_u8(value: u8) -> Option<Self> {
        Some(match value {
            1 => Self::Connect,
            2 => Self::Connack,
            3 => Self::Publish,
            4 => Self::Puback,
            5 => Self::Pubrec,
            6 => Self::Pubrel,
            7 => Self::Pubcomp,
            8 => Self::Subscribe,
            9 => Self::Suback,
            10 => Self::Unsubscribe,
            11 => Self::Unsuback,
            12 => Self::Pingreq,
            13 => Self::Pingresp,
            14 => Self::Disconnect,
            15 => Self::Auth,
            _ => return None,
        })
    }

    /// 转换为报文类型数值。
    pub fn as_u8(self) -> u8 {
        self as u8
    }

    /// 固定头第一字节低 4 位的合法默认值：
    /// PUBREL/SUBSCRIBE/UNSUBSCRIBE = 0b0010，其余（含 PUBLISH）= 0b0000。
    pub fn default_flags(self) -> u8 {
        match self {
            Self::Pubrel | Self::Subscribe | Self::Unsubscribe => 0b0010,
            _ => 0b0000,
        }
    }
}

impl fmt::Display for PacketType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            Self::Connect => "CONNECT",
            Self::Connack => "CONNACK",
            Self::Publish => "PUBLISH",
            Self::Puback => "PUBACK",
            Self::Pubrec => "PUBREC",
            Self::Pubrel => "PUBREL",
            Self::Pubcomp => "PUBCOMP",
            Self::Subscribe => "SUBSCRIBE",
            Self::Suback => "SUBACK",
            Self::Unsubscribe => "UNSUBSCRIBE",
            Self::Unsuback => "UNSUBACK",
            Self::Pingreq => "PINGREQ",
            Self::Pingresp => "PINGRESP",
            Self::Disconnect => "DISCONNECT",
            Self::Auth => "AUTH",
        };
        f.write_str(name)
    }
}

/// 校验固定头低 4 位标志位是否合法；返回中文静态描述。
fn validate_flags(packet_type: PacketType, flags: u8) -> Result<(), &'static str> {
    if flags > 0b1111 {
        return Err("固定头标志位超出低 4 位");
    }
    match packet_type {
        PacketType::Publish => {
            let qos = (flags >> 1) & 0b11;
            let dup = flags & 0b1000 != 0;
            if qos == 0b11 {
                return Err("PUBLISH 的 QoS 位不能为保留值 3");
            }
            if qos == 0 && dup {
                return Err("PUBLISH 在 QoS 0 时不能设置 DUP 标志");
            }
            Ok(())
        }
        _ => {
            if flags != packet_type.default_flags() {
                return Err("固定头标志位与报文类型不匹配");
            }
            Ok(())
        }
    }
}

/// MQTT 固定头。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FixedHeader {
    /// 报文类型。
    pub packet_type: PacketType,
    /// 固定头第一字节低 4 位标志。
    pub flags: u8,
    /// Remaining Length。
    pub remaining_length: u32,
}

impl FixedHeader {
    /// 解析固定头，返回 (header, 固定头字节数)。
    ///
    /// 校验报文类型与 flags 合法性；数据不足返回 `Incomplete`。
    pub fn decode(buf: &[u8]) -> Result<(Self, usize), DecodeError> {
        let first = *buf.first().ok_or(DecodeError::Incomplete { needed: 1 })?;
        let packet_type =
            PacketType::from_u8(first >> 4).ok_or(DecodeError::Malformed("报文类型非法"))?;
        let flags = first & 0b1111;
        validate_flags(packet_type, flags).map_err(DecodeError::Malformed)?;
        let (remaining_length, length_bytes) = match varint::decode(&buf[1..]) {
            Ok(decoded) => decoded,
            Err(DecodeError::Incomplete { needed }) => {
                return Err(DecodeError::Incomplete { needed: needed + 1 });
            }
            Err(err) => return Err(err),
        };
        Ok((
            Self {
                packet_type,
                flags,
                remaining_length,
            },
            1 + length_bytes,
        ))
    }

    /// 固定头编码后的字节数：1 + Variable Byte Integer 长度。
    pub fn encoded_len(&self) -> usize {
        1 + varint::encoded_len(self.remaining_length)
    }

    /// 校验 flags 与长度后写出第一字节 + Remaining Length。
    pub fn encode_to(&self, out: &mut Vec<u8>) -> Result<(), EncodeError> {
        validate_flags(self.packet_type, self.flags).map_err(EncodeError::Invalid)?;
        if self.remaining_length > varint::MAX {
            return Err(EncodeError::PacketTooLarge {
                size: 5 + self.remaining_length as usize,
            });
        }
        out.try_reserve(self.encoded_len())
            .map_err(|_| EncodeError::AllocationFailed)?;
        out.push((self.packet_type.as_u8() << 4) | (self.flags & 0b1111));
        varint::encode(self.remaining_length, out)
    }
}

#[cfg(test)]
mod tests {
    use super::{FixedHeader, PacketType};
    use crate::mqtt::error::{DecodeError, EncodeError};
    use crate::mqtt::varint;
    use alloc::string::ToString;
    use alloc::vec::Vec;

    /// (类型, 数值, 默认标志, 显示名) 表。
    const PACKET_TYPES: [(PacketType, u8, u8, &str); 15] = [
        (PacketType::Connect, 1, 0b0000, "CONNECT"),
        (PacketType::Connack, 2, 0b0000, "CONNACK"),
        (PacketType::Publish, 3, 0b0000, "PUBLISH"),
        (PacketType::Puback, 4, 0b0000, "PUBACK"),
        (PacketType::Pubrec, 5, 0b0000, "PUBREC"),
        (PacketType::Pubrel, 6, 0b0010, "PUBREL"),
        (PacketType::Pubcomp, 7, 0b0000, "PUBCOMP"),
        (PacketType::Subscribe, 8, 0b0010, "SUBSCRIBE"),
        (PacketType::Suback, 9, 0b0000, "SUBACK"),
        (PacketType::Unsubscribe, 10, 0b0010, "UNSUBSCRIBE"),
        (PacketType::Unsuback, 11, 0b0000, "UNSUBACK"),
        (PacketType::Pingreq, 12, 0b0000, "PINGREQ"),
        (PacketType::Pingresp, 13, 0b0000, "PINGRESP"),
        (PacketType::Disconnect, 14, 0b0000, "DISCONNECT"),
        (PacketType::Auth, 15, 0b0000, "AUTH"),
    ];

    #[test]
    fn packet_type_roundtrip() {
        for (packet_type, value, flags, name) in PACKET_TYPES {
            assert_eq!(packet_type.as_u8(), value);
            assert_eq!(PacketType::from_u8(value), Some(packet_type));
            assert_eq!(packet_type.default_flags(), flags);
            assert_eq!(packet_type.to_string(), name);
        }
    }

    #[test]
    fn packet_type_unknown() {
        for value in [0u8, 16, 17, 100, 240, 255] {
            assert_eq!(PacketType::from_u8(value), None, "value = {value}");
        }
    }

    #[test]
    fn decode_simple_headers() {
        for (packet_type, value, flags, _) in PACKET_TYPES {
            let bytes = [(value << 4) | flags, 0x00];
            let (header, used) = FixedHeader::decode(&bytes).unwrap();
            assert_eq!(header.packet_type, packet_type);
            assert_eq!(header.flags, flags);
            assert_eq!(header.remaining_length, 0);
            assert_eq!(used, 2);
            assert_eq!(header.encoded_len(), used);
        }
    }

    #[test]
    fn decode_remaining_length_varint() {
        // PUBLISH，Remaining Length = 300（0xAC 0x02）。
        let bytes = [0x30, 0xAC, 0x02, 0xFF];
        let (header, used) = FixedHeader::decode(&bytes).unwrap();
        assert_eq!(header.remaining_length, 300);
        assert_eq!(used, 3);
        assert_eq!(header.encoded_len(), 3);
    }

    #[test]
    fn decode_incomplete() {
        assert_eq!(
            FixedHeader::decode(&[]),
            Err(DecodeError::Incomplete { needed: 1 })
        );
        assert_eq!(
            FixedHeader::decode(&[0x30]),
            Err(DecodeError::Incomplete { needed: 2 })
        );
        assert_eq!(
            FixedHeader::decode(&[0x30, 0x80]),
            Err(DecodeError::Incomplete { needed: 3 })
        );
        assert_eq!(
            FixedHeader::decode(&[0x30, 0x80, 0x80]),
            Err(DecodeError::Incomplete { needed: 4 })
        );
    }

    #[test]
    fn decode_malformed() {
        assert_eq!(
            FixedHeader::decode(&[0x00, 0x00]),
            Err(DecodeError::Malformed("报文类型非法"))
        );
        assert_eq!(
            FixedHeader::decode(&[0x0F, 0x00]),
            Err(DecodeError::Malformed("报文类型非法"))
        );
        // 非最小编码的 Remaining Length。
        assert_eq!(
            FixedHeader::decode(&[0x30, 0x80, 0x00]),
            Err(DecodeError::Malformed("Variable Byte Integer 非最小编码"))
        );
        // 超过 4 字节的 Remaining Length。
        assert_eq!(
            FixedHeader::decode(&[0x30, 0x80, 0x80, 0x80, 0x80, 0x00]),
            Err(DecodeError::Malformed("Variable Byte Integer 超过 4 字节"))
        );
    }

    #[test]
    fn decode_invalid_fixed_flags() {
        for (packet_type, value, flags, _) in PACKET_TYPES {
            if matches!(packet_type, PacketType::Publish) {
                continue;
            }
            let wrong_flags = flags ^ 0b0001;
            let bytes = [(value << 4) | wrong_flags, 0x00];
            assert_eq!(
                FixedHeader::decode(&bytes),
                Err(DecodeError::Malformed("固定头标志位与报文类型不匹配")),
                "packet_type = {packet_type}"
            );
        }
    }

    #[test]
    fn decode_publish_flags() {
        // 合法：QoS 0/1/2 且 QoS 0 不允许 DUP。
        for flags in [
            0b0000u8, 0b0001, 0b0010, 0b0011, 0b0100, 0b0101, 0b1010, 0b1011, 0b1100, 0b1101,
        ] {
            let bytes = [0x30 | flags, 0x00];
            let (header, _) = FixedHeader::decode(&bytes).unwrap();
            assert_eq!(header.flags, flags);
        }
        // QoS 位为 3 非法。
        for flags in [0b0110u8, 0b0111, 0b1110, 0b1111] {
            assert_eq!(
                FixedHeader::decode(&[0x30 | flags, 0x00]),
                Err(DecodeError::Malformed("PUBLISH 的 QoS 位不能为保留值 3")),
                "flags = {flags}"
            );
        }
        // QoS 0 且 DUP=1 非法。
        for flags in [0b1000u8, 0b1001] {
            assert_eq!(
                FixedHeader::decode(&[0x30 | flags, 0x00]),
                Err(DecodeError::Malformed(
                    "PUBLISH 在 QoS 0 时不能设置 DUP 标志"
                )),
                "flags = {flags}"
            );
        }
    }

    #[test]
    fn encoded_len_boundaries() {
        let header = |remaining_length: u32| FixedHeader {
            packet_type: PacketType::Publish,
            flags: 0,
            remaining_length,
        };
        assert_eq!(header(0).encoded_len(), 2);
        assert_eq!(header(127).encoded_len(), 2);
        assert_eq!(header(128).encoded_len(), 3);
        assert_eq!(header(16_383).encoded_len(), 3);
        assert_eq!(header(16_384).encoded_len(), 4);
        assert_eq!(header(2_097_151).encoded_len(), 4);
        assert_eq!(header(2_097_152).encoded_len(), 5);
        assert_eq!(header(varint::MAX).encoded_len(), 5);
    }

    #[test]
    fn encode_to_roundtrip() {
        for (packet_type, _, flags, _) in PACKET_TYPES {
            for remaining_length in [
                0u32,
                1,
                127,
                128,
                16_383,
                16_384,
                2_097_151,
                2_097_152,
                varint::MAX,
            ] {
                let header = FixedHeader {
                    packet_type,
                    flags,
                    remaining_length,
                };
                let mut out = Vec::new();
                header.encode_to(&mut out).unwrap();
                assert_eq!(header.encoded_len(), out.len());
                let (decoded, used) = FixedHeader::decode(&out).unwrap();
                assert_eq!(decoded, header);
                assert_eq!(used, out.len());
            }
        }
    }

    #[test]
    fn encode_to_rejects() {
        let oversized = FixedHeader {
            packet_type: PacketType::Publish,
            flags: 0,
            remaining_length: varint::MAX + 1,
        };
        assert_eq!(
            oversized.encode_to(&mut Vec::new()),
            Err(EncodeError::PacketTooLarge {
                size: 5 + varint::MAX as usize + 1,
            })
        );

        let connect_flags = FixedHeader {
            packet_type: PacketType::Connect,
            flags: 1,
            remaining_length: 0,
        };
        assert_eq!(
            connect_flags.encode_to(&mut Vec::new()),
            Err(EncodeError::Invalid("固定头标志位与报文类型不匹配"))
        );

        let pubrel_flags = FixedHeader {
            packet_type: PacketType::Pubrel,
            flags: 0,
            remaining_length: 0,
        };
        assert_eq!(
            pubrel_flags.encode_to(&mut Vec::new()),
            Err(EncodeError::Invalid("固定头标志位与报文类型不匹配"))
        );

        let publish_qos3 = FixedHeader {
            packet_type: PacketType::Publish,
            flags: 0b0110,
            remaining_length: 0,
        };
        assert_eq!(
            publish_qos3.encode_to(&mut Vec::new()),
            Err(EncodeError::Invalid("PUBLISH 的 QoS 位不能为保留值 3"))
        );

        let publish_dup = FixedHeader {
            packet_type: PacketType::Publish,
            flags: 0b1000,
            remaining_length: 0,
        };
        assert_eq!(
            publish_dup.encode_to(&mut Vec::new()),
            Err(EncodeError::Invalid("PUBLISH 在 QoS 0 时不能设置 DUP 标志"))
        );

        let high_bits = FixedHeader {
            packet_type: PacketType::Publish,
            flags: 0b1_0000,
            remaining_length: 0,
        };
        assert_eq!(
            high_bits.encode_to(&mut Vec::new()),
            Err(EncodeError::Invalid("固定头标志位超出低 4 位"))
        );
    }
}
