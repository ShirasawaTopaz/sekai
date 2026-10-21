//! MQTT 5.0 报文层独立验收测试（黑盒）。
//!
//! 本文件是报文层实现的独立 oracle：只通过 `sekai` 的公开 API 断言行为，
//! 不读取实现内部结构。所有黄金字节向量均按 MQTT 5.0 规范手工推导，
//! 注释中逐字节说明含义（十六进制）。
//!
//! 覆盖范围：
//! - 15 种控制报文的典型/最短黄金向量：decode → encode 字节一致；
//! - 最短合法编码分支（PUBACK 系列 RL=2/3/>=4；DISCONNECT/AUTH RL=0/1/>=2）；
//! - 27 个属性 ID 的字节级往返、允许集/重复/取值/上下文错误；
//! - 原因码表驱动（try_from_u8 往返 + allowed_in 正反例）；
//! - 主题名/主题过滤器/订阅选项边界；
//! - 错误分级（Malformed / ProtocolError / UnsupportedProtocolVersion / ...）；
//! - `Incomplete.needed` 前缀遍历、`decode_with_limit`、`encoded_len` 一致性；
//! - 固定种子 xorshift 随机字节 fuzz（不 panic + 成功解码后可重编码往返）。
//!
//! 集成测试 crate 默认链接 std，可自由使用 `Vec`/`format!` 等。

use sekai::mqtt::error::{DecodeError, EncodeError};
use sekai::mqtt::header::{FixedHeader, PacketType};
use sekai::mqtt::packet::{
    Ack, Auth, Connack, Connect, Disconnect, Packet, Publish, Suback, Subscribe, Unsuback,
    Unsubscribe, Will,
};
use sekai::mqtt::property::{Properties, PropertyContext, PropertyId};
use sekai::mqtt::qos::QoS;
use sekai::mqtt::reason::ReasonCode;
use sekai::mqtt::topic::{Subscription, SubscriptionOptions, TopicFilter, TopicName};
use sekai::mqtt::varint;

// ===========================================================================
// 通用辅助
// ===========================================================================

/// 把字节切片格式化为十六进制，便于失败信息定位。
fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 3);
    for (index, byte) in bytes.iter().enumerate() {
        if index > 0 {
            out.push(' ');
        }
        out.push_str(&format!("{byte:02X}"));
    }
    out
}

/// 解码并要求恰好消耗传入的全部字节。
fn decode_ok(bytes: &[u8]) -> Packet {
    match Packet::decode(bytes) {
        Ok((packet, used)) => {
            assert_eq!(
                used,
                bytes.len(),
                "decode 应恰好消耗全部字节：{}",
                hex(bytes)
            );
            packet
        }
        Err(err) => panic!("解码本应成功却失败：{}，错误：{err:?}", hex(bytes)),
    }
}

/// 解码并断言失败，返回错误。
fn decode_err(bytes: &[u8]) -> DecodeError {
    match Packet::decode(bytes) {
        Ok((packet, used)) => {
            panic!(
                "解码本应失败却成功：{}，报文：{packet:?}，消耗 {used}",
                hex(bytes)
            )
        }
        Err(err) => err,
    }
}

/// 断言解码错误是 Malformed，返回内部消息。
fn assert_malformed(bytes: &[u8]) -> &'static str {
    match decode_err(bytes) {
        DecodeError::Malformed(message) => message,
        other => panic!("期望 Malformed，实际 {other:?}：{}", hex(bytes)),
    }
}

/// 断言解码错误是 ProtocolError，返回内部消息。
fn assert_protocol_error(bytes: &[u8]) -> &'static str {
    match decode_err(bytes) {
        DecodeError::ProtocolError(message) => message,
        other => panic!("期望 ProtocolError，实际 {other:?}：{}", hex(bytes)),
    }
}

/// 断言解码错误是 UnsupportedProtocolVersion 且版本号一致。
fn assert_unsupported_version(bytes: &[u8], expected: u8) {
    match decode_err(bytes) {
        DecodeError::UnsupportedProtocolVersion(version) => {
            assert_eq!(
                version,
                expected,
                "UnsupportedProtocolVersion 版本号不符：{}",
                hex(bytes)
            );
        }
        other => panic!(
            "期望 UnsupportedProtocolVersion，实际 {other:?}：{}",
            hex(bytes)
        ),
    }
}

/// 断言 `Packet::decode` 对完整合法报文的行为：
/// 类型正确、恰好消费全部字节、再编码字节一致、再解码结构相等、
/// `encoded_len` 与实际编码长度一致、`encode_to` 与 `encode` 一致。
fn assert_wire_roundtrip(bytes: &[u8], expected_type: PacketType) {
    let packet = decode_ok(bytes);
    assert_eq!(
        packet.packet_type(),
        expected_type,
        "报文类型不符：{}",
        hex(bytes)
    );
    assert_eq!(
        packet.encoded_len().expect("encoded_len 不应失败"),
        bytes.len(),
        "encoded_len 不一致：{}",
        hex(bytes)
    );
    let encoded = packet.encode().expect("encode 不应失败");
    assert_eq!(encoded, bytes, "重编码字节不一致：{}", hex(bytes));
    assert_eq!(
        decode_ok(&encoded),
        packet,
        "再解码结构不相等：{}",
        hex(bytes)
    );

    let mut via_encode_to = Vec::new();
    packet
        .encode_to(&mut via_encode_to)
        .expect("encode_to 不应失败");
    assert_eq!(
        via_encode_to,
        encoded,
        "encode_to 与 encode 结果不一致：{}",
        hex(bytes)
    );

    // 解码只消费一个报文，固定头之外的多余字节必须留给调用方。
    let mut with_trailing = bytes.to_vec();
    with_trailing.extend_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF]);
    match Packet::decode(&with_trailing) {
        Ok((trailing_packet, used)) => {
            assert_eq!(used, bytes.len(), "多余字节不应被消费：{}", hex(bytes));
            assert_eq!(
                trailing_packet,
                packet,
                "带尾随字节解码结果不一致：{}",
                hex(bytes)
            );
        }
        Err(err) => panic!("带尾随字节解码失败：{}，错误：{err:?}", hex(bytes)),
    }

    assert_incomplete_prefixes(bytes);
}

/// 遍历一个合法报文的全部截断前缀，断言都返回正确的 `Incomplete.needed`：
/// - 空缓冲区：needed = 1（至少还差固定头第一字节）；
/// - 固定头未读完：needed = 前缀长度 + 1（每次至少还差 1 个固定头字节）；
/// - 固定头已读完：needed = 固定头长度 + Remaining Length（整个报文总长度）。
fn assert_incomplete_prefixes(bytes: &[u8]) {
    let (header, header_len) = FixedHeader::decode(bytes).expect("合法报文的固定头必须可解码");
    let total = header_len + header.remaining_length as usize;
    assert_eq!(
        total,
        bytes.len(),
        "黄金向量长度与 Remaining Length 不一致：{}",
        hex(bytes)
    );

    for prefix_len in 0..bytes.len() {
        let prefix = &bytes[..prefix_len];
        match Packet::decode(prefix) {
            Err(DecodeError::Incomplete { needed }) => {
                // 固定头未收完时 needed 只要求是保守下界（spec-core §3.4 只约束了
                // 空缓冲区和「第一字节后 VarInt 不足」两种情形）；固定头完整时
                // 必须精确等于声明的报文总长。
                if prefix_len == 0 {
                    assert_eq!(needed, 1, "空缓冲区应报告 needed = 1");
                } else if prefix_len < header_len {
                    assert!(
                        needed > prefix_len,
                        "前缀长度 {prefix_len} 的 Incomplete.needed={needed} 应大于前缀长度：{}",
                        hex(bytes)
                    );
                } else {
                    assert_eq!(
                        needed,
                        total,
                        "前缀长度 {prefix_len} 的 Incomplete.needed 应为报文总长 {total}：{}",
                        hex(bytes)
                    );
                }
            }
            Ok((packet, used)) => panic!(
                "截断报文（前缀 {prefix_len} 字节）不应解码成功：{}，得到 {packet:?}，消耗 {used}",
                hex(bytes)
            ),
            Err(other) => panic!(
                "截断报文（前缀 {prefix_len} 字节）应返回 Incomplete，实际 {other:?}：{}",
                hex(bytes)
            ),
        }
    }

    match Packet::decode(&[]) {
        Err(DecodeError::Incomplete { needed }) => {
            assert_eq!(needed, 1, "空缓冲区应报告 needed = 1")
        }
        other => panic!("空缓冲区应返回 Incomplete {{ needed: 1 }}，实际 {other:?}"),
    }
}

/// 断言某个报文恰好比 `max_packet_size` 大 1 时被 `decode_with_limit` 拒绝，
/// 且返回的 size 是报文总长度。
fn assert_limit_rejects(bytes: &[u8]) {
    let total = bytes.len() as u32;
    let limit = total - 1;
    match Packet::decode_with_limit(bytes, limit) {
        Err(DecodeError::PacketTooLarge { size, max }) => {
            assert_eq!(
                size,
                bytes.len(),
                "PacketTooLarge.size 应为报文总长度：{}",
                hex(bytes)
            );
            assert_eq!(max, limit, "PacketTooLarge.max 应为限制值：{}", hex(bytes));
        }
        Ok((packet, used)) => panic!(
            "超过 max_packet_size 的报文不应解码成功：{}，得到 {packet:?}，消耗 {used}",
            hex(bytes)
        ),
        Err(other) => panic!("期望 PacketTooLarge，实际 {other:?}：{}", hex(bytes)),
    }
}

// ===========================================================================
// Variable Byte Integer（public API 独立验收）
// ===========================================================================

/// 规范 `1.5.5 的全部边界值：编码字节、编码长度、解码往返。
#[test]
fn varint_边界值编码解码往返() {
    let cases: &[(u32, &[u8])] = &[
        (0, &[0x00]),
        (1, &[0x01]),
        (127, &[0x7F]),
        (128, &[0x80, 0x01]),
        (16_383, &[0xFF, 0x7F]),
        (16_384, &[0x80, 0x80, 0x01]),
        (2_097_151, &[0xFF, 0xFF, 0x7F]),
        (2_097_152, &[0x80, 0x80, 0x80, 0x01]),
        (268_435_455, &[0xFF, 0xFF, 0xFF, 0x7F]),
    ];
    for &(value, expected) in cases {
        assert_eq!(
            varint::encoded_len(value),
            expected.len(),
            "encoded_len({value}) 不符"
        );
        let mut encoded = Vec::new();
        varint::encode(value, &mut encoded).expect("encode 不应失败");
        assert_eq!(encoded, expected, "encode({value}) 字节不符");
        assert_eq!(
            varint::decode(expected),
            Ok((value, expected.len())),
            "decode({value}) 不符"
        );

        // 任意截断前缀都必须返回 Incomplete { needed: 前缀长度 + 1 }。
        for prefix_len in 0..expected.len() {
            let expected_needed = prefix_len + 1;
            assert_eq!(
                varint::decode(&expected[..prefix_len]),
                Err(DecodeError::Incomplete {
                    needed: expected_needed
                }),
                "value={value} 截断到 {prefix_len} 字节"
            );
        }

        // 尾随字节不属于 VarInt，不能被消费。
        let mut extended = expected.to_vec();
        extended.push(0xAA);
        assert_eq!(
            varint::decode(&extended),
            Ok((value, expected.len())),
            "value={value} 带尾随字节时解码长度不符"
        );
    }
}

/// VarInt 非最小编码必须拒绝（规范 MQTT-1.5.5-1 要求最小编码）。
#[test]
fn varint_非最小编码被拒() {
    // 0 编码成 0x80 0x00（多一个延续字节）。
    assert_malformed_varint(&[0x80, 0x00]);
    // 127 编码成 0xFF 0x00。
    assert_malformed_varint(&[0xFF, 0x00]);
    // 128 编码成 0x80 0x81 0x00。
    assert_malformed_varint(&[0x80, 0x81, 0x00]);
    // 2_097_151 编码成 4 字节非最小形式。
    assert_malformed_varint(&[0xFF, 0xFF, 0xFF, 0x00]);
}

fn assert_malformed_varint(bytes: &[u8]) {
    match varint::decode(bytes) {
        Err(DecodeError::Malformed(message)) => {
            assert!(!message.is_empty(), "Malformed 消息不应为空");
        }
        other => panic!(
            "期望非最小编码返回 Malformed，实际 {other:?}：{}",
            hex(bytes)
        ),
    }
}

/// 4 字节仍不终止 / 第 5 字节继续：Malformed；缓冲区不足：Incomplete。
#[test]
fn varint_超长与截断() {
    // 4 个字节全部带延续位：已不可能是合法 VarInt。
    assert_malformed_varint(&[0xFF, 0xFF, 0xFF, 0xFF]);
    // 5 个字节（第 5 个带延续位）：超过 4 字节上限。
    assert_malformed_varint(&[0xFF, 0xFF, 0xFF, 0xFF, 0x01]);
    // 空输入与前缀不足：Incomplete，且 needed = 已有字节 + 1。
    assert_eq!(
        varint::decode(&[]),
        Err(DecodeError::Incomplete { needed: 1 })
    );
    assert_eq!(
        varint::decode(&[0x80]),
        Err(DecodeError::Incomplete { needed: 2 })
    );
    assert_eq!(
        varint::decode(&[0x80, 0x80]),
        Err(DecodeError::Incomplete { needed: 3 })
    );
    assert_eq!(
        varint::decode(&[0x80, 0x80, 0x80]),
        Err(DecodeError::Incomplete { needed: 4 })
    );
}

/// 超出 268_435_455 的编码必须返回 ValueOutOfRange。
#[test]
fn varint_超出上限编码失败() {
    for value in [268_435_456_u32, 1_000_000_000, u32::MAX] {
        let mut out = Vec::new();
        let expected = EncodeError::ValueOutOfRange {
            field: "Variable Byte Integer",
            value,
            max: varint::MAX,
        };
        assert_eq!(
            varint::encode(value, &mut out),
            Err(expected),
            "encode({value}) 应失败"
        );
        assert!(out.is_empty(), "失败的 encode({value}) 不应写入任何字节");
        assert_eq!(
            varint::encoded_len(value),
            4,
            "超出上限时 encoded_len 按 4 返回"
        );
    }
    assert_eq!(varint::MAX, 268_435_455);
}

// ===========================================================================
// QoS（public API 独立验收）
// ===========================================================================

#[test]
fn qos_编号与显示() {
    let cases = [
        (QoS::AtMostOnce, 0_u8),
        (QoS::AtLeastOnce, 1),
        (QoS::ExactlyOnce, 2),
    ];
    for (qos, value) in cases {
        assert_eq!(qos.as_u8(), value);
        assert_eq!(QoS::from_u8(value), Some(qos));
    }
    for invalid in [3_u8, 4, 127, 255] {
        assert_eq!(
            QoS::from_u8(invalid),
            None,
            "QoS::from_u8({invalid}) 应为 None"
        );
    }
    assert_eq!(format!("{}", QoS::AtMostOnce), "QoS 0");
    assert_eq!(format!("{}", QoS::AtLeastOnce), "QoS 1");
    assert_eq!(format!("{}", QoS::ExactlyOnce), "QoS 2");
}

// ===========================================================================
// 固定头与报文类型（public API 独立验收）
// ===========================================================================

const ALL_PACKET_TYPES: &[PacketType] = &[
    PacketType::Connect,
    PacketType::Connack,
    PacketType::Publish,
    PacketType::Puback,
    PacketType::Pubrec,
    PacketType::Pubrel,
    PacketType::Pubcomp,
    PacketType::Subscribe,
    PacketType::Suback,
    PacketType::Unsubscribe,
    PacketType::Unsuback,
    PacketType::Pingreq,
    PacketType::Pingresp,
    PacketType::Disconnect,
    PacketType::Auth,
];

#[test]
fn packet_type_编号往返与显示() {
    assert_eq!(ALL_PACKET_TYPES.len(), 15);
    for (index, packet_type) in ALL_PACKET_TYPES.iter().enumerate() {
        let value = (index + 1) as u8;
        assert_eq!(packet_type.as_u8(), value);
        assert_eq!(PacketType::from_u8(value), Some(*packet_type));
    }
    assert_eq!(PacketType::from_u8(0), None, "报文类型 0 非法");
    for value in 16..=u8::MAX {
        assert_eq!(PacketType::from_u8(value), None, "报文类型 {value} 非法");
    }

    let names = [
        "CONNECT",
        "CONNACK",
        "PUBLISH",
        "PUBACK",
        "PUBREC",
        "PUBREL",
        "PUBCOMP",
        "SUBSCRIBE",
        "SUBACK",
        "UNSUBSCRIBE",
        "UNSUBACK",
        "PINGREQ",
        "PINGRESP",
        "DISCONNECT",
        "AUTH",
    ];
    for (packet_type, name) in ALL_PACKET_TYPES.iter().zip(names) {
        assert_eq!(format!("{packet_type}"), name, "报文类型显示名不符");
    }
}

#[test]
fn packet_type_默认flags() {
    for packet_type in ALL_PACKET_TYPES {
        let expected = match packet_type {
            PacketType::Pubrel | PacketType::Subscribe | PacketType::Unsubscribe => 0b0010,
            _ => 0b0000,
        };
        assert_eq!(
            packet_type.default_flags(),
            expected,
            "{packet_type} 默认 flags 不符"
        );
    }
}

#[test]
fn fixed_header_十五种类型解码编码() {
    for (index, packet_type) in ALL_PACKET_TYPES.iter().enumerate() {
        let first_byte = ((index as u8 + 1) << 4) | packet_type.default_flags();
        // 最短固定头：RL = 0。
        let shortest = [first_byte, 0x00];
        let (header, used) = FixedHeader::decode(&shortest).expect("固定头应可解码");
        assert_eq!(used, 2);
        assert_eq!(header.packet_type, *packet_type);
        assert_eq!(header.flags, packet_type.default_flags());
        assert_eq!(header.remaining_length, 0);
        assert_eq!(header.encoded_len(), 2);

        let mut encoded = Vec::new();
        header.encode_to(&mut encoded).expect("固定头编码不应失败");
        assert_eq!(encoded, shortest, "固定头重编码不一致：{packet_type}");

        // 多字节 Remaining Length：200 -> 0xC8 0x01。
        let with_body = [first_byte, 0xC8, 0x01];
        let (header, used) = FixedHeader::decode(&with_body).expect("固定头应可解码");
        assert_eq!(used, 3);
        assert_eq!(header.remaining_length, 200);
        assert_eq!(header.encoded_len(), 3);
        let mut encoded = Vec::new();
        header.encode_to(&mut encoded).expect("固定头编码不应失败");
        assert_eq!(
            encoded, with_body,
            "多字节 RL 固定头重编码不一致：{packet_type}"
        );
    }
}

#[test]
fn fixed_header_flags校验() {
    // CONNECT 的 flags 必须为 0：0x11 = CONNECT + flags 0b0001。
    assert_malformed_fixed_header(&[0x11, 0x00]);
    // PINGREQ 的 flags 必须为 0：0xC1。
    assert_malformed_fixed_header(&[0xC1, 0x00]);
    // PUBREL 的 flags 必须为 0b0010：0x60 非法、0x61 非法。
    assert_malformed_fixed_header(&[0x60, 0x00]);
    assert_malformed_fixed_header(&[0x61, 0x00]);
    // SUBSCRIBE / UNSUBSCRIBE 同理。
    assert_malformed_fixed_header(&[0x80, 0x00]);
    assert_malformed_fixed_header(&[0xA0, 0x00]);
    // PUBLISH QoS=3（0b0110）：0x36 非法。
    assert_malformed_fixed_header(&[0x36, 0x00]);
    // PUBLISH QoS=0 且 DUP=1（0b1000）：0x38 非法。
    assert_malformed_fixed_header(&[0x38, 0x00]);
    // 报文类型 0（高四位 0000）：0x00 非法。
    assert_malformed_fixed_header(&[0x00, 0x00]);
    assert_malformed_fixed_header(&[0x0F, 0x00]);

    // 合法 PUBLISH flags：DUP=1 QoS=2 RETAIN=1 -> 0b1101 -> 0x3D。
    let (header, _) = FixedHeader::decode(&[0x3D, 0x00]).expect("PUBLISH flags 0b1101 合法");
    assert_eq!(header.packet_type, PacketType::Publish);
    assert_eq!(header.flags, 0b1101);
}

fn assert_malformed_fixed_header(bytes: &[u8]) {
    match FixedHeader::decode(bytes) {
        Err(DecodeError::Malformed(message)) => {
            assert!(!message.is_empty(), "Malformed 消息不应为空")
        }
        other => panic!("期望固定头 Malformed，实际 {other:?}：{}", hex(bytes)),
    }
}

#[test]
fn fixed_header_截断返回incomplete() {
    // 空缓冲区：还差固定头第一字节。
    assert_eq!(
        FixedHeader::decode(&[]).map(|_| ()),
        Err(DecodeError::Incomplete { needed: 1 })
    );
    // 只有第一字节：还差 1 个 VarInt 字节。
    assert_eq!(
        FixedHeader::decode(&[0x30]).map(|_| ()),
        Err(DecodeError::Incomplete { needed: 2 })
    );
    // VarInt 已有 1 个延续字节：还差 1 个。
    assert_eq!(
        FixedHeader::decode(&[0x30, 0x80]).map(|_| ()),
        Err(DecodeError::Incomplete { needed: 3 })
    );
    assert_eq!(
        FixedHeader::decode(&[0x30, 0x80, 0x80]).map(|_| ()),
        Err(DecodeError::Incomplete { needed: 4 })
    );
    // 4 个字节全部带延续位：Malformed（不是 Incomplete）。
    assert_malformed_fixed_header(&[0x30, 0xFF, 0xFF, 0xFF, 0xFF]);
}

#[test]
fn fixed_header_非法flags编码失败与超大长度() {
    for packet_type in ALL_PACKET_TYPES {
        let invalid_flags = match packet_type {
            PacketType::Pubrel | PacketType::Subscribe | PacketType::Unsubscribe => 0b0000,
            // PUBLISH 的 flags 位有语义：0b0001 是合法的 RETAIN，用 QoS=3（0b0110）制造非法。
            PacketType::Publish => 0b0110,
            _ => 0b0001,
        };
        let header = FixedHeader {
            packet_type: *packet_type,
            flags: invalid_flags,
            remaining_length: 0,
        };
        let mut out = Vec::new();
        assert!(
            matches!(header.encode_to(&mut out), Err(EncodeError::Invalid(_))),
            "{packet_type} 非法 flags 编码应失败"
        );
        assert!(out.is_empty(), "失败的固定头编码不应写入字节");
    }

    // Remaining Length 超过 VarInt 上限：PacketTooLarge { size: 5 + RL }。
    let huge = FixedHeader {
        packet_type: PacketType::Publish,
        flags: 0,
        remaining_length: varint::MAX + 1,
    };
    let mut out = Vec::new();
    match huge.encode_to(&mut out) {
        Err(EncodeError::PacketTooLarge { size }) => {
            assert_eq!(size, 5 + varint::MAX as usize + 1);
        }
        other => panic!("期望 PacketTooLarge，实际 {other:?}"),
    }
    // 边界值 MAX 可以编码（第 5 个固定头字节是 VarInt 的第 4 字节）。
    let max = FixedHeader {
        packet_type: PacketType::Publish,
        flags: 0,
        remaining_length: varint::MAX,
    };
    assert_eq!(max.encoded_len(), 5);
    let mut out = Vec::new();
    max.encode_to(&mut out)
        .expect("最大 Remaining Length 应可编码");
    assert_eq!(out, [0x30, 0xFF, 0xFF, 0xFF, 0x7F]);
}

// ===========================================================================
// 原因码（public API 独立验收）
// ===========================================================================

const ALL_REASON_CODES: &[ReasonCode] = &[
    ReasonCode::Success,
    ReasonCode::GrantedQos1,
    ReasonCode::GrantedQos2,
    ReasonCode::DisconnectWithWillMessage,
    ReasonCode::NoMatchingSubscribers,
    ReasonCode::NoSubscriptionExisted,
    ReasonCode::ContinueAuthentication,
    ReasonCode::ReAuthenticate,
    ReasonCode::UnspecifiedError,
    ReasonCode::MalformedPacket,
    ReasonCode::ProtocolError,
    ReasonCode::ImplementationSpecificError,
    ReasonCode::UnsupportedProtocolVersion,
    ReasonCode::ClientIdentifierNotValid,
    ReasonCode::BadUserNameOrPassword,
    ReasonCode::NotAuthorized,
    ReasonCode::ServerUnavailable,
    ReasonCode::ServerBusy,
    ReasonCode::Banned,
    ReasonCode::ServerShuttingDown,
    ReasonCode::BadAuthenticationMethod,
    ReasonCode::KeepAliveTimeout,
    ReasonCode::SessionTakenOver,
    ReasonCode::TopicFilterInvalid,
    ReasonCode::TopicNameInvalid,
    ReasonCode::PacketIdentifierInUse,
    ReasonCode::PacketIdentifierNotFound,
    ReasonCode::ReceiveMaximumExceeded,
    ReasonCode::TopicAliasInvalid,
    ReasonCode::PacketTooLarge,
    ReasonCode::MessageRateTooHigh,
    ReasonCode::QuotaExceeded,
    ReasonCode::AdministrativeAction,
    ReasonCode::PayloadFormatInvalid,
    ReasonCode::RetainNotSupported,
    ReasonCode::QosNotSupported,
    ReasonCode::UseAnotherServer,
    ReasonCode::ServerMoved,
    ReasonCode::SharedSubscriptionsNotSupported,
    ReasonCode::ConnectionRateExceeded,
    ReasonCode::MaximumConnectTime,
    ReasonCode::SubscriptionIdentifiersNotSupported,
    ReasonCode::WildcardSubscriptionsNotSupported,
];

#[test]
fn reason_code_编号往返() {
    assert_eq!(ALL_REASON_CODES.len(), 43);
    for code in ALL_REASON_CODES {
        let value = code.as_u8();
        assert_eq!(
            ReasonCode::try_from_u8(value),
            Some(*code),
            "原因码 0x{value:02X} 往返失败"
        );
    }
    // 规范中不存在的编号。
    for invalid in [
        0x03_u8, 0x05, 0x06, 0x07, 0x0A, 0x12, 0x1A, 0x1B, 0x7F, 0xA3, 0xFF,
    ] {
        assert_eq!(
            ReasonCode::try_from_u8(invalid),
            None,
            "原因码 0x{invalid:02X} 不应存在"
        );
    }
    // is_error：>= 0x80。
    for code in ALL_REASON_CODES {
        assert_eq!(
            code.is_error(),
            code.as_u8() >= 0x80,
            "is_error 判断不符：0x{:02X}",
            code.as_u8()
        );
    }
    // Display 至少包含十六进制编号，便于日志定位。
    let display = format!("{}", ReasonCode::NotAuthorized);
    assert!(!display.is_empty());
    assert!(
        display.contains("0x87"),
        "原因码 Display 应包含十六进制编号，实际：{display}"
    );
}

/// 按规范逐报文类型给出允许的原因码集合；其余报文类型一律为空。
fn reason_codes_allowed_in(packet_type: PacketType) -> &'static [u8] {
    match packet_type {
        PacketType::Connack => &[
            0x00, 0x80, 0x81, 0x82, 0x83, 0x84, 0x85, 0x86, 0x87, 0x88, 0x89, 0x8A, 0x8C, 0x90,
            0x95, 0x97, 0x99, 0x9A, 0x9B, 0x9C, 0x9D, 0x9F,
        ],
        PacketType::Puback | PacketType::Pubrec => {
            &[0x00, 0x10, 0x80, 0x83, 0x87, 0x90, 0x91, 0x97, 0x99]
        }
        PacketType::Pubrel | PacketType::Pubcomp => &[0x00, 0x92],
        PacketType::Suback => &[
            0x00, 0x01, 0x02, 0x80, 0x83, 0x87, 0x8F, 0x91, 0x97, 0x9E, 0xA1, 0xA2,
        ],
        PacketType::Unsuback => &[0x00, 0x11, 0x80, 0x83, 0x87, 0x8F, 0x91],
        PacketType::Disconnect => &[
            0x00, 0x04, 0x80, 0x81, 0x82, 0x83, 0x87, 0x89, 0x8B, 0x8D, 0x8E, 0x8F, 0x90, 0x93,
            0x94, 0x95, 0x96, 0x97, 0x98, 0x99, 0x9A, 0x9B, 0x9C, 0x9D, 0x9E, 0x9F, 0xA0, 0xA1,
            0xA2,
        ],
        PacketType::Auth => &[0x00, 0x18, 0x19],
        _ => &[],
    }
}

#[test]
fn reason_code_allowed_in表驱动() {
    for packet_type in ALL_PACKET_TYPES {
        let allowed = reason_codes_allowed_in(*packet_type);
        for code in ALL_REASON_CODES {
            let expected = allowed.contains(&code.as_u8());
            assert_eq!(
                code.allowed_in(*packet_type),
                expected,
                "原因码 0x{:02X} 在 {packet_type} 中的 allowed_in 不符（期望 {expected}）",
                code.as_u8()
            );
        }
    }
    // 作为交叉验证：PUBACK 允许 NoMatchingSubscribers，但 SUBACK 不允许。
    assert!(ReasonCode::NoMatchingSubscribers.allowed_in(PacketType::Puback));
    assert!(!ReasonCode::NoMatchingSubscribers.allowed_in(PacketType::Suback));
    // SUBACK 的 0x00 同时表示 Granted QoS 0。
    assert!(ReasonCode::Success.allowed_in(PacketType::Suback));
    assert!(ReasonCode::GrantedQos1.allowed_in(PacketType::Suback));
    assert!(ReasonCode::GrantedQos2.allowed_in(PacketType::Suback));
}

// ===========================================================================
// 主题名 / 主题过滤器 / 订阅选项（public API 独立验收）
// ===========================================================================

#[test]
fn topic_name_合法与非法() {
    for valid in [
        "a",
        "a/b",
        "/",
        "a//b",
        "sport/tennis",
        "/finance",
        "中文/主题",
        "$SYS/broker",
    ] {
        let topic =
            TopicName::new(valid).unwrap_or_else(|err| panic!("主题名 {valid:?} 应合法：{err}"));
        assert_eq!(topic.as_str(), valid);
        assert_eq!(topic.into_string(), valid);
    }
    for invalid in ["", "+", "#", "a/+", "a/#", "+/a", "a+b", "a#b"] {
        assert!(
            TopicName::new(invalid).is_err(),
            "主题名 {invalid:?} 应非法"
        );
    }
    assert!(
        TopicName::new("a\u{0000}b").is_err(),
        "主题名含 U+0000 应非法"
    );
    assert!(
        TopicName::new("\u{0000}").is_err(),
        "主题名含 U+0000 应非法"
    );

    // 长度边界：UTF-8 字节长度 <= 65535。
    assert!(
        TopicName::new("a".repeat(65_535)).is_ok(),
        "65535 字节主题名应合法"
    );
    assert!(
        TopicName::new("a".repeat(65_536)).is_err(),
        "65536 字节主题名应非法"
    );
}

#[test]
fn topic_filter_通配符规则() {
    for valid in [
        "#",
        "+",
        "sport/#",
        "sport/+/player1",
        "+/+/+",
        "/finance",
        "a//b",
        "/",
        "a/b/c",
        "sport/tennis/#",
        "中文/+/主题",
    ] {
        assert!(
            TopicFilter::new(valid).is_ok(),
            "主题过滤器 {valid:?} 应合法"
        );
    }
    for invalid in [
        "", "sport#", "#/x", "a#/b", "a+", "a/+b", "a/b+", "+b", "b+", "a/++/b", "a/#/b",
    ] {
        assert!(
            TopicFilter::new(invalid).is_err(),
            "主题过滤器 {invalid:?} 应非法"
        );
    }
    assert!(
        TopicFilter::new("a\u{0000}b").is_err(),
        "主题过滤器含 U+0000 应非法"
    );
    assert!(
        TopicFilter::new("a".repeat(65_535)).is_ok(),
        "65535 字节过滤器应合法"
    );
    assert!(
        TopicFilter::new("a".repeat(65_536)).is_err(),
        "65536 字节过滤器应非法"
    );
}

#[test]
fn topic_filter_共享订阅规则() {
    let cases: &[(&str, bool, bool)] = &[
        // (过滤器, is_shared, has_wildcard)
        ("$share/group/topic", true, false),
        ("$share/group/a/+/b", true, true),
        ("$share/group/#", true, true),
        ("$share/g/x", true, false),
        ("a/b", false, false),
        ("a/+/b", false, true),
        ("#", false, true),
        ("a/$share/g/x", false, false),
    ];
    for &(filter, shared, wildcard) in cases {
        let parsed = TopicFilter::new(filter)
            .unwrap_or_else(|err| panic!("过滤器 {filter:?} 应合法：{err}"));
        assert_eq!(parsed.is_shared(), shared, "{filter:?} is_shared 不符");
        assert_eq!(
            parsed.has_wildcard(),
            wildcard,
            "{filter:?} has_wildcard 不符"
        );
        assert_eq!(parsed.as_str(), filter);
    }

    for invalid in [
        "$share/",           // 缺少 ShareName 与 filter
        "$share//x",         // ShareName 为空
        "$share/g",          // 缺少 filter
        "$share/g/",         // filter 为空
        "$share/g+/x",       // ShareName 含 '+'
        "$share/g#/x",       // ShareName 含 '#'
        "$share/g/$share/x", // 嵌套 $share
    ] {
        assert!(
            TopicFilter::new(invalid).is_err(),
            "共享订阅过滤器 {invalid:?} 应非法"
        );
    }
}

#[test]
fn subscription_options_比特布局与校验() {
    let cases: &[(QoS, bool, bool, u8, u8)] = &[
        // (qos, no_local, retain_as_published, retain_handling, 期望线上字节)
        (QoS::AtMostOnce, false, false, 0, 0x00),
        (QoS::AtLeastOnce, false, false, 0, 0x01),
        (QoS::ExactlyOnce, false, false, 0, 0x02),
        (QoS::AtMostOnce, true, false, 0, 0x04),
        (QoS::AtMostOnce, false, true, 0, 0x08),
        (QoS::AtMostOnce, false, false, 1, 0x10),
        (QoS::AtMostOnce, false, false, 2, 0x20),
        (QoS::AtLeastOnce, true, true, 2, 0x2D),
        (QoS::ExactlyOnce, false, true, 1, 0x1A),
    ];
    for &(qos, no_local, retain_as_published, retain_handling, expected) in cases {
        let options = SubscriptionOptions::new(qos, no_local, retain_as_published, retain_handling)
            .expect("合法订阅选项不应失败");
        assert_eq!(options.qos, qos);
        assert_eq!(options.no_local, no_local);
        assert_eq!(options.retain_as_published, retain_as_published);
        assert_eq!(options.retain_handling, retain_handling);
        assert_eq!(options.to_byte(), expected);
        // 线上字节反解必须得到同样的选项。
        assert_eq!(
            SubscriptionOptions::try_from(expected),
            Ok(options),
            "线上字节 0x{expected:02X} 反解不符"
        );
    }

    assert!(
        SubscriptionOptions::new(QoS::AtMostOnce, false, false, 3).is_err(),
        "retain_handling = 3 应非法"
    );
    assert!(
        SubscriptionOptions::new(QoS::AtMostOnce, false, false, 255).is_err(),
        "retain_handling = 255 应非法"
    );

    // TryFrom 校验：保留位（bit6/bit7）、QoS=3、retain_handling=3。
    for invalid in [0x03_u8, 0x30, 0x40, 0x80, 0xC0, 0x33] {
        assert!(
            SubscriptionOptions::try_from(invalid).is_err(),
            "订阅选项字节 0x{invalid:02X} 应非法"
        );
    }
}

#[test]
fn subscription_结构体() {
    let subscription = Subscription {
        filter: TopicFilter::new("a/+/b").expect("过滤器应合法"),
        options: SubscriptionOptions::new(QoS::AtLeastOnce, true, false, 2)
            .expect("订阅选项应合法"),
    };
    assert_eq!(subscription.filter.as_str(), "a/+/b");
    // QoS1 = 0x01，no_local = 0x04，retain_handling=2 = 0x20，合计 0x25。
    assert_eq!(subscription.options.to_byte(), 0x25);
    let cloned = subscription.clone();
    assert_eq!(cloned, subscription);
}

// ===========================================================================
// 错误类型（public API 独立验收）
// ===========================================================================

#[test]
fn decode_error_原因码映射与显示() {
    let cases: &[(DecodeError, Option<u8>)] = &[
        (DecodeError::Malformed("x"), Some(0x81)),
        (DecodeError::ProtocolError("x"), Some(0x82)),
        (DecodeError::UnsupportedProtocolVersion(4), Some(0x84)),
        (DecodeError::PacketTooLarge { size: 10, max: 5 }, Some(0x95)),
        (DecodeError::Incomplete { needed: 1 }, None),
        (DecodeError::AllocationFailed, None),
    ];
    for &(error, expected) in cases {
        assert_eq!(
            error.reason_code(),
            expected,
            "DecodeError::reason_code 不符：{error:?}"
        );
        assert!(
            !format!("{error}").is_empty(),
            "DecodeError Display 不应为空：{error:?}"
        );
    }
    // 实现 core::error::Error。
    fn assert_error<E: core::error::Error>() {}
    assert_error::<DecodeError>();
    assert_error::<EncodeError>();
}

#[test]
fn encode_error_显示() {
    let cases = [
        EncodeError::ValueOutOfRange {
            field: "字段",
            value: 300,
            max: 255,
        },
        EncodeError::StringTooLong {
            field: "UTF-8 字符串",
            len: 70_000,
        },
        EncodeError::PacketTooLarge { size: 300_000_000 },
        EncodeError::Invalid("非法"),
        EncodeError::AllocationFailed,
    ];
    for error in cases {
        assert!(
            !format!("{error}").is_empty(),
            "EncodeError Display 不应为空：{error:?}"
        );
    }
}

// ===========================================================================
// 属性 ID 编号（public API 独立验收）
// ===========================================================================

/// 27 个属性 ID 的编号与 try_from_u32 往返。
#[test]
fn property_id_编号往返() {
    let cases: &[(PropertyId, u32)] = &[
        (PropertyId::PayloadFormatIndicator, 0x01),
        (PropertyId::MessageExpiryInterval, 0x02),
        (PropertyId::ContentType, 0x03),
        (PropertyId::ResponseTopic, 0x08),
        (PropertyId::CorrelationData, 0x09),
        (PropertyId::SubscriptionIdentifier, 0x0B),
        (PropertyId::SessionExpiryInterval, 0x11),
        (PropertyId::AssignedClientIdentifier, 0x12),
        (PropertyId::ServerKeepAlive, 0x13),
        (PropertyId::AuthenticationMethod, 0x15),
        (PropertyId::AuthenticationData, 0x16),
        (PropertyId::RequestProblemInformation, 0x17),
        (PropertyId::WillDelayInterval, 0x18),
        (PropertyId::RequestResponseInformation, 0x19),
        (PropertyId::ResponseInformation, 0x1A),
        (PropertyId::ServerReference, 0x1C),
        (PropertyId::ReasonString, 0x1F),
        (PropertyId::ReceiveMaximum, 0x21),
        (PropertyId::TopicAliasMaximum, 0x22),
        (PropertyId::TopicAlias, 0x23),
        (PropertyId::MaximumQos, 0x24),
        (PropertyId::RetainAvailable, 0x25),
        (PropertyId::UserProperty, 0x26),
        (PropertyId::MaximumPacketSize, 0x27),
        (PropertyId::WildcardSubscriptionAvailable, 0x28),
        (PropertyId::SubscriptionIdentifierAvailable, 0x29),
        (PropertyId::SharedSubscriptionAvailable, 0x2A),
    ];
    assert_eq!(cases.len(), 27);
    for &(property, value) in cases {
        assert_eq!(property.as_u32(), value);
        assert_eq!(PropertyId::try_from_u32(value), Some(property));
    }
    for invalid in [
        0_u32, 0x04, 0x07, 0x0A, 0x0C, 0x10, 0x14, 0x1B, 0x1D, 0x20, 0x2B, 0x7F, 0x80, 0xFFFFFFFF,
    ] {
        assert_eq!(
            PropertyId::try_from_u32(invalid),
            None,
            "属性 ID 0x{invalid:X} 不应存在"
        );
    }
}

// ===========================================================================
// 黄金向量：15 种控制报文（字节含义逐条注释，均按 MQTT 5.0 规范推导）
// ===========================================================================

/// CONNECT 最小合法报文。
///
/// 固定头 10 | RL=0F；可变头：协议名 "MQTT"、协议版本 5、连接标志 0x02
/// （bit1 = Clean Start，其余全 0）、Keep Alive=60、属性长度 0；
/// 载荷：客户端 ID "c1"。
const GOLDEN_CONNECT_MIN: &[u8] = &[
    0x10, // CONNECT：类型 1 << 4，flags 0000
    0x0F, // Remaining Length = 15
    0x00, 0x04, b'M', b'Q', b'T', b'T', // 协议名 "MQTT"
    0x05, // 协议版本 5
    0x02, // 连接标志：Clean Start = 1
    0x00, 0x3C, // Keep Alive = 60
    0x00, // 属性长度 = 0
    0x00, 0x02, b'c', b'1', // 客户端 ID "c1"
];

/// CONNECT 完整报文：Will（QoS1 + Retain + WillDelayInterval=5）+
/// 用户名 "u" + 密码 "p" + SessionExpiryInterval=60。
const GOLDEN_CONNECT_FULL: &[u8] = &[
    0x10, // CONNECT：类型 1 << 4，flags 0000
    0x2B, // Remaining Length = 43
    0x00, 0x04, b'M', b'Q', b'T', b'T', // 协议名 "MQTT"
    0x05, // 协议版本 5
    0xEC, // 连接标志：用户名 0x80 | 密码 0x40 | Will Retain 0x20 | Will QoS1 0x08 | Will 0x04
    0x00, 0x1E, // Keep Alive = 30
    0x05, // 属性长度 = 5
    0x11, 0x00, 0x00, 0x00, 0x3C, // 0x11 SessionExpiryInterval = 60
    0x00, 0x03, b'd', b'e', b'v', // 客户端 ID "dev"
    0x05, // Will 属性长度 = 5
    0x18, 0x00, 0x00, 0x00, 0x05, // 0x18 WillDelayInterval = 5
    0x00, 0x03, b'w', b'/', b't', // Will 主题 "w/t"
    0x00, 0x03, b'b', b'y', b'e', // Will 载荷 "bye"（二进制）
    0x00, 0x01, b'u', // 用户名 "u"
    0x00, 0x01, b'p', // 密码 "p"（二进制）
];

/// CONNACK：session present=1、原因码 Success、ReceiveMaximum=10。
const GOLDEN_CONNACK: &[u8] = &[
    0x20, // CONNACK：类型 2 << 4，flags 0000
    0x06, // Remaining Length = 6
    0x01, // 连接确认标志：bit0 Session Present = 1
    0x00, // 原因码 Success
    0x03, // 属性长度 = 3
    0x21, 0x00, 0x0A, // 0x21 ReceiveMaximum = 10
];

/// PUBLISH：QoS1 + Retain、主题 "a/b"、包 ID 10、载荷 "hi"，
/// 属性 PayloadFormatIndicator=1 与 MessageExpiryInterval=60。
const GOLDEN_PUBLISH: &[u8] = &[
    0x33, // PUBLISH：类型 3 << 4 | flags 0011（DUP=0，QoS=1，RETAIN=1）
    0x11, // Remaining Length = 17
    0x00, 0x03, b'a', b'/', b'b', // 主题名 "a/b"
    0x00, 0x0A, // 包 ID = 10
    0x07, // 属性长度 = 7
    0x01, 0x01, // 0x01 PayloadFormatIndicator = 1
    0x02, 0x00, 0x00, 0x00, 0x3C, // 0x02 MessageExpiryInterval = 60
    b'h', b'i', // 载荷 "hi"
];

/// PUBACK：包 ID 0x1234、原因码 NoMatchingSubscribers(0x10)、
/// 属性 ReasonString="nf"。
const GOLDEN_PUBACK: &[u8] = &[
    0x40, // PUBACK：类型 4 << 4，flags 0000
    0x09, // Remaining Length = 9
    0x12, 0x34, // 包 ID = 0x1234
    0x10, // 原因码 No Matching Subscribers
    0x05, // 属性长度 = 5
    0x1F, 0x00, 0x02, b'n', b'f', // 0x1F ReasonString = "nf"
];

/// PUBREC 最短形式：RL=2，原因码缺省 Success、无属性。
const GOLDEN_PUBREC: &[u8] = &[
    0x50, // PUBREC：类型 5 << 4，flags 0000
    0x02, // Remaining Length = 2
    0x00, 0x0A, // 包 ID = 10
];

/// PUBREL 最短形式：固定头 flags 必须为 0b0010；RL=2。
const GOLDEN_PUBREL: &[u8] = &[
    0x62, // PUBREL：类型 6 << 4 | flags 0010
    0x02, // Remaining Length = 2
    0x00, 0x0A, // 包 ID = 10
];

/// PUBCOMP：RL=3，原因码 PacketIdentifierNotFound(0x92)、无属性。
const GOLDEN_PUBCOMP: &[u8] = &[
    0x70, // PUBCOMP：类型 7 << 4，flags 0000
    0x03, // Remaining Length = 3
    0x00, 0x0A, // 包 ID = 10
    0x92, // 原因码 Packet Identifier Not Found
];

/// SUBSCRIBE：固定头 flags 0b0010；包 ID 7、SubscriptionIdentifier=3、
/// 过滤器 "a/+/b"、订阅选项 QoS1。
const GOLDEN_SUBSCRIBE: &[u8] = &[
    0x82, // SUBSCRIBE：类型 8 << 4 | flags 0010
    0x0D, // Remaining Length = 13
    0x00, 0x07, // 包 ID = 7
    0x02, // 属性长度 = 2
    0x0B, 0x03, // 0x0B SubscriptionIdentifier = 3（VarInt）
    0x00, 0x05, b'a', b'/', b'+', b'/', b'b', // 主题过滤器 "a/+/b"
    0x01, // 订阅选项：QoS1，其余位 0
];

/// SUBACK：包 ID 7、无属性、四个原因码 [Success, GrantedQos1, GrantedQos2, 0x80]。
const GOLDEN_SUBACK: &[u8] = &[
    0x90, // SUBACK：类型 9 << 4，flags 0000
    0x07, // Remaining Length = 7
    0x00, 0x07, // 包 ID = 7
    0x00, // 属性长度 = 0
    0x00, // 原因码 Granted QoS 0 / Success
    0x01, // 原因码 Granted QoS 1
    0x02, // 原因码 Granted QoS 2
    0x80, // 原因码 Unspecified Error
];

/// UNSUBSCRIBE：固定头 flags 0b0010；包 ID 8、无属性、过滤器 "a/b" 与 "c"。
const GOLDEN_UNSUBSCRIBE: &[u8] = &[
    0xA2, // UNSUBSCRIBE：类型 10 << 4 | flags 0010
    0x0B, // Remaining Length = 11
    0x00, 0x08, // 包 ID = 8
    0x00, // 属性长度 = 0
    0x00, 0x03, b'a', b'/', b'b', // 主题过滤器 "a/b"
    0x00, 0x01, b'c', // 主题过滤器 "c"
];

/// UNSUBACK：包 ID 8、无属性、单个原因码 Success。
const GOLDEN_UNSUBACK: &[u8] = &[
    0xB0, // UNSUBACK：类型 11 << 4，flags 0000
    0x04, // Remaining Length = 4
    0x00, 0x08, // 包 ID = 8
    0x00, // 属性长度 = 0
    0x00, // 原因码 Success
];

/// PINGREQ：无报文体，Remaining Length 必须为 0。
const GOLDEN_PINGREQ: &[u8] = &[
    0xC0, // PINGREQ：类型 12 << 4，flags 0000
    0x00, // Remaining Length = 0
];

/// PINGRESP：无报文体，Remaining Length 必须为 0。
const GOLDEN_PINGRESP: &[u8] = &[
    0xD0, // PINGRESP：类型 13 << 4，flags 0000
    0x00, // Remaining Length = 0
];

/// DISCONNECT：reason=DisconnectWithWillMessage(0x04)、无属性（RL=1 只写原因码）。
const GOLDEN_DISCONNECT: &[u8] = &[
    0xE0, // DISCONNECT：类型 14 << 4，flags 0000
    0x01, // Remaining Length = 1
    0x04, // 原因码 Disconnect with Will Message
];

/// AUTH：reason=ContinueAuthentication(0x18)、无属性（RL=1 只写原因码）。
const GOLDEN_AUTH: &[u8] = &[
    0xF0, // AUTH：类型 15 << 4，flags 0000
    0x01, // Remaining Length = 1
    0x18, // 原因码 Continue Authentication
];

/// 15 种控制报文的（名称、黄金字节、报文类型）表。
const GOLDEN_PACKETS: &[(&str, &[u8], PacketType)] = &[
    ("CONNECT 最小合法", GOLDEN_CONNECT_MIN, PacketType::Connect),
    (
        "CONNECT 完整（Will/用户名/密码/属性）",
        GOLDEN_CONNECT_FULL,
        PacketType::Connect,
    ),
    (
        "CONNACK 带 ReceiveMaximum",
        GOLDEN_CONNACK,
        PacketType::Connack,
    ),
    (
        "PUBLISH QoS1 Retain 带属性",
        GOLDEN_PUBLISH,
        PacketType::Publish,
    ),
    (
        "PUBACK 带原因码与 ReasonString",
        GOLDEN_PUBACK,
        PacketType::Puback,
    ),
    ("PUBREC 最短形式", GOLDEN_PUBREC, PacketType::Pubrec),
    ("PUBREL 最短形式", GOLDEN_PUBREL, PacketType::Pubrel),
    ("PUBCOMP 带原因码", GOLDEN_PUBCOMP, PacketType::Pubcomp),
    (
        "SUBSCRIBE 带 SubscriptionIdentifier",
        GOLDEN_SUBSCRIBE,
        PacketType::Subscribe,
    ),
    ("SUBACK 四个原因码", GOLDEN_SUBACK, PacketType::Suback),
    (
        "UNSUBSCRIBE 两个过滤器",
        GOLDEN_UNSUBSCRIBE,
        PacketType::Unsubscribe,
    ),
    ("UNSUBACK 单个原因码", GOLDEN_UNSUBACK, PacketType::Unsuback),
    ("PINGREQ RL=0", GOLDEN_PINGREQ, PacketType::Pingreq),
    ("PINGRESP RL=0", GOLDEN_PINGRESP, PacketType::Pingresp),
    (
        "DISCONNECT 仅原因码",
        GOLDEN_DISCONNECT,
        PacketType::Disconnect,
    ),
    ("AUTH 仅原因码", GOLDEN_AUTH, PacketType::Auth),
];

/// 断言构造的报文编码为指定黄金字节，且 encode/encode_to/encoded_len 一致、
/// 解码结果与原结构相等。
fn assert_packet_roundtrip(packet: Packet, expected_bytes: &[u8]) {
    let encoded = packet.encode().expect("报文编码不应失败");
    assert_eq!(encoded, expected_bytes, "构造报文编码字节不符：{packet:?}");
    assert_eq!(
        packet.encoded_len().expect("encoded_len 不应失败"),
        expected_bytes.len(),
        "encoded_len 与实际编码长度不符：{packet:?}"
    );
    let mut via_encode_to = Vec::new();
    packet
        .encode_to(&mut via_encode_to)
        .expect("encode_to 不应失败");
    assert_eq!(
        via_encode_to, encoded,
        "encode_to 与 encode 结果不符：{packet:?}"
    );
    assert_eq!(
        decode_ok(expected_bytes),
        packet,
        "解码结果与构造结构不相等"
    );
    assert_eq!(
        decode_ok(&encoded),
        packet,
        "重编码再解码结果与构造结构不相等"
    );
}

#[test]
fn golden_十五种控制报文_解码后重编码字节一致() {
    assert_eq!(
        GOLDEN_PACKETS.len(),
        16,
        "黄金向量应覆盖 CONNECT 两组与其余各一组"
    );
    let mut covered: Vec<PacketType> = Vec::new();
    for (name, bytes, expected_type) in GOLDEN_PACKETS {
        assert_wire_roundtrip(bytes, *expected_type);
        if !covered.contains(expected_type) {
            covered.push(*expected_type);
        }
        assert!(!name.is_empty(), "黄金向量名称不应为空");
    }
    // 15 种报文类型全部出现（CONNECT 出现两次）。
    for packet_type in ALL_PACKET_TYPES {
        assert!(
            covered.contains(packet_type),
            "黄金向量缺少报文类型 {packet_type}"
        );
    }
}

#[test]
fn golden_十五种控制报文_构造后编码字节一致() {
    // CONNECT 最小合法。
    assert_packet_roundtrip(
        Packet::Connect(Connect {
            clean_start: true,
            keep_alive: 60,
            client_id: "c1".to_string(),
            will: None,
            username: None,
            password: None,
            properties: Properties::new(),
        }),
        GOLDEN_CONNECT_MIN,
    );

    // CONNECT 完整：Will + 用户名 + 密码 + 属性。
    let mut will_properties = Properties::new();
    will_properties.will_delay_interval = Some(5);
    let mut connect_properties = Properties::new();
    connect_properties.session_expiry_interval = Some(60);
    assert_packet_roundtrip(
        Packet::Connect(Connect {
            clean_start: false,
            keep_alive: 30,
            client_id: "dev".to_string(),
            will: Some(Will {
                topic: TopicName::new("w/t").expect("Will 主题应合法"),
                payload: b"bye".to_vec(),
                qos: QoS::AtLeastOnce,
                retain: true,
                properties: will_properties,
            }),
            username: Some("u".to_string()),
            password: Some(b"p".to_vec()),
            properties: connect_properties,
        }),
        GOLDEN_CONNECT_FULL,
    );

    // CONNACK。
    let mut connack_properties = Properties::new();
    connack_properties.receive_maximum = Some(10);
    assert_packet_roundtrip(
        Packet::Connack(Connack {
            session_present: true,
            reason_code: ReasonCode::Success,
            properties: connack_properties,
        }),
        GOLDEN_CONNACK,
    );

    // PUBLISH。
    let mut publish_properties = Properties::new();
    publish_properties.payload_format_indicator = Some(true);
    publish_properties.message_expiry_interval = Some(60);
    assert_packet_roundtrip(
        Packet::Publish(Publish {
            dup: false,
            qos: QoS::AtLeastOnce,
            retain: true,
            topic: Some(TopicName::new("a/b").expect("主题名应合法")),
            packet_id: Some(10),
            properties: publish_properties,
            payload: b"hi".to_vec(),
        }),
        GOLDEN_PUBLISH,
    );

    // PUBACK。
    let mut puback_properties = Properties::new();
    puback_properties.reason_string = Some("nf".to_string());
    assert_packet_roundtrip(
        Packet::Puback(Ack {
            packet_id: 0x1234,
            reason_code: ReasonCode::NoMatchingSubscribers,
            properties: puback_properties,
        }),
        GOLDEN_PUBACK,
    );

    // PUBREC / PUBREL / PUBCOMP 最短形式。
    let shortest_ack = |packet_id: u16| Ack {
        packet_id,
        reason_code: ReasonCode::Success,
        properties: Properties::new(),
    };
    assert_packet_roundtrip(Packet::Pubrec(shortest_ack(10)), GOLDEN_PUBREC);
    assert_packet_roundtrip(Packet::Pubrel(shortest_ack(10)), GOLDEN_PUBREL);
    assert_packet_roundtrip(
        Packet::Pubcomp(Ack {
            packet_id: 10,
            reason_code: ReasonCode::PacketIdentifierNotFound,
            properties: Properties::new(),
        }),
        GOLDEN_PUBCOMP,
    );

    // SUBSCRIBE。
    let mut subscribe_properties = Properties::new();
    subscribe_properties.subscription_identifier = vec![3];
    assert_packet_roundtrip(
        Packet::Subscribe(Subscribe {
            packet_id: 7,
            properties: subscribe_properties,
            subscriptions: vec![Subscription {
                filter: TopicFilter::new("a/+/b").expect("过滤器应合法"),
                options: SubscriptionOptions::new(QoS::AtLeastOnce, false, false, 0)
                    .expect("订阅选项应合法"),
            }],
        }),
        GOLDEN_SUBSCRIBE,
    );

    // SUBACK。
    assert_packet_roundtrip(
        Packet::Suback(Suback {
            packet_id: 7,
            properties: Properties::new(),
            reason_codes: vec![
                ReasonCode::Success,
                ReasonCode::GrantedQos1,
                ReasonCode::GrantedQos2,
                ReasonCode::UnspecifiedError,
            ],
        }),
        GOLDEN_SUBACK,
    );

    // UNSUBSCRIBE。
    assert_packet_roundtrip(
        Packet::Unsubscribe(Unsubscribe {
            packet_id: 8,
            properties: Properties::new(),
            filters: vec![
                TopicFilter::new("a/b").expect("过滤器应合法"),
                TopicFilter::new("c").expect("过滤器应合法"),
            ],
        }),
        GOLDEN_UNSUBSCRIBE,
    );

    // UNSUBACK。
    assert_packet_roundtrip(
        Packet::Unsuback(Unsuback {
            packet_id: 8,
            properties: Properties::new(),
            reason_codes: vec![ReasonCode::Success],
        }),
        GOLDEN_UNSUBACK,
    );

    // PINGREQ / PINGRESP。
    assert_packet_roundtrip(Packet::Pingreq, GOLDEN_PINGREQ);
    assert_packet_roundtrip(Packet::Pingresp, GOLDEN_PINGRESP);

    // DISCONNECT / AUTH。
    assert_packet_roundtrip(
        Packet::Disconnect(Disconnect {
            reason_code: ReasonCode::DisconnectWithWillMessage,
            properties: Properties::new(),
        }),
        GOLDEN_DISCONNECT,
    );
    assert_packet_roundtrip(
        Packet::Auth(Auth {
            reason_code: ReasonCode::ContinueAuthentication,
            properties: Properties::new(),
        }),
        GOLDEN_AUTH,
    );
}

// ===========================================================================
// 最短合法编码分支
// ===========================================================================

/// 在四种 ACK 报文（PUBACK/PUBREC/PUBREL/PUBCOMP）上构造同一组最短编码用例。
/// 返回每个报文类型的固定头首字节（PUBREL 的 flags 为 0b0010）。
const ACK_PACKET_TYPES: &[(PacketType, u8)] = &[
    (PacketType::Puback, 0x40),
    (PacketType::Pubrec, 0x50),
    (PacketType::Pubrel, 0x62),
    (PacketType::Pubcomp, 0x70),
];

fn build_ack(
    packet_type: PacketType,
    packet_id: u16,
    reason_code: ReasonCode,
    properties: Properties,
) -> Packet {
    let ack = Ack {
        packet_id,
        reason_code,
        properties,
    };
    match packet_type {
        PacketType::Puback => Packet::Puback(ack),
        PacketType::Pubrec => Packet::Pubrec(ack),
        PacketType::Pubrel => Packet::Pubrel(ack),
        PacketType::Pubcomp => Packet::Pubcomp(ack),
        other => panic!("不是 ACK 报文类型：{other}"),
    }
}

#[test]
fn ack系列最短编码分支_rl2_rl3_带属性() {
    for &(packet_type, first_byte) in ACK_PACKET_TYPES {
        // RL=2：原因码缺省 Success 且无属性，原因码与属性长度整体省略。
        let shortest: &[u8] = &[first_byte, 0x02, 0x00, 0x0A];
        assert_wire_roundtrip(shortest, packet_type);
        assert_packet_roundtrip(
            build_ack(packet_type, 10, ReasonCode::Success, Properties::new()),
            shortest,
        );

        // RL=3：原因码非 Success 且无属性，只写原因码字节。
        let reason = if matches!(packet_type, PacketType::Pubrel | PacketType::Pubcomp) {
            ReasonCode::PacketIdentifierNotFound // 0x92
        } else {
            ReasonCode::NoMatchingSubscribers // 0x10
        };
        let with_reason: &[u8] = &[first_byte, 0x03, 0x00, 0x0A, reason.as_u8()];
        assert_wire_roundtrip(with_reason, packet_type);
        assert_packet_roundtrip(
            build_ack(packet_type, 10, reason, Properties::new()),
            with_reason,
        );

        // 带属性：原因码 + 属性长度 + 属性内容（RL = 2 + 1 + 8 = 11）。
        let mut properties = Properties::new();
        properties
            .user_property
            .push(("k".to_string(), "v".to_string()));
        let with_properties: &[u8] = &[
            first_byte, 0x0B, // Remaining Length = 11
            0x00, 0x0A, // 包 ID = 10
            0x00, // 原因码 Success
            0x07, // 属性长度 = 7
            0x26, 0x00, 0x01, b'k', 0x00, 0x01, b'v', // 0x26 UserProperty = ("k", "v")
        ];
        assert_wire_roundtrip(with_properties, packet_type);
        assert_packet_roundtrip(
            build_ack(packet_type, 10, ReasonCode::Success, properties),
            with_properties,
        );
    }

    // RL 只允许 2/3/>=4：0 与 1 必须是 Malformed。
    for &(_, first_byte) in ACK_PACKET_TYPES {
        assert_malformed(&[first_byte, 0x00]);
        assert_malformed(&[first_byte, 0x01, 0x00]);
    }
}

#[test]
fn ack系列非最短编码解码后重编码为最短形式() {
    // RL=4：原因码 Success + 属性长度 0（合法但非最短）。
    let decoded = decode_ok(&[0x40, 0x04, 0x00, 0x0A, 0x00, 0x00]);
    assert_eq!(
        decoded,
        build_ack(
            PacketType::Puback,
            10,
            ReasonCode::Success,
            Properties::new()
        )
    );
    assert_eq!(
        decoded.encode().expect("编码不应失败"),
        [0x40, 0x02, 0x00, 0x0A]
    );
}

#[test]
fn disconnect_auth最短编码分支_rl0_rl1_带属性() {
    // RL=0：reason Success 且无属性，原因码与属性长度整体省略。
    for (packet, first_byte) in [
        (
            Packet::Disconnect(Disconnect {
                reason_code: ReasonCode::Success,
                properties: Properties::new(),
            }),
            0xE0_u8,
        ),
        (
            Packet::Auth(Auth {
                reason_code: ReasonCode::Success,
                properties: Properties::new(),
            }),
            0xF0_u8,
        ),
    ] {
        let bytes = [first_byte, 0x00];
        let packet_type = packet.packet_type();
        assert_wire_roundtrip(&bytes, packet_type);
        assert_packet_roundtrip(packet, &bytes);
    }

    // RL=1：原因码非 Success 且无属性，只写原因码字节。
    let disconnect_reason_only: &[u8] = &[0xE0, 0x01, 0x04];
    assert_wire_roundtrip(disconnect_reason_only, PacketType::Disconnect);
    assert_packet_roundtrip(
        Packet::Disconnect(Disconnect {
            reason_code: ReasonCode::DisconnectWithWillMessage,
            properties: Properties::new(),
        }),
        disconnect_reason_only,
    );

    let auth_reason_only: &[u8] = &[0xF0, 0x01, 0x18];
    assert_wire_roundtrip(auth_reason_only, PacketType::Auth);
    assert_packet_roundtrip(
        Packet::Auth(Auth {
            reason_code: ReasonCode::ContinueAuthentication,
            properties: Properties::new(),
        }),
        auth_reason_only,
    );

    // RL>=2：带属性时必须写出原因码 + 属性段。
    let disconnect_with_properties: &[u8] = &[
        0xE0, 0x07, // Remaining Length = 7
        0x00, // 原因码 Success
        0x05, // 属性长度 = 5
        0x11, 0x00, 0x00, 0x00, 0x3C, // 0x11 SessionExpiryInterval = 60
    ];
    let mut disconnect_properties = Properties::new();
    disconnect_properties.session_expiry_interval = Some(60);
    assert_wire_roundtrip(disconnect_with_properties, PacketType::Disconnect);
    assert_packet_roundtrip(
        Packet::Disconnect(Disconnect {
            reason_code: ReasonCode::Success,
            properties: disconnect_properties,
        }),
        disconnect_with_properties,
    );

    let auth_with_properties: &[u8] = &[
        0xF0, 0x06, // Remaining Length = 6
        0x18, // 原因码 Continue Authentication
        0x04, // 属性长度 = 4
        0x15, 0x00, 0x01, b'x', // 0x15 AuthenticationMethod = "x"
    ];
    let mut auth_properties = Properties::new();
    auth_properties.authentication_method = Some("x".to_string());
    assert_wire_roundtrip(auth_with_properties, PacketType::Auth);
    assert_packet_roundtrip(
        Packet::Auth(Auth {
            reason_code: ReasonCode::ContinueAuthentication,
            properties: auth_properties,
        }),
        auth_with_properties,
    );

    // 非最短但合法：RL=1 且原因码 Success；解码后应重编码为 RL=0。
    let decoded = decode_ok(&[0xE0, 0x01, 0x00]);
    assert_eq!(
        decoded,
        Packet::Disconnect(Disconnect {
            reason_code: ReasonCode::Success,
            properties: Properties::new(),
        })
    );
    assert_eq!(decoded.encode().expect("编码不应失败"), [0xE0, 0x00]);

    let decoded = decode_ok(&[0xF0, 0x01, 0x00]);
    assert_eq!(
        decoded,
        Packet::Auth(Auth {
            reason_code: ReasonCode::Success,
            properties: Properties::new(),
        })
    );
    assert_eq!(decoded.encode().expect("编码不应失败"), [0xF0, 0x00]);

    // 非最短但合法：RL=2 且原因码 Success + 属性长度 0；解码后应重编码为 RL=0。
    let decoded = decode_ok(&[0xE0, 0x02, 0x00, 0x00]);
    assert_eq!(decoded.encode().expect("编码不应失败"), [0xE0, 0x00]);
}

#[test]
fn pingreq_pingresp_rl必须为0() {
    assert_wire_roundtrip(GOLDEN_PINGREQ, PacketType::Pingreq);
    assert_wire_roundtrip(GOLDEN_PINGRESP, PacketType::Pingresp);
    // PINGREQ/PINGRESP 不允许有任何报文体。
    assert_malformed(&[0xC0, 0x01, 0x00]);
    assert_malformed(&[0xD0, 0x01, 0x00]);
    assert_malformed(&[0xC0, 0x80, 0x00]);
}

#[test]
fn connack_最短编码与多字节remaining_length() {
    // CONNACK 最小：ack flags + 原因码 + 属性长度 = 3 字节。
    let shortest: &[u8] = &[0x20, 0x03, 0x00, 0x00, 0x00];
    assert_wire_roundtrip(shortest, PacketType::Connack);

    // 多字节 Remaining Length 的 PUBLISH：主题 "big"、空属性、200 字节载荷。
    let packet = Packet::Publish(Publish {
        dup: false,
        qos: QoS::AtMostOnce,
        retain: false,
        topic: Some(TopicName::new("big").expect("主题名应合法")),
        packet_id: None,
        properties: Properties::new(),
        payload: vec![0xAA; 200],
    });
    let encoded = packet.encode().expect("编码不应失败");
    // body = 2(主题长度) + 3(主题) + 1(属性长度) + 200(载荷) = 206 = 0xCE 0x01。
    assert_eq!(
        &encoded[..3],
        &[0x30, 0xCE, 0x01],
        "多字节 Remaining Length 编码不符"
    );
    assert_eq!(
        packet.encoded_len().expect("encoded_len 不应失败"),
        encoded.len()
    );
    assert_eq!(decode_ok(&encoded), packet);
    let (header, header_len) = FixedHeader::decode(&encoded).expect("固定头应可解码");
    assert_eq!(header_len, 3);
    assert_eq!(header.remaining_length, 206);
}

// ===========================================================================
// 属性（Properties）：27 个属性 ID 的字节级与构造级往返
//
// 属性段的线上形式（规范 §2.2.2）：属性长度 VarInt + (属性 ID VarInt + 属性值)...
// 实现内部 §Properties::decode/encode_into§ 是 pub(crate)，集成测试只能通过报文
// 间接验证——这正是本节的独立 oracle 方式：手工拼出包含目标属性的报文，
// 断言 decode → 属性正确 → encode 字节一致；再用同一份 Properties 构造成报文，
// 断言编码结果等于手工字节。
// ===========================================================================

/// 编码 VarInt（属性长度、SubscriptionIdentifier 值等）。
fn encode_varint(value: u32) -> Vec<u8> {
    let mut out = Vec::new();
    varint::encode(value, &mut out).expect("测试数据的 VarInt 编码不应失败");
    out
}

/// 编码带 u16 长度前缀的 UTF-8 字符串。
fn encode_utf8(value: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(2 + value.len());
    out.extend_from_slice(&(value.len() as u16).to_be_bytes());
    out.extend_from_slice(value.as_bytes());
    out
}

/// 编码带 u16 长度前缀的二进制数据。
fn encode_binary(value: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(2 + value.len());
    out.extend_from_slice(&(value.len() as u16).to_be_bytes());
    out.extend_from_slice(value);
    out
}

/// 拼接属性段：属性长度 VarInt + 属性内容。
fn property_section(content: &[u8]) -> Vec<u8> {
    let mut out = encode_varint(content.len() as u32);
    out.extend_from_slice(content);
    out
}

/// 拼接完整报文：固定头首字节 + Remaining Length VarInt + 报文体。
fn assemble_packet(first_byte: u8, body: &[u8]) -> Vec<u8> {
    let mut out = vec![first_byte];
    out.extend_from_slice(&encode_varint(body.len() as u32));
    out.extend_from_slice(body);
    out
}

/// 拼接多个属性条目。
fn cat(parts: &[Vec<u8>]) -> Vec<u8> {
    let mut out = Vec::new();
    for part in parts {
        out.extend_from_slice(part);
    }
    out
}

// ---- 单个属性条目的线上字节 ----

fn prop_u8(id: u8, value: u8) -> Vec<u8> {
    vec![id, value]
}

fn prop_u16(id: u8, value: u16) -> Vec<u8> {
    let mut out = vec![id];
    out.extend_from_slice(&value.to_be_bytes());
    out
}

fn prop_u32(id: u8, value: u32) -> Vec<u8> {
    let mut out = vec![id];
    out.extend_from_slice(&value.to_be_bytes());
    out
}

fn prop_str(id: u8, value: &str) -> Vec<u8> {
    let mut out = vec![id];
    out.extend_from_slice(&encode_utf8(value));
    out
}

fn prop_binary(id: u8, value: &[u8]) -> Vec<u8> {
    let mut out = vec![id];
    out.extend_from_slice(&encode_binary(value));
    out
}

fn prop_varint_value(id: u8, value: u32) -> Vec<u8> {
    let mut out = vec![id];
    out.extend_from_slice(&encode_varint(value));
    out
}

fn prop_user_property(key: &str, value: &str) -> Vec<u8> {
    let mut out = vec![0x26];
    out.extend_from_slice(&encode_utf8(key));
    out.extend_from_slice(&encode_utf8(value));
    out
}

// ---- 各报文类型的报文体拼装（固定字段取最小合法值） ----

fn connect_bytes(property_content: &[u8]) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&encode_utf8("MQTT")); // 协议名
    body.push(0x05); // 协议版本
    body.push(0x02); // 连接标志：Clean Start
    body.extend_from_slice(&[0x00, 0x3C]); // Keep Alive = 60
    body.extend_from_slice(&property_section(property_content));
    body.extend_from_slice(&encode_utf8("c")); // 客户端 ID
    assemble_packet(0x10, &body)
}

fn connect_with_will_bytes(will_property_content: &[u8]) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&encode_utf8("MQTT"));
    body.push(0x05);
    body.push(0x06); // 连接标志：Clean Start + Will Flag（Will QoS0、无 Retain）
    body.extend_from_slice(&[0x00, 0x3C]);
    body.extend_from_slice(&property_section(&[])); // CONNECT 属性为空
    body.extend_from_slice(&encode_utf8("c")); // 客户端 ID
    body.extend_from_slice(&property_section(will_property_content)); // Will 属性
    body.extend_from_slice(&encode_utf8("w")); // Will 主题
    body.extend_from_slice(&encode_binary(b"p")); // Will 载荷
    assemble_packet(0x10, &body)
}

fn connack_bytes(property_content: &[u8]) -> Vec<u8> {
    let mut body = vec![0x00, 0x00]; // Session Present = 0，原因码 Success
    body.extend_from_slice(&property_section(property_content));
    assemble_packet(0x20, &body)
}

fn publish_bytes(property_content: &[u8]) -> Vec<u8> {
    let mut body = encode_utf8("t"); // 主题名（QoS0 无包 ID）
    body.extend_from_slice(&property_section(property_content));
    body.extend_from_slice(b"x"); // 载荷
    assemble_packet(0x30, &body)
}

fn ack_bytes(first_byte: u8, property_content: &[u8]) -> Vec<u8> {
    let mut body = vec![0x00, 0x0A, 0x00]; // 包 ID = 10，原因码 Success
    body.extend_from_slice(&property_section(property_content));
    assemble_packet(first_byte, &body)
}

fn subscribe_bytes(property_content: &[u8]) -> Vec<u8> {
    let mut body = vec![0x00, 0x0A]; // 包 ID = 10
    body.extend_from_slice(&property_section(property_content));
    body.extend_from_slice(&encode_utf8("a")); // 过滤器 "a"
    body.push(0x00); // 订阅选项 QoS0
    assemble_packet(0x82, &body)
}

fn suback_bytes(property_content: &[u8]) -> Vec<u8> {
    let mut body = vec![0x00, 0x0A]; // 包 ID = 10
    body.extend_from_slice(&property_section(property_content));
    body.push(0x00); // 原因码 Success
    assemble_packet(0x90, &body)
}

fn unsubscribe_bytes(property_content: &[u8]) -> Vec<u8> {
    let mut body = vec![0x00, 0x0A];
    body.extend_from_slice(&property_section(property_content));
    body.extend_from_slice(&encode_utf8("a"));
    assemble_packet(0xA2, &body)
}

fn unsuback_bytes(property_content: &[u8]) -> Vec<u8> {
    let mut body = vec![0x00, 0x0A];
    body.extend_from_slice(&property_section(property_content));
    body.push(0x00); // 原因码 Success
    assemble_packet(0xB0, &body)
}

fn disconnect_bytes(property_content: &[u8]) -> Vec<u8> {
    let mut body = vec![0x00]; // 原因码 Success
    body.extend_from_slice(&property_section(property_content));
    assemble_packet(0xE0, &body)
}

fn auth_bytes(property_content: &[u8]) -> Vec<u8> {
    let mut body = vec![0x18]; // 原因码 Continue Authentication
    body.extend_from_slice(&property_section(property_content));
    assemble_packet(0xF0, &body)
}

// ---- 与手工字节一一对应的构造器（构造方向验证） ----

fn build_connect(properties: Properties) -> Packet {
    Packet::Connect(Connect {
        clean_start: true,
        keep_alive: 60,
        client_id: "c".to_string(),
        will: None,
        username: None,
        password: None,
        properties,
    })
}

fn build_connect_with_will(properties: Properties) -> Packet {
    Packet::Connect(Connect {
        clean_start: true,
        keep_alive: 60,
        client_id: "c".to_string(),
        will: Some(Will {
            topic: TopicName::new("w").expect("Will 主题应合法"),
            payload: b"p".to_vec(),
            qos: QoS::AtMostOnce,
            retain: false,
            properties,
        }),
        username: None,
        password: None,
        properties: Properties::new(),
    })
}

fn build_connack(properties: Properties) -> Packet {
    Packet::Connack(Connack {
        session_present: false,
        reason_code: ReasonCode::Success,
        properties,
    })
}

fn build_publish(properties: Properties) -> Packet {
    Packet::Publish(Publish {
        dup: false,
        qos: QoS::AtMostOnce,
        retain: false,
        topic: Some(TopicName::new("t").expect("主题名应合法")),
        packet_id: None,
        properties,
        payload: b"x".to_vec(),
    })
}

fn build_puback(properties: Properties) -> Packet {
    Packet::Puback(Ack {
        packet_id: 10,
        reason_code: ReasonCode::Success,
        properties,
    })
}

fn build_subscribe(properties: Properties) -> Packet {
    Packet::Subscribe(Subscribe {
        packet_id: 10,
        properties,
        subscriptions: vec![Subscription {
            filter: TopicFilter::new("a").expect("过滤器应合法"),
            options: SubscriptionOptions::new(QoS::AtMostOnce, false, false, 0)
                .expect("订阅选项应合法"),
        }],
    })
}

fn build_unsubscribe(properties: Properties) -> Packet {
    Packet::Unsubscribe(Unsubscribe {
        packet_id: 10,
        properties,
        filters: vec![TopicFilter::new("a").expect("过滤器应合法")],
    })
}

fn build_disconnect(properties: Properties) -> Packet {
    Packet::Disconnect(Disconnect {
        reason_code: ReasonCode::Success,
        properties,
    })
}

fn build_auth(properties: Properties) -> Packet {
    Packet::Auth(Auth {
        reason_code: ReasonCode::ContinueAuthentication,
        properties,
    })
}

type PacketBuilder = fn(Properties) -> Packet;

/// 单个属性的字节级用例。
struct PropertyWireCase {
    /// 用例名称。
    name: &'static str,
    /// 报文类型。
    packet_type: PacketType,
    /// 属性出现场景。
    context: PropertyContext,
    /// 手工拼出的完整报文黄金字节。
    bytes: Vec<u8>,
    /// 期望解析出的属性。
    expected: Properties,
    /// 使用该属性构造报文的构造器。
    build: PacketBuilder,
}

/// 提取报文中指定场景的属性段。
fn packet_properties(packet: &Packet, context: PropertyContext) -> Option<&Properties> {
    match (packet, context) {
        (Packet::Connect(connect), PropertyContext::Connect) => Some(&connect.properties),
        (Packet::Connect(connect), PropertyContext::Will) => {
            connect.will.as_ref().map(|will| &will.properties)
        }
        (Packet::Connack(connack), PropertyContext::Connack) => Some(&connack.properties),
        (Packet::Publish(publish), PropertyContext::Publish) => Some(&publish.properties),
        (Packet::Puback(ack), PropertyContext::Puback)
        | (Packet::Pubrec(ack), PropertyContext::Pubrec)
        | (Packet::Pubrel(ack), PropertyContext::Pubrel)
        | (Packet::Pubcomp(ack), PropertyContext::Pubcomp) => Some(&ack.properties),
        (Packet::Subscribe(subscribe), PropertyContext::Subscribe) => Some(&subscribe.properties),
        (Packet::Suback(suback), PropertyContext::Suback) => Some(&suback.properties),
        (Packet::Unsubscribe(unsubscribe), PropertyContext::Unsubscribe) => {
            Some(&unsubscribe.properties)
        }
        (Packet::Unsuback(unsuback), PropertyContext::Unsuback) => Some(&unsuback.properties),
        (Packet::Disconnect(disconnect), PropertyContext::Disconnect) => {
            Some(&disconnect.properties)
        }
        (Packet::Auth(auth), PropertyContext::Auth) => Some(&auth.properties),
        _ => None,
    }
}

/// 27 个属性 ID 的完整清单。
const ALL_PROPERTY_IDS: &[PropertyId] = &[
    PropertyId::PayloadFormatIndicator,
    PropertyId::MessageExpiryInterval,
    PropertyId::ContentType,
    PropertyId::ResponseTopic,
    PropertyId::CorrelationData,
    PropertyId::SubscriptionIdentifier,
    PropertyId::SessionExpiryInterval,
    PropertyId::AssignedClientIdentifier,
    PropertyId::ServerKeepAlive,
    PropertyId::AuthenticationMethod,
    PropertyId::AuthenticationData,
    PropertyId::RequestProblemInformation,
    PropertyId::WillDelayInterval,
    PropertyId::RequestResponseInformation,
    PropertyId::ResponseInformation,
    PropertyId::ServerReference,
    PropertyId::ReasonString,
    PropertyId::ReceiveMaximum,
    PropertyId::TopicAliasMaximum,
    PropertyId::TopicAlias,
    PropertyId::MaximumQos,
    PropertyId::RetainAvailable,
    PropertyId::UserProperty,
    PropertyId::MaximumPacketSize,
    PropertyId::WildcardSubscriptionAvailable,
    PropertyId::SubscriptionIdentifierAvailable,
    PropertyId::SharedSubscriptionAvailable,
];

/// 属性是否出现在解出的 §Properties§ 中。
fn has_property(properties: &Properties, id: PropertyId) -> bool {
    match id {
        PropertyId::PayloadFormatIndicator => properties.payload_format_indicator.is_some(),
        PropertyId::MessageExpiryInterval => properties.message_expiry_interval.is_some(),
        PropertyId::ContentType => properties.content_type.is_some(),
        PropertyId::ResponseTopic => properties.response_topic.is_some(),
        PropertyId::CorrelationData => properties.correlation_data.is_some(),
        PropertyId::SubscriptionIdentifier => !properties.subscription_identifier.is_empty(),
        PropertyId::SessionExpiryInterval => properties.session_expiry_interval.is_some(),
        PropertyId::AssignedClientIdentifier => properties.assigned_client_identifier.is_some(),
        PropertyId::ServerKeepAlive => properties.server_keep_alive.is_some(),
        PropertyId::AuthenticationMethod => properties.authentication_method.is_some(),
        PropertyId::AuthenticationData => properties.authentication_data.is_some(),
        PropertyId::RequestProblemInformation => properties.request_problem_information.is_some(),
        PropertyId::WillDelayInterval => properties.will_delay_interval.is_some(),
        PropertyId::RequestResponseInformation => properties.request_response_information.is_some(),
        PropertyId::ResponseInformation => properties.response_information.is_some(),
        PropertyId::ServerReference => properties.server_reference.is_some(),
        PropertyId::ReasonString => properties.reason_string.is_some(),
        PropertyId::ReceiveMaximum => properties.receive_maximum.is_some(),
        PropertyId::TopicAliasMaximum => properties.topic_alias_maximum.is_some(),
        PropertyId::TopicAlias => properties.topic_alias.is_some(),
        PropertyId::MaximumQos => properties.maximum_qos.is_some(),
        PropertyId::RetainAvailable => properties.retain_available.is_some(),
        PropertyId::UserProperty => !properties.user_property.is_empty(),
        PropertyId::MaximumPacketSize => properties.maximum_packet_size.is_some(),
        PropertyId::WildcardSubscriptionAvailable => {
            properties.wildcard_subscription_available.is_some()
        }
        PropertyId::SubscriptionIdentifierAvailable => {
            properties.subscription_identifier_available.is_some()
        }
        PropertyId::SharedSubscriptionAvailable => {
            properties.shared_subscription_available.is_some()
        }
    }
}

/// 构造全部属性用例：覆盖 27 个属性 ID，并覆盖可重复属性与多字节属性长度。
fn property_wire_cases() -> Vec<PropertyWireCase> {
    let mut cases: Vec<PropertyWireCase> = Vec::new();
    {
        let mut add = |name: &'static str,
                       packet_type: PacketType,
                       context: PropertyContext,
                       bytes: Vec<u8>,
                       expected: Properties,
                       build: PacketBuilder| {
            cases.push(PropertyWireCase {
                name,
                packet_type,
                context,
                bytes,
                expected,
                build,
            });
        };

        // ---- PUBLISH 上下文：0x01/0x02/0x03/0x08/0x09/0x0B/0x23/0x26 ----
        let mut p = Properties::new();
        p.payload_format_indicator = Some(true);
        add(
            "PUBLISH 0x01 PayloadFormatIndicator=1",
            PacketType::Publish,
            PropertyContext::Publish,
            publish_bytes(&prop_u8(0x01, 0x01)),
            p,
            build_publish,
        );

        let mut p = Properties::new();
        p.message_expiry_interval = Some(60);
        add(
            "PUBLISH 0x02 MessageExpiryInterval=60",
            PacketType::Publish,
            PropertyContext::Publish,
            publish_bytes(&prop_u32(0x02, 60)),
            p,
            build_publish,
        );

        let mut p = Properties::new();
        p.content_type = Some("text/plain".to_string());
        add(
            "PUBLISH 0x03 ContentType=text/plain",
            PacketType::Publish,
            PropertyContext::Publish,
            publish_bytes(&prop_str(0x03, "text/plain")),
            p,
            build_publish,
        );

        let mut p = Properties::new();
        p.response_topic = Some(TopicName::new("resp/topic").expect("响应主题应合法"));
        add(
            "PUBLISH 0x08 ResponseTopic=resp/topic",
            PacketType::Publish,
            PropertyContext::Publish,
            publish_bytes(&prop_str(0x08, "resp/topic")),
            p,
            build_publish,
        );

        let mut p = Properties::new();
        p.correlation_data = Some(vec![0xDE, 0xAD]);
        add(
            "PUBLISH 0x09 CorrelationData=[DE AD]",
            PacketType::Publish,
            PropertyContext::Publish,
            publish_bytes(&prop_binary(0x09, &[0xDE, 0xAD])),
            p,
            build_publish,
        );

        let mut p = Properties::new();
        p.subscription_identifier = vec![5];
        add(
            "PUBLISH 0x0B SubscriptionIdentifier=5",
            PacketType::Publish,
            PropertyContext::Publish,
            publish_bytes(&prop_varint_value(0x0B, 5)),
            p,
            build_publish,
        );

        // PUBLISH 中 SubscriptionIdentifier 允许重复出现。
        let mut p = Properties::new();
        p.subscription_identifier = vec![1, 2];
        add(
            "PUBLISH 0x0B SubscriptionIdentifier 重复（1、2）",
            PacketType::Publish,
            PropertyContext::Publish,
            publish_bytes(&cat(&[
                prop_varint_value(0x0B, 1),
                prop_varint_value(0x0B, 2),
            ])),
            p,
            build_publish,
        );

        let mut p = Properties::new();
        p.topic_alias = Some(1);
        add(
            "PUBLISH 0x23 TopicAlias=1",
            PacketType::Publish,
            PropertyContext::Publish,
            publish_bytes(&prop_u16(0x23, 1)),
            p,
            build_publish,
        );

        // PUBLISH 中 UserProperty 允许重复出现。
        let mut p = Properties::new();
        p.user_property.push(("k1".to_string(), "v1".to_string()));
        p.user_property.push(("k2".to_string(), "v2".to_string()));
        add(
            "PUBLISH 0x26 UserProperty 重复（k1/v1、k2/v2）",
            PacketType::Publish,
            PropertyContext::Publish,
            publish_bytes(&cat(&[
                prop_user_property("k1", "v1"),
                prop_user_property("k2", "v2"),
            ])),
            p,
            build_publish,
        );

        // 属性段长度超过 127，属性长度 VarInt 必须用两字节编码。
        let long_key = "k".repeat(200);
        let long_value = "v".repeat(200);
        let mut p = Properties::new();
        p.user_property.push((long_key.clone(), long_value.clone()));
        add(
            "PUBLISH 0x26 UserProperty 超长（属性长度两字节 VarInt）",
            PacketType::Publish,
            PropertyContext::Publish,
            publish_bytes(&prop_user_property(&long_key, &long_value)),
            p,
            build_publish,
        );

        // ---- CONNECT 上下文：0x11/0x21/0x27/0x22/0x19/0x17/0x26/0x15/0x16 ----
        let mut p = Properties::new();
        p.session_expiry_interval = Some(60);
        add(
            "CONNECT 0x11 SessionExpiryInterval=60",
            PacketType::Connect,
            PropertyContext::Connect,
            connect_bytes(&prop_u32(0x11, 60)),
            p,
            build_connect,
        );

        let mut p = Properties::new();
        p.receive_maximum = Some(10);
        add(
            "CONNECT 0x21 ReceiveMaximum=10",
            PacketType::Connect,
            PropertyContext::Connect,
            connect_bytes(&prop_u16(0x21, 10)),
            p,
            build_connect,
        );

        let mut p = Properties::new();
        p.maximum_packet_size = Some(65_536);
        add(
            "CONNECT 0x27 MaximumPacketSize=65536",
            PacketType::Connect,
            PropertyContext::Connect,
            connect_bytes(&prop_u32(0x27, 65_536)),
            p,
            build_connect,
        );

        let mut p = Properties::new();
        p.topic_alias_maximum = Some(5);
        add(
            "CONNECT 0x22 TopicAliasMaximum=5",
            PacketType::Connect,
            PropertyContext::Connect,
            connect_bytes(&prop_u16(0x22, 5)),
            p,
            build_connect,
        );

        let mut p = Properties::new();
        p.request_response_information = Some(true);
        add(
            "CONNECT 0x19 RequestResponseInformation=1",
            PacketType::Connect,
            PropertyContext::Connect,
            connect_bytes(&prop_u8(0x19, 0x01)),
            p,
            build_connect,
        );

        let mut p = Properties::new();
        p.request_problem_information = Some(false);
        add(
            "CONNECT 0x17 RequestProblemInformation=0",
            PacketType::Connect,
            PropertyContext::Connect,
            connect_bytes(&prop_u8(0x17, 0x00)),
            p,
            build_connect,
        );

        let mut p = Properties::new();
        p.user_property.push(("uk".to_string(), "uv".to_string()));
        add(
            "CONNECT 0x26 UserProperty（uk/uv）",
            PacketType::Connect,
            PropertyContext::Connect,
            connect_bytes(&prop_user_property("uk", "uv")),
            p,
            build_connect,
        );

        let mut p = Properties::new();
        p.authentication_method = Some("method".to_string());
        add(
            "CONNECT 0x15 AuthenticationMethod=method",
            PacketType::Connect,
            PropertyContext::Connect,
            connect_bytes(&prop_str(0x15, "method")),
            p,
            build_connect,
        );

        let mut p = Properties::new();
        p.authentication_method = Some("m".to_string());
        p.authentication_data = Some(vec![1, 2, 3]);
        add(
            "CONNECT 0x15+0x16 AuthenticationMethod=m + Data=[01 02 03]",
            PacketType::Connect,
            PropertyContext::Connect,
            connect_bytes(&cat(&[prop_str(0x15, "m"), prop_binary(0x16, &[1, 2, 3])])),
            p,
            build_connect,
        );

        // ---- Will 上下文：0x18/0x01/0x02/0x03/0x08/0x09/0x26 ----
        let mut p = Properties::new();
        p.will_delay_interval = Some(5);
        add(
            "Will 0x18 WillDelayInterval=5",
            PacketType::Connect,
            PropertyContext::Will,
            connect_with_will_bytes(&prop_u32(0x18, 5)),
            p,
            build_connect_with_will,
        );

        let mut p = Properties::new();
        p.payload_format_indicator = Some(false);
        add(
            "Will 0x01 PayloadFormatIndicator=0",
            PacketType::Connect,
            PropertyContext::Will,
            connect_with_will_bytes(&prop_u8(0x01, 0x00)),
            p,
            build_connect_with_will,
        );

        let mut p = Properties::new();
        p.message_expiry_interval = Some(10);
        add(
            "Will 0x02 MessageExpiryInterval=10",
            PacketType::Connect,
            PropertyContext::Will,
            connect_with_will_bytes(&prop_u32(0x02, 10)),
            p,
            build_connect_with_will,
        );

        let mut p = Properties::new();
        p.content_type = Some("json".to_string());
        add(
            "Will 0x03 ContentType=json",
            PacketType::Connect,
            PropertyContext::Will,
            connect_with_will_bytes(&prop_str(0x03, "json")),
            p,
            build_connect_with_will,
        );

        let mut p = Properties::new();
        p.response_topic = Some(TopicName::new("wr").expect("响应主题应合法"));
        add(
            "Will 0x08 ResponseTopic=wr",
            PacketType::Connect,
            PropertyContext::Will,
            connect_with_will_bytes(&prop_str(0x08, "wr")),
            p,
            build_connect_with_will,
        );

        let mut p = Properties::new();
        p.correlation_data = Some(vec![9]);
        add(
            "Will 0x09 CorrelationData=[09]",
            PacketType::Connect,
            PropertyContext::Will,
            connect_with_will_bytes(&prop_binary(0x09, &[9])),
            p,
            build_connect_with_will,
        );

        let mut p = Properties::new();
        p.user_property.push(("wk".to_string(), "wv".to_string()));
        add(
            "Will 0x26 UserProperty（wk/wv）",
            PacketType::Connect,
            PropertyContext::Will,
            connect_with_will_bytes(&prop_user_property("wk", "wv")),
            p,
            build_connect_with_will,
        );

        // ---- CONNACK 上下文：0x12/0x13/0x1A/0x1C/0x1F/0x21/0x24/0x25/0x28/0x29/0x2A ----
        let mut p = Properties::new();
        p.assigned_client_identifier = Some("assigned".to_string());
        add(
            "CONNACK 0x12 AssignedClientIdentifier=assigned",
            PacketType::Connack,
            PropertyContext::Connack,
            connack_bytes(&prop_str(0x12, "assigned")),
            p,
            build_connack,
        );

        let mut p = Properties::new();
        p.server_keep_alive = Some(30);
        add(
            "CONNACK 0x13 ServerKeepAlive=30",
            PacketType::Connack,
            PropertyContext::Connack,
            connack_bytes(&prop_u16(0x13, 30)),
            p,
            build_connack,
        );

        let mut p = Properties::new();
        p.response_information = Some("ri".to_string());
        add(
            "CONNACK 0x1A ResponseInformation=ri",
            PacketType::Connack,
            PropertyContext::Connack,
            connack_bytes(&prop_str(0x1A, "ri")),
            p,
            build_connack,
        );

        let mut p = Properties::new();
        p.server_reference = Some("srv".to_string());
        add(
            "CONNACK 0x1C ServerReference=srv",
            PacketType::Connack,
            PropertyContext::Connack,
            connack_bytes(&prop_str(0x1C, "srv")),
            p,
            build_connack,
        );

        let mut p = Properties::new();
        p.reason_string = Some("rs".to_string());
        add(
            "CONNACK 0x1F ReasonString=rs",
            PacketType::Connack,
            PropertyContext::Connack,
            connack_bytes(&prop_str(0x1F, "rs")),
            p,
            build_connack,
        );

        let mut p = Properties::new();
        p.maximum_qos = Some(1);
        add(
            "CONNACK 0x24 MaximumQos=1",
            PacketType::Connack,
            PropertyContext::Connack,
            connack_bytes(&prop_u8(0x24, 0x01)),
            p,
            build_connack,
        );

        let mut p = Properties::new();
        p.retain_available = Some(false);
        add(
            "CONNACK 0x25 RetainAvailable=0",
            PacketType::Connack,
            PropertyContext::Connack,
            connack_bytes(&prop_u8(0x25, 0x00)),
            p,
            build_connack,
        );

        let mut p = Properties::new();
        p.wildcard_subscription_available = Some(true);
        add(
            "CONNACK 0x28 WildcardSubscriptionAvailable=1",
            PacketType::Connack,
            PropertyContext::Connack,
            connack_bytes(&prop_u8(0x28, 0x01)),
            p,
            build_connack,
        );

        let mut p = Properties::new();
        p.subscription_identifier_available = Some(false);
        add(
            "CONNACK 0x29 SubscriptionIdentifierAvailable=0",
            PacketType::Connack,
            PropertyContext::Connack,
            connack_bytes(&prop_u8(0x29, 0x00)),
            p,
            build_connack,
        );

        let mut p = Properties::new();
        p.shared_subscription_available = Some(true);
        add(
            "CONNACK 0x2A SharedSubscriptionAvailable=1",
            PacketType::Connack,
            PropertyContext::Connack,
            connack_bytes(&prop_u8(0x2A, 0x01)),
            p,
            build_connack,
        );

        // 组合属性：必须按属性 ID 升序编码（0x12、0x21、0x24、0x25）。
        let mut p = Properties::new();
        p.assigned_client_identifier = Some("id".to_string());
        p.receive_maximum = Some(10);
        p.maximum_qos = Some(1);
        p.retain_available = Some(true);
        add(
            "CONNACK 组合属性（0x12/0x21/0x24/0x25）",
            PacketType::Connack,
            PropertyContext::Connack,
            connack_bytes(&cat(&[
                prop_str(0x12, "id"),
                prop_u16(0x21, 10),
                prop_u8(0x24, 0x01),
                prop_u8(0x25, 0x01),
            ])),
            p,
            build_connack,
        );

        // ---- PUBACK 上下文：0x1F/0x26 ----
        let mut p = Properties::new();
        p.reason_string = Some("nf".to_string());
        add(
            "PUBACK 0x1F ReasonString=nf",
            PacketType::Puback,
            PropertyContext::Puback,
            ack_bytes(0x40, &prop_str(0x1F, "nf")),
            p,
            build_puback,
        );

        let mut p = Properties::new();
        p.user_property.push(("a".to_string(), "b".to_string()));
        add(
            "PUBACK 0x26 UserProperty（a/b）",
            PacketType::Puback,
            PropertyContext::Puback,
            ack_bytes(0x40, &prop_user_property("a", "b")),
            p,
            build_puback,
        );

        // ---- SUBSCRIBE 上下文：0x0B/0x26 ----
        let mut p = Properties::new();
        p.subscription_identifier = vec![7];
        add(
            "SUBSCRIBE 0x0B SubscriptionIdentifier=7",
            PacketType::Subscribe,
            PropertyContext::Subscribe,
            subscribe_bytes(&prop_varint_value(0x0B, 7)),
            p,
            build_subscribe,
        );

        let mut p = Properties::new();
        p.user_property.push(("sk".to_string(), "sv".to_string()));
        add(
            "SUBSCRIBE 0x26 UserProperty（sk/sv）",
            PacketType::Subscribe,
            PropertyContext::Subscribe,
            subscribe_bytes(&prop_user_property("sk", "sv")),
            p,
            build_subscribe,
        );

        // ---- UNSUBSCRIBE 上下文：0x26 ----
        let mut p = Properties::new();
        p.user_property.push(("uk".to_string(), "uv".to_string()));
        add(
            "UNSUBSCRIBE 0x26 UserProperty（uk/uv）",
            PacketType::Unsubscribe,
            PropertyContext::Unsubscribe,
            unsubscribe_bytes(&prop_user_property("uk", "uv")),
            p,
            build_unsubscribe,
        );

        // ---- DISCONNECT 上下文：0x11/0x1C/0x1F/0x26 ----
        let mut p = Properties::new();
        p.session_expiry_interval = Some(0);
        add(
            "DISCONNECT 0x11 SessionExpiryInterval=0",
            PacketType::Disconnect,
            PropertyContext::Disconnect,
            disconnect_bytes(&prop_u32(0x11, 0)),
            p,
            build_disconnect,
        );

        let mut p = Properties::new();
        p.server_reference = Some("other".to_string());
        add(
            "DISCONNECT 0x1C ServerReference=other",
            PacketType::Disconnect,
            PropertyContext::Disconnect,
            disconnect_bytes(&prop_str(0x1C, "other")),
            p,
            build_disconnect,
        );

        let mut p = Properties::new();
        p.reason_string = Some("bye".to_string());
        add(
            "DISCONNECT 0x1F ReasonString=bye",
            PacketType::Disconnect,
            PropertyContext::Disconnect,
            disconnect_bytes(&prop_str(0x1F, "bye")),
            p,
            build_disconnect,
        );

        let mut p = Properties::new();
        p.user_property.push(("dk".to_string(), "dv".to_string()));
        add(
            "DISCONNECT 0x26 UserProperty（dk/dv）",
            PacketType::Disconnect,
            PropertyContext::Disconnect,
            disconnect_bytes(&prop_user_property("dk", "dv")),
            p,
            build_disconnect,
        );

        // ---- AUTH 上下文：0x15/0x16/0x1F/0x26 ----
        let mut p = Properties::new();
        p.authentication_method = Some("m".to_string());
        add(
            "AUTH 0x15 AuthenticationMethod=m",
            PacketType::Auth,
            PropertyContext::Auth,
            auth_bytes(&prop_str(0x15, "m")),
            p,
            build_auth,
        );

        // AUTH 中 AuthenticationData 不强制要求 AuthenticationMethod。
        let mut p = Properties::new();
        p.authentication_data = Some(vec![0xAB]);
        add(
            "AUTH 0x16 AuthenticationData 无 Method（允许）",
            PacketType::Auth,
            PropertyContext::Auth,
            auth_bytes(&prop_binary(0x16, &[0xAB])),
            p,
            build_auth,
        );

        let mut p = Properties::new();
        p.reason_string = Some("ok".to_string());
        add(
            "AUTH 0x1F ReasonString=ok",
            PacketType::Auth,
            PropertyContext::Auth,
            auth_bytes(&prop_str(0x1F, "ok")),
            p,
            build_auth,
        );

        let mut p = Properties::new();
        p.user_property.push(("ak".to_string(), "av".to_string()));
        add(
            "AUTH 0x26 UserProperty（ak/av）",
            PacketType::Auth,
            PropertyContext::Auth,
            auth_bytes(&prop_user_property("ak", "av")),
            p,
            build_auth,
        );
    }
    cases
}

#[test]
fn properties_基础方法() {
    let mut properties = Properties::new();
    assert!(properties.is_empty());
    assert_eq!(properties, Properties::default());
    properties.receive_maximum = Some(1);
    assert!(!properties.is_empty());
}

#[test]
fn 属性_二十七种id字节级与构造往返() {
    let cases = property_wire_cases();
    for case in &cases {
        // 字节级：decode → encode 字节一致、Incomplete 前缀、encoded_len。
        assert_wire_roundtrip(&case.bytes, case.packet_type);
        // 解码后的属性必须与期望完全一致。
        let decoded = decode_ok(&case.bytes);
        let actual = packet_properties(&decoded, case.context)
            .unwrap_or_else(|| panic!("{}：报文中没有对应属性段", case.name));
        assert_eq!(actual, &case.expected, "{}：属性解析不符", case.name);
        // 构造级：同一份属性构造出的报文必须编码为同一手工字节。
        assert_packet_roundtrip((case.build)(case.expected.clone()), &case.bytes);
    }

    // 27 个属性 ID 每个都至少出现在一个用例中。
    for id in ALL_PROPERTY_IDS {
        assert!(
            cases.iter().any(|case| has_property(&case.expected, *id)),
            "属性 ID 0x{:X} 未覆盖",
            id.as_u32()
        );
    }
}

// ===========================================================================
// 属性规则负例
// ===========================================================================

#[test]
fn 属性_除user_property与publish的subscription_identifier外不得重复() {
    // PUBLISH 中重复 PayloadFormatIndicator。
    assert_protocol_error(&publish_bytes(&cat(&[
        prop_u8(0x01, 0x01),
        prop_u8(0x01, 0x00),
    ])));
    // CONNECT 中重复 ReceiveMaximum。
    assert_protocol_error(&connect_bytes(&cat(&[
        prop_u16(0x21, 10),
        prop_u16(0x21, 11),
    ])));
    // CONNACK 中重复 MaximumQos。
    assert_protocol_error(&connack_bytes(&cat(&[
        prop_u8(0x24, 0x00),
        prop_u8(0x24, 0x01),
    ])));
    // SUBSCRIBE 中 SubscriptionIdentifier 至多一次。
    assert_protocol_error(&subscribe_bytes(&cat(&[
        prop_varint_value(0x0B, 1),
        prop_varint_value(0x0B, 2),
    ])));
    // PUBACK 中重复 ReasonString。
    assert_protocol_error(&ack_bytes(
        0x40,
        &cat(&[prop_str(0x1F, "a"), prop_str(0x1F, "b")]),
    ));
}

#[test]
fn 属性_不允许出现在该上下文被拒() {
    // 每个属性场景挑一个明确不在允许集内的属性。
    // Connect 允许集不含 0x01。
    assert_protocol_error(&connect_bytes(&prop_u8(0x01, 0x01)));
    // Will 允许集不含 0x11。
    assert_protocol_error(&connect_with_will_bytes(&prop_u32(0x11, 0)));
    // Connack 允许集不含 0x09。
    assert_protocol_error(&connack_bytes(&prop_binary(0x09, &[1])));
    // Publish 允许集不含 0x11。
    assert_protocol_error(&publish_bytes(&prop_u32(0x11, 60)));
    // PUBACK/PUBREC/PUBREL/PUBCOMP 允许集不含 0x01。
    for first_byte in [0x40_u8, 0x50, 0x62, 0x70] {
        assert_protocol_error(&ack_bytes(first_byte, &prop_u8(0x01, 0x01)));
    }
    // Subscribe 允许集不含 0x03。
    assert_protocol_error(&subscribe_bytes(&prop_str(0x03, "x")));
    // Suback 允许集不含 0x01。
    assert_protocol_error(&suback_bytes(&prop_u8(0x01, 0x01)));
    // Unsubscribe 允许集不含 0x1F。
    assert_protocol_error(&unsubscribe_bytes(&prop_str(0x1F, "x")));
    // Unsuback 允许集不含 0x01。
    assert_protocol_error(&unsuback_bytes(&prop_u8(0x01, 0x01)));
    // Disconnect 允许集不含 0x21。
    assert_protocol_error(&disconnect_bytes(&prop_u16(0x21, 10)));
    // Auth 允许集不含 0x11。
    assert_protocol_error(&auth_bytes(&prop_u32(0x11, 0)));
}

#[test]
fn 属性_布尔与maximum_qos取值校验() {
    // 布尔语义属性只接受 0/1。
    assert_protocol_error(&publish_bytes(&prop_u8(0x01, 0x02)));
    assert_protocol_error(&connect_bytes(&prop_u8(0x17, 0x02)));
    assert_protocol_error(&connect_bytes(&prop_u8(0x19, 0xFF)));
    assert_protocol_error(&connack_bytes(&prop_u8(0x25, 0x02)));
    assert_protocol_error(&connack_bytes(&prop_u8(0x28, 0x02)));
    assert_protocol_error(&connack_bytes(&prop_u8(0x29, 0x02)));
    assert_protocol_error(&connack_bytes(&prop_u8(0x2A, 0x02)));
    // MaximumQos 只接受 0/1。
    assert_protocol_error(&connack_bytes(&prop_u8(0x24, 0x02)));
    assert_protocol_error(&connack_bytes(&prop_u8(0x24, 0xFF)));
}

#[test]
fn 属性_零值非法() {
    // ReceiveMaximum 不能为 0。
    assert_protocol_error(&connect_bytes(&prop_u16(0x21, 0)));
    assert_protocol_error(&connack_bytes(&prop_u16(0x21, 0)));
    // MaximumPacketSize 不能为 0。
    assert_protocol_error(&connect_bytes(&prop_u32(0x27, 0)));
    // TopicAlias 不能为 0。
    assert_protocol_error(&publish_bytes(&prop_u16(0x23, 0)));
    // SubscriptionIdentifier 不能为 0（VarInt 值 0）。
    assert_protocol_error(&publish_bytes(&prop_varint_value(0x0B, 0)));
    assert_protocol_error(&subscribe_bytes(&prop_varint_value(0x0B, 0)));
}

#[test]
fn 属性_认证数据依赖认证方法() {
    // CONNECT/CONNACK 中 AuthenticationData 存在时必须同时有 AuthenticationMethod。
    assert_protocol_error(&connect_bytes(&prop_binary(0x16, &[1, 2])));
    assert_protocol_error(&connack_bytes(&prop_binary(0x16, &[1, 2])));
    // AUTH 中不强制（规范歧义 MQTT-537），见正向用例「AUTH 0x16 无 Method」。
    let packet = decode_ok(&auth_bytes(&prop_binary(0x16, &[1, 2])));
    let properties = packet_properties(&packet, PropertyContext::Auth).expect("AUTH 必须有属性段");
    assert_eq!(properties.authentication_data, Some(vec![1, 2]));
    assert_eq!(properties.authentication_method, None);
}

#[test]
fn 属性_响应主题与分配客户端id语义() {
    // ResponseTopic 必须是合法主题名：非空、无通配符。
    assert_protocol_error(&publish_bytes(&prop_str(0x08, "a/+")));
    assert_protocol_error(&publish_bytes(&prop_str(0x08, "a/#")));
    assert_protocol_error(&publish_bytes(&prop_str(0x08, "")));
    // AssignedClientIdentifier 不能为空。
    assert_protocol_error(&connack_bytes(&prop_str(0x12, "")));
    // 对照：ContentType 允许空字符串。
    let packet = decode_ok(&publish_bytes(&prop_str(0x03, "")));
    let properties =
        packet_properties(&packet, PropertyContext::Publish).expect("PUBLISH 必须有属性段");
    assert_eq!(properties.content_type.as_deref(), Some(""));
}

#[test]
fn 属性_未知id与属性段长度错误() {
    // 未知属性 ID -> Malformed。
    assert_malformed(&publish_bytes(&[0x04, 0x01]));
    assert_malformed(&publish_bytes(&[0x30, 0x01]));
    assert_malformed(&publish_bytes(&[0x7F, 0x01]));
    // 属性 ID 用 VarInt 解码，非最小编码 -> Malformed。
    assert_malformed(&publish_bytes(&[0x80, 0x00]));
    // 属性长度超出报文体剩余字节 -> Malformed。
    let bytes = assemble_packet(0x30, &[0x00, 0x01, b't', 0x7F, b'x']);
    assert_malformed(&bytes);
    // 属性段内容比声明的属性值短 -> Malformed。
    let bytes = assemble_packet(0x30, &[0x00, 0x01, b't', 0x01, 0x02]);
    assert_malformed(&bytes);
    // 属性段内多余的字节被当作下一个属性 ID（未知）-> Malformed。
    let bytes = assemble_packet(0x30, &[0x00, 0x01, b't', 0x03, 0x01, 0x01, 0x00]);
    assert_malformed(&bytes);
    // 属性长度 VarInt 非最小编码 -> Malformed。
    let bytes = assemble_packet(0x40, &[0x00, 0x0A, 0x10, 0x80, 0x00]);
    assert_malformed(&bytes);
}

#[test]
fn 报文体末尾多余字节被拒() {
    // CONNACK：属性长度 0 之后还有 1 个多余字节。
    assert_malformed(&[0x20, 0x04, 0x00, 0x00, 0x00, 0x00]);
    // CONNECT：客户端 ID 之后还有多余字节。
    let mut bytes = GOLDEN_CONNECT_MIN.to_vec();
    bytes[1] = 0x10; // RL 从 15 改为 16
    bytes.push(0x00);
    assert_malformed(&bytes);
    // PUBACK：属性长度 0 之后还有多余字节（RL=5）。
    assert_malformed(&[0x40, 0x05, 0x00, 0x0A, 0x00, 0x00, 0x00]);
}

// ===========================================================================
// 全部语料：encoded_len 与 encode 长度一致
// ===========================================================================

/// 汇集所有字节级语料（黄金向量 + 属性用例），供长度一致性与 fuzz 复用。
fn wire_corpus() -> Vec<Vec<u8>> {
    let mut corpus: Vec<Vec<u8>> = GOLDEN_PACKETS
        .iter()
        .map(|(_, bytes, _)| bytes.to_vec())
        .collect();
    for case in property_wire_cases() {
        corpus.push(case.bytes);
    }
    corpus
}

#[test]
fn encoded_len_与encode长度一致_全部字节语料() {
    let corpus = wire_corpus();
    assert!(corpus.len() >= 60, "字节语料数量过少：{}", corpus.len());
    for bytes in &corpus {
        let packet = decode_ok(bytes);
        let encoded = packet.encode().expect("编码不应失败");
        assert_eq!(
            packet.encoded_len().expect("encoded_len 不应失败"),
            encoded.len(),
            "encoded_len 与 encode().len() 不一致：{}",
            hex(bytes)
        );
        assert_eq!(encoded, *bytes, "重编码字节不一致：{}", hex(bytes));
    }
}

#[test]
fn encoded_len_与encode长度一致_构造语料() {
    // 覆盖各类报文的构造语料。
    let mut packets = vec![
        Packet::Pingreq,
        Packet::Pingresp,
        Packet::Connect(Connect {
            clean_start: true,
            keep_alive: 0,
            client_id: String::new(),
            will: None,
            username: None,
            password: None,
            properties: Properties::new(),
        }),
    ];
    let mut properties = Properties::new();
    properties.receive_maximum = Some(1);
    properties.maximum_packet_size = Some(1024);
    packets.push(Packet::Connack(Connack {
        session_present: false,
        reason_code: ReasonCode::Success,
        properties,
    }));
    packets.push(Packet::Publish(Publish {
        dup: true,
        qos: QoS::ExactlyOnce,
        retain: false,
        topic: Some(TopicName::new("a/b/c").expect("主题名应合法")),
        packet_id: Some(0xFFFF),
        properties: Properties::new(),
        payload: vec![0x00; 300],
    }));
    for packet_type in [
        PacketType::Puback,
        PacketType::Pubrec,
        PacketType::Pubrel,
        PacketType::Pubcomp,
    ] {
        // PUBACK/PUBREC 允许 No Matching Subscribers；PUBREL/PUBCOMP 只允许 0x92。
        let failure_reason = if matches!(packet_type, PacketType::Pubrel | PacketType::Pubcomp) {
            ReasonCode::PacketIdentifierNotFound
        } else {
            ReasonCode::NoMatchingSubscribers
        };
        packets.push(build_ack(
            packet_type,
            1,
            ReasonCode::Success,
            Properties::new(),
        ));
        packets.push(build_ack(
            packet_type,
            65535,
            failure_reason,
            Properties::new(),
        ));
    }
    packets.push(Packet::Subscribe(Subscribe {
        packet_id: 1,
        properties: Properties::new(),
        subscriptions: vec![
            Subscription {
                filter: TopicFilter::new("#").expect("过滤器应合法"),
                options: SubscriptionOptions::new(QoS::ExactlyOnce, false, true, 2)
                    .expect("订阅选项应合法"),
            },
            Subscription {
                filter: TopicFilter::new("$share/g/a/+").expect("共享订阅过滤器应合法"),
                options: SubscriptionOptions::new(QoS::AtMostOnce, false, false, 1)
                    .expect("订阅选项应合法"),
            },
        ],
    }));
    packets.push(Packet::Suback(Suback {
        packet_id: 2,
        properties: Properties::new(),
        reason_codes: vec![ReasonCode::GrantedQos1, ReasonCode::NotAuthorized],
    }));
    packets.push(Packet::Unsubscribe(Unsubscribe {
        packet_id: 3,
        properties: Properties::new(),
        filters: vec![TopicFilter::new("a/+/b").expect("过滤器应合法")],
    }));
    packets.push(Packet::Unsuback(Unsuback {
        packet_id: 3,
        properties: Properties::new(),
        reason_codes: vec![ReasonCode::Success, ReasonCode::NoSubscriptionExisted],
    }));
    packets.push(Packet::Disconnect(Disconnect {
        reason_code: ReasonCode::ServerShuttingDown,
        properties: Properties::new(),
    }));
    packets.push(Packet::Auth(Auth {
        reason_code: ReasonCode::ReAuthenticate,
        properties: Properties::new(),
    }));

    for packet in &packets {
        let encoded = packet.encode().expect("编码不应失败");
        assert_eq!(
            packet.encoded_len().expect("encoded_len 不应失败"),
            encoded.len(),
            "encoded_len 与 encode().len() 不一致：{packet:?}"
        );
        // encode_to 的结果也必须一致。
        let mut via_encode_to = Vec::new();
        packet
            .encode_to(&mut via_encode_to)
            .expect("encode_to 不应失败");
        assert_eq!(via_encode_to, encoded);
    }
}

// ===========================================================================
// decode_with_limit
// ===========================================================================

#[test]
fn decode_with_limit_边界与优先级() {
    // 限制值等于报文总长：正常解码。
    for (name, bytes, expected_type) in GOLDEN_PACKETS {
        let total = bytes.len() as u32;
        match Packet::decode_with_limit(bytes, total) {
            Ok((packet, used)) => {
                assert_eq!(
                    used,
                    bytes.len(),
                    "{name}：decode_with_limit 消耗字节数不符"
                );
                assert_eq!(packet.packet_type(), *expected_type, "{name}：报文类型不符");
            }
            Err(err) => panic!("{name}：限制值等于总长时应成功，错误 {err:?}"),
        }
        // 限制值恰小 1：PacketTooLarge。
        assert_limit_rejects(bytes);
        // 限制值足够大：正常解码。
        assert!(
            Packet::decode_with_limit(bytes, u32::MAX).is_ok(),
            "{name}：限制值足够大时应成功"
        );
    }

    // 优先级：固定头已完整、声明总长 > max 时，即使缓冲区还没收全也返回 PacketTooLarge。
    let truncated = &GOLDEN_CONNECT_FULL[..2];
    match Packet::decode_with_limit(truncated, 10) {
        Err(DecodeError::PacketTooLarge { size, max }) => {
            assert_eq!(
                size,
                GOLDEN_CONNECT_FULL.len(),
                "PacketTooLarge.size 应为声明总长"
            );
            assert_eq!(max, 10);
        }
        other => panic!("声明总长超限时应优先返回 PacketTooLarge，实际 {other:?}"),
    }

    // 固定头本身不完整时才是 Incomplete（优先于超限判断）。
    match Packet::decode_with_limit(&GOLDEN_CONNECT_FULL[..1], 1) {
        Err(DecodeError::Incomplete { needed }) => assert_eq!(needed, 2),
        other => panic!("固定头不完整时应返回 Incomplete，实际 {other:?}"),
    }
    assert!(matches!(
        Packet::decode_with_limit(&[], 0),
        Err(DecodeError::Incomplete { .. })
    ));

    // 极小限制值：PINGREQ 总长 2，限制 1 时必须拒绝。
    match Packet::decode_with_limit(GOLDEN_PINGREQ, 1) {
        Err(DecodeError::PacketTooLarge { size, max }) => {
            assert_eq!(size, 2);
            assert_eq!(max, 1);
        }
        other => panic!("PINGREQ 限制 1 时应返回 PacketTooLarge，实际 {other:?}"),
    }
}

#[test]
fn decode_多报文串联只消费一个报文() {
    let mut concatenated = GOLDEN_PUBLISH.to_vec();
    concatenated.extend_from_slice(GOLDEN_PINGREQ);
    concatenated.extend_from_slice(GOLDEN_AUTH);
    let (packet, used) = Packet::decode(&concatenated).expect("第一个报文应可解码");
    assert_eq!(used, GOLDEN_PUBLISH.len());
    assert_eq!(packet.packet_type(), PacketType::Publish);
    let mut offset = used;
    let (packet, used) = Packet::decode(&concatenated[offset..]).expect("第二个报文应可解码");
    assert_eq!(used, GOLDEN_PINGREQ.len());
    assert_eq!(packet, Packet::Pingreq);
    offset += used;
    let (packet, used) = Packet::decode(&concatenated[offset..]).expect("第三个报文应可解码");
    assert_eq!(used, GOLDEN_AUTH.len());
    assert_eq!(
        offset + used,
        concatenated.len(),
        "三个报文应恰好消费完整字节流"
    );
    assert_eq!(
        packet,
        Packet::Auth(Auth {
            reason_code: ReasonCode::ContinueAuthentication,
            properties: Properties::new(),
        })
    );
}

// ===========================================================================
// 错误分级：固定头 / CONNECT
// ===========================================================================

#[test]
fn packet_decode_固定头flags错误() {
    // CONNECT flags 非 0。
    assert_malformed(&[0x11, 0x00]);
    // PINGREQ / PINGRESP flags 非 0。
    assert_malformed(&[0xC1, 0x00]);
    assert_malformed(&[0xD1, 0x00]);
    // PUBREL 必须 flags 0b0010。
    assert_malformed(&[0x60, 0x02, 0x00, 0x01]);
    // SUBSCRIBE / UNSUBSCRIBE 必须 flags 0b0010。
    assert_malformed(&[0x80, 0x03, 0x00, 0x01, 0x00]);
    assert_malformed(&[0xA0, 0x03, 0x00, 0x01, 0x00]);
    // PUBLISH QoS=3。
    assert_malformed(&[0x36, 0x04, 0x00, 0x01, b't', 0x00]);
    // PUBLISH QoS0 + DUP。
    assert_malformed(&[0x38, 0x04, 0x00, 0x01, b't', 0x00]);
    // 报文类型 0。
    assert_malformed(&[0x00, 0x00]);
    // 固定头 Remaining Length 非最小编码。
    assert_malformed(&[0x30, 0x80, 0x00]);
}

/// 拼装任意 CONNECT：协议名、版本、连接标志、Keep Alive、属性、载荷。
fn connect_raw(
    protocol_name: &str,
    version: u8,
    flags: u8,
    keep_alive: u16,
    property_content: &[u8],
    payload: &[u8],
) -> Vec<u8> {
    let mut body = encode_utf8(protocol_name);
    body.push(version);
    body.push(flags);
    body.extend_from_slice(&keep_alive.to_be_bytes());
    body.extend_from_slice(&property_section(property_content));
    body.extend_from_slice(payload);
    assemble_packet(0x10, &body)
}

#[test]
fn connect_协议名版本保留位与will标志错误() {
    let payload = encode_utf8("c");
    // 协议名必须是 "MQTT" -> Malformed。
    assert_malformed(&connect_raw("MQTX", 5, 0x02, 60, &[], &payload));
    assert_malformed(&connect_raw("MQTTX", 5, 0x02, 60, &[], &payload));
    // 协议版本必须为 5 -> UnsupportedProtocolVersion。
    assert_unsupported_version(&connect_raw("MQTT", 4, 0x02, 60, &[], &payload), 4);
    assert_unsupported_version(&connect_raw("MQTT", 6, 0x02, 60, &[], &payload), 6);
    assert_unsupported_version(&connect_raw("MQTT", 0, 0x02, 60, &[], &payload), 0);
    // 连接标志 bit0 是保留位，必须为 0 -> Malformed。
    assert_malformed(&connect_raw("MQTT", 5, 0x03, 60, &[], &payload));
    // Will Flag=0 但 Will QoS 位非 0 -> Malformed。
    assert_malformed(&connect_raw("MQTT", 5, 0x0A, 60, &[], &payload));
    // Will Flag=0 但 Will Retain 位非 0 -> Malformed。
    assert_malformed(&connect_raw("MQTT", 5, 0x22, 60, &[], &payload));
    // Will Flag=1 但 Will QoS=3 -> Malformed。
    assert_malformed(&connect_raw("MQTT", 5, 0x1E, 60, &[], &payload));
    // 载荷缺少客户端 ID -> Malformed（报文体按 Remaining Length 已完整）。
    assert_malformed(&connect_raw("MQTT", 5, 0x02, 60, &[], &[]));
    // Will Flag=1 但载荷缺少 Will 属性/主题/载荷 -> Malformed。
    assert_malformed(&connect_raw("MQTT", 5, 0x06, 60, &[], &payload));
    // CONNECT 中出现未知属性 ID -> Malformed。
    assert_malformed(&connect_raw(
        "MQTT",
        5,
        0x02,
        60,
        &prop_u8(0x04, 0x01),
        &payload,
    ));
}

#[test]
fn connect_密码可以没有用户名() {
    // MQTT 5.0 允许 Password Flag=1 而 User Name Flag=0。
    let payload = cat(&[encode_utf8("c"), encode_binary(b"p")]);
    let bytes = connect_raw("MQTT", 5, 0x42, 60, &[], &payload);
    assert_wire_roundtrip(&bytes, PacketType::Connect);
    let expected = Packet::Connect(Connect {
        clean_start: true,
        keep_alive: 60,
        client_id: "c".to_string(),
        will: None,
        username: None,
        password: Some(b"p".to_vec()),
        properties: Properties::new(),
    });
    assert_eq!(decode_ok(&bytes), expected);
    assert_packet_roundtrip(expected, &bytes);
}

// ===========================================================================
// 错误分级：PUBLISH 主题与 QoS
// ===========================================================================

/// 拼装 PUBLISH（QoS0）：主题名字节任意、属性内容任意、载荷任意。
fn publish_raw(topic_name_bytes: &[u8], property_content: &[u8], payload: &[u8]) -> Vec<u8> {
    let mut body = Vec::new();
    assert!(topic_name_bytes.len() <= u16::MAX as usize);
    body.extend_from_slice(&(topic_name_bytes.len() as u16).to_be_bytes());
    body.extend_from_slice(topic_name_bytes);
    body.extend_from_slice(&property_section(property_content));
    body.extend_from_slice(payload);
    assemble_packet(0x30, &body)
}

/// 拼装 QoS1 PUBLISH（带包 ID）。
fn publish_qos1_bytes(topic: &str, packet_id: u16, payload: &[u8]) -> Vec<u8> {
    let mut body = encode_utf8(topic);
    body.extend_from_slice(&packet_id.to_be_bytes());
    body.extend_from_slice(&property_section(&[]));
    body.extend_from_slice(payload);
    assemble_packet(0x32, &body)
}

#[test]
fn publish_主题utf8与通配符校验() {
    // 非法 UTF-8 主题 -> Malformed。
    assert_malformed(&publish_raw(&[0xFF, 0xFE], &[], b""));
    // 主题含 U+0000 -> Malformed。
    assert_malformed(&publish_raw(b"a\x00b", &[], b""));
    // 主题含通配符 -> ProtocolError。
    assert_protocol_error(&publish_raw(b"+", &[], b""));
    assert_protocol_error(&publish_raw(b"a/#", &[], b""));
    assert_protocol_error(&publish_raw(b"a/+/b", &[], b""));
    // 空主题且没有 TopicAlias -> ProtocolError。
    assert_protocol_error(&publish_raw(b"", &[], b""));
}

#[test]
fn publish_空主题加主题别名合法() {
    // 空主题 + TopicAlias 是主题别名续用的合法形式。
    let bytes = assemble_packet(
        0x30,
        &cat(&[vec![0x00, 0x00], property_section(&prop_u16(0x23, 1))]),
    );
    assert_wire_roundtrip(&bytes, PacketType::Publish);
    let mut properties = Properties::new();
    properties.topic_alias = Some(1);
    let expected = Packet::Publish(Publish {
        dup: false,
        qos: QoS::AtMostOnce,
        retain: false,
        topic: None,
        packet_id: None,
        properties,
        payload: Vec::new(),
    });
    assert_eq!(decode_ok(&bytes), expected);
    assert_packet_roundtrip(expected, &bytes);
}

#[test]
fn publish_qos2与dup往返() {
    let packet = Packet::Publish(Publish {
        dup: true,
        qos: QoS::ExactlyOnce,
        retain: true,
        topic: Some(TopicName::new("a/b").expect("主题名应合法")),
        packet_id: Some(0xFFFF),
        properties: Properties::new(),
        payload: b"payload".to_vec(),
    });
    let encoded = packet.encode().expect("编码不应失败");
    // flags = DUP 0x08 | QoS2 0x04 | RETAIN 0x01 = 0x0D，首字节 0x3D。
    assert_eq!(encoded[0], 0x3D);
    assert_eq!(decode_ok(&encoded), packet);
}

// ===========================================================================
// 错误分级：包 ID 为 0
// ===========================================================================

fn subscribe_with_pid(packet_id: u16) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&packet_id.to_be_bytes());
    body.extend_from_slice(&property_section(&[]));
    body.extend_from_slice(&encode_utf8("a"));
    body.push(0x00);
    assemble_packet(0x82, &body)
}

fn suback_with_pid(packet_id: u16) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&packet_id.to_be_bytes());
    body.extend_from_slice(&property_section(&[]));
    body.push(0x00);
    assemble_packet(0x90, &body)
}

fn unsubscribe_with_pid(packet_id: u16) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&packet_id.to_be_bytes());
    body.extend_from_slice(&property_section(&[]));
    body.extend_from_slice(&encode_utf8("a"));
    assemble_packet(0xA2, &body)
}

fn unsuback_with_pid(packet_id: u16) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&packet_id.to_be_bytes());
    body.extend_from_slice(&property_section(&[]));
    body.push(0x00);
    assemble_packet(0xB0, &body)
}

#[test]
fn 包id为零被拒() {
    // PUBACK / PUBREC / PUBREL / PUBCOMP 包 ID 为 0。
    for first_byte in [0x40_u8, 0x50, 0x62, 0x70] {
        assert_protocol_error(&[first_byte, 0x02, 0x00, 0x00]);
    }
    // PUBLISH QoS1 包 ID 为 0。
    assert_protocol_error(&publish_qos1_bytes("a", 0, b"x"));
    // SUBSCRIBE / SUBACK / UNSUBSCRIBE / UNSUBACK 包 ID 为 0。
    assert_protocol_error(&subscribe_with_pid(0));
    assert_protocol_error(&suback_with_pid(0));
    assert_protocol_error(&unsubscribe_with_pid(0));
    assert_protocol_error(&unsuback_with_pid(0));
}

// ===========================================================================
// 错误分级：空载荷与订阅选项
// ===========================================================================

fn subscribe_with_options(options: u8) -> Vec<u8> {
    let mut body = vec![0x00, 0x0A];
    body.extend_from_slice(&property_section(&[]));
    body.extend_from_slice(&encode_utf8("a"));
    body.push(options);
    assemble_packet(0x82, &body)
}

fn subscribe_with_filter(filter: &str) -> Vec<u8> {
    let mut body = vec![0x00, 0x0A];
    body.extend_from_slice(&property_section(&[]));
    body.extend_from_slice(&encode_utf8(filter));
    body.push(0x00);
    assemble_packet(0x82, &body)
}

fn unsubscribe_with_filter(filter: &str) -> Vec<u8> {
    let mut body = vec![0x00, 0x0A];
    body.extend_from_slice(&property_section(&[]));
    body.extend_from_slice(&encode_utf8(filter));
    assemble_packet(0xA2, &body)
}

#[test]
fn 空载荷被拒() {
    // 空 SUBSCRIBE / SUBACK / UNSUBSCRIBE / UNSUBACK 载荷 -> ProtocolError。
    assert_protocol_error(&assemble_packet(
        0x82,
        &cat(&[vec![0x00, 0x0A], property_section(&[])]),
    ));
    assert_protocol_error(&assemble_packet(
        0x90,
        &cat(&[vec![0x00, 0x0A], property_section(&[])]),
    ));
    assert_protocol_error(&assemble_packet(
        0xA2,
        &cat(&[vec![0x00, 0x0A], property_section(&[])]),
    ));
    assert_protocol_error(&assemble_packet(
        0xB0,
        &cat(&[vec![0x00, 0x0A], property_section(&[])]),
    ));
}

#[test]
fn subscribe_过滤器与选项校验() {
    // 空主题过滤器 -> ProtocolError。
    assert_protocol_error(&subscribe_with_filter(""));
    assert_protocol_error(&unsubscribe_with_filter(""));
    // 非法过滤器（通配符未独占层级）-> ProtocolError。
    assert_protocol_error(&subscribe_with_filter("a+"));
    assert_protocol_error(&subscribe_with_filter("#/x"));
    // 过滤器含 NUL -> Malformed（UTF-8 编码层）。
    assert_malformed(&subscribe_with_filter("a\x00b"));
    // 订阅选项保留位 bit6/bit7 非 0 -> Malformed。
    assert_malformed(&subscribe_with_options(0x40));
    assert_malformed(&subscribe_with_options(0x80));
    assert_malformed(&subscribe_with_options(0xC0));
    // 订阅选项 QoS=3 -> ProtocolError。
    assert_protocol_error(&subscribe_with_options(0x03));
    // 订阅选项 retain_handling=3 -> ProtocolError。
    assert_protocol_error(&subscribe_with_options(0x30));
    // 合法选项：QoS0 + NoLocal + RetainAsPublished + retain_handling=2 -> 0x2C。
    assert_wire_roundtrip(&subscribe_with_options(0x2C), PacketType::Subscribe);
}

// ===========================================================================
// 错误分级：原因码
// ===========================================================================

#[test]
fn 原因码解码校验() {
    // 未知原因码字节 -> Malformed。
    assert_malformed(&[0x40, 0x03, 0x00, 0x0A, 0x03]);
    assert_malformed(&[0x90, 0x04, 0x00, 0x0A, 0x00, 0x03]);
    assert_malformed(&[0xE0, 0x02, 0x03, 0x00]);
    // 已知原因码但不在该报文允许集内 -> ProtocolError。
    // PUBACK 不允许 Granted QoS 1 (0x01)。
    assert_protocol_error(&[0x40, 0x03, 0x00, 0x0A, 0x01]);
    // PUBREL 不允许 No Matching Subscribers (0x10)。
    assert_protocol_error(&[0x62, 0x03, 0x00, 0x0A, 0x10]);
    // CONNACK 不允许 Granted QoS 1 (0x01)。
    assert_protocol_error(&[0x20, 0x03, 0x00, 0x01, 0x00]);
    // SUBACK 不允许 Disconnect with Will Message (0x04)。
    assert_protocol_error(&[0x90, 0x04, 0x00, 0x0A, 0x00, 0x04]);
    // UNSUBACK 不允许 No Matching Subscribers (0x10)。
    assert_protocol_error(&[0xB0, 0x04, 0x00, 0x0A, 0x00, 0x10]);
    // DISCONNECT 不允许 Granted QoS 1 (0x01)。
    assert_protocol_error(&[0xE0, 0x02, 0x01, 0x00]);
    // AUTH 不允许 Granted QoS 1 (0x01)。
    assert_protocol_error(&[0xF0, 0x02, 0x01, 0x00]);
    // CONNACK 连接确认标志保留位非 0 -> Malformed。
    assert_malformed(&[0x20, 0x03, 0x02, 0x00, 0x00]);
    // CONNACK 报文体过短（缺少属性长度）-> Malformed。
    assert_malformed(&[0x20, 0x02, 0x00, 0x00]);
}

// ===========================================================================
// 编码侧校验（EncodeError）
// ===========================================================================

/// 断言编码失败且错误为 EncodeError::Invalid。
fn assert_invalid_encode(packet: Packet) {
    match packet.encode() {
        Err(EncodeError::Invalid(message)) => assert!(!message.is_empty(), "Invalid 消息不应为空"),
        Err(other) => panic!("期望 EncodeError::Invalid，实际 {other:?}：{packet:?}"),
        Ok(bytes) => panic!("编码本应失败却成功：{packet:?} => {}", hex(&bytes)),
    }
    let mut out = Vec::new();
    assert!(
        matches!(packet.encode_to(&mut out), Err(EncodeError::Invalid(_))),
        "encode_to 也应返回 Invalid：{packet:?}"
    );
    assert!(
        packet.encoded_len().is_err(),
        "encoded_len 也应失败：{packet:?}"
    );
}

fn empty_ack(packet_id: u16, reason_code: ReasonCode) -> Ack {
    Ack {
        packet_id,
        reason_code,
        properties: Properties::new(),
    }
}

#[test]
fn encode_包id与qos组合校验() {
    // ACK 包 ID 为 0。
    assert_invalid_encode(Packet::Puback(empty_ack(0, ReasonCode::Success)));
    assert_invalid_encode(Packet::Pubrec(empty_ack(0, ReasonCode::Success)));
    assert_invalid_encode(Packet::Pubrel(empty_ack(0, ReasonCode::Success)));
    assert_invalid_encode(Packet::Pubcomp(empty_ack(0, ReasonCode::Success)));
    // SUBSCRIBE / UNSUBSCRIBE 包 ID 为 0。
    assert_invalid_encode(Packet::Subscribe(Subscribe {
        packet_id: 0,
        properties: Properties::new(),
        subscriptions: vec![Subscription {
            filter: TopicFilter::new("a").expect("过滤器应合法"),
            options: SubscriptionOptions::new(QoS::AtMostOnce, false, false, 0)
                .expect("订阅选项应合法"),
        }],
    }));
    assert_invalid_encode(Packet::Unsubscribe(Unsubscribe {
        packet_id: 0,
        properties: Properties::new(),
        filters: vec![TopicFilter::new("a").expect("过滤器应合法")],
    }));

    // PUBLISH QoS0 带包 ID。
    assert_invalid_encode(Packet::Publish(Publish {
        dup: false,
        qos: QoS::AtMostOnce,
        retain: false,
        topic: Some(TopicName::new("a").expect("主题名应合法")),
        packet_id: Some(1),
        properties: Properties::new(),
        payload: Vec::new(),
    }));
    // PUBLISH QoS1/QoS2 不带包 ID。
    for qos in [QoS::AtLeastOnce, QoS::ExactlyOnce] {
        assert_invalid_encode(Packet::Publish(Publish {
            dup: false,
            qos,
            retain: false,
            topic: Some(TopicName::new("a").expect("主题名应合法")),
            packet_id: None,
            properties: Properties::new(),
            payload: Vec::new(),
        }));
    }
    // PUBLISH QoS0 + DUP。
    assert_invalid_encode(Packet::Publish(Publish {
        dup: true,
        qos: QoS::AtMostOnce,
        retain: false,
        topic: Some(TopicName::new("a").expect("主题名应合法")),
        packet_id: None,
        properties: Properties::new(),
        payload: Vec::new(),
    }));
    // PUBLISH 空主题但没有 TopicAlias。
    assert_invalid_encode(Packet::Publish(Publish {
        dup: false,
        qos: QoS::AtMostOnce,
        retain: false,
        topic: None,
        packet_id: None,
        properties: Properties::new(),
        payload: Vec::new(),
    }));
}

#[test]
fn encode_空列表与原因码校验() {
    // 空 SUBSCRIBE / SUBACK / UNSUBSCRIBE / UNSUBACK。
    assert_invalid_encode(Packet::Subscribe(Subscribe {
        packet_id: 1,
        properties: Properties::new(),
        subscriptions: Vec::new(),
    }));
    assert_invalid_encode(Packet::Suback(Suback {
        packet_id: 1,
        properties: Properties::new(),
        reason_codes: Vec::new(),
    }));
    assert_invalid_encode(Packet::Unsubscribe(Unsubscribe {
        packet_id: 1,
        properties: Properties::new(),
        filters: Vec::new(),
    }));
    assert_invalid_encode(Packet::Unsuback(Unsuback {
        packet_id: 1,
        properties: Properties::new(),
        reason_codes: Vec::new(),
    }));
    // SUBACK 中的原因码必须是 0x00/0x01/0x02 或 SUBACK 允许的错误码。
    assert_invalid_encode(Packet::Suback(Suback {
        packet_id: 1,
        properties: Properties::new(),
        reason_codes: vec![ReasonCode::NoMatchingSubscribers],
    }));
    // UNSUBACK 不允许 Granted QoS 1。
    assert_invalid_encode(Packet::Unsuback(Unsuback {
        packet_id: 1,
        properties: Properties::new(),
        reason_codes: vec![ReasonCode::GrantedQos1],
    }));
    // PUBACK / PUBREL 不允许的原因码。
    assert_invalid_encode(Packet::Puback(empty_ack(1, ReasonCode::GrantedQos1)));
    assert_invalid_encode(Packet::Pubrel(empty_ack(
        1,
        ReasonCode::NoMatchingSubscribers,
    )));
    // CONNACK / DISCONNECT / AUTH 不允许的原因码。
    assert_invalid_encode(Packet::Connack(Connack {
        session_present: false,
        reason_code: ReasonCode::ContinueAuthentication,
        properties: Properties::new(),
    }));
    assert_invalid_encode(Packet::Disconnect(Disconnect {
        reason_code: ReasonCode::ContinueAuthentication,
        properties: Properties::new(),
    }));
    assert_invalid_encode(Packet::Auth(Auth {
        reason_code: ReasonCode::GrantedQos1,
        properties: Properties::new(),
    }));
}

#[test]
fn encode_属性上下文与取值校验() {
    // 属性不允许出现在该上下文。
    let mut properties = Properties::new();
    properties.topic_alias = Some(1);
    assert_invalid_encode(Packet::Connect(Connect {
        clean_start: true,
        keep_alive: 0,
        client_id: "c".to_string(),
        will: None,
        username: None,
        password: None,
        properties,
    }));

    let mut properties = Properties::new();
    properties.session_expiry_interval = Some(60);
    assert_invalid_encode(Packet::Publish(Publish {
        dup: false,
        qos: QoS::AtMostOnce,
        retain: false,
        topic: Some(TopicName::new("a").expect("主题名应合法")),
        packet_id: None,
        properties,
        payload: Vec::new(),
    }));

    let mut properties = Properties::new();
    properties.correlation_data = Some(vec![1]);
    assert_invalid_encode(Packet::Connack(Connack {
        session_present: false,
        reason_code: ReasonCode::Success,
        properties,
    }));

    let mut will_properties = Properties::new();
    will_properties.session_expiry_interval = Some(0);
    assert_invalid_encode(Packet::Connect(Connect {
        clean_start: true,
        keep_alive: 0,
        client_id: "c".to_string(),
        will: Some(Will {
            topic: TopicName::new("w").expect("Will 主题应合法"),
            payload: Vec::new(),
            qos: QoS::AtMostOnce,
            retain: false,
            properties: will_properties,
        }),
        username: None,
        password: None,
        properties: Properties::new(),
    }));

    // AuthenticationData 在 CONNECT 中必须与 AuthenticationMethod 同时出现。
    let mut properties = Properties::new();
    properties.authentication_data = Some(vec![1, 2]);
    assert_invalid_encode(Packet::Connect(Connect {
        clean_start: true,
        keep_alive: 0,
        client_id: "c".to_string(),
        will: None,
        username: None,
        password: None,
        properties,
    }));

    // 零值属性错误。
    let mut properties = Properties::new();
    properties.receive_maximum = Some(0);
    assert_invalid_encode(Packet::Connect(Connect {
        clean_start: true,
        keep_alive: 0,
        client_id: "c".to_string(),
        will: None,
        username: None,
        password: None,
        properties,
    }));

    let mut properties = Properties::new();
    properties.maximum_packet_size = Some(0);
    assert_invalid_encode(Packet::Connect(Connect {
        clean_start: true,
        keep_alive: 0,
        client_id: "c".to_string(),
        will: None,
        username: None,
        password: None,
        properties,
    }));

    let mut properties = Properties::new();
    properties.topic_alias = Some(0);
    assert_invalid_encode(Packet::Publish(Publish {
        dup: false,
        qos: QoS::AtMostOnce,
        retain: false,
        topic: Some(TopicName::new("a").expect("主题名应合法")),
        packet_id: None,
        properties,
        payload: Vec::new(),
    }));

    let mut properties = Properties::new();
    properties.subscription_identifier = vec![0];
    assert_invalid_encode(Packet::Publish(Publish {
        dup: false,
        qos: QoS::AtMostOnce,
        retain: false,
        topic: Some(TopicName::new("a").expect("主题名应合法")),
        packet_id: None,
        properties,
        payload: Vec::new(),
    }));

    // SUBSCRIBE 中 SubscriptionIdentifier 至多一个。
    let mut properties = Properties::new();
    properties.subscription_identifier = vec![1, 2];
    assert_invalid_encode(Packet::Subscribe(Subscribe {
        packet_id: 1,
        properties,
        subscriptions: vec![Subscription {
            filter: TopicFilter::new("a").expect("过滤器应合法"),
            options: SubscriptionOptions::new(QoS::AtMostOnce, false, false, 0)
                .expect("订阅选项应合法"),
        }],
    }));

    // MaximumQos 只接受 0/1。
    let mut properties = Properties::new();
    properties.maximum_qos = Some(2);
    assert_invalid_encode(Packet::Connack(Connack {
        session_present: false,
        reason_code: ReasonCode::Success,
        properties,
    }));
}

#[test]
fn encode_字符串超长被拒() {
    // UTF-8 字符串（ContentType）超过 65535 字节。
    let mut properties = Properties::new();
    properties.content_type = Some("x".repeat(70_000));
    let packet = Packet::Publish(Publish {
        dup: false,
        qos: QoS::AtMostOnce,
        retain: false,
        topic: Some(TopicName::new("a").expect("主题名应合法")),
        packet_id: None,
        properties,
        payload: Vec::new(),
    });
    match packet.encode() {
        Err(EncodeError::StringTooLong { field, len }) => {
            assert_eq!(field, "UTF-8 字符串");
            assert_eq!(len, 70_000);
        }
        other => panic!("期望 StringTooLong，实际 {other:?}"),
    }
    assert!(packet.encoded_len().is_err());

    // 二进制数据（CorrelationData）超过 65535 字节。
    let mut properties = Properties::new();
    properties.correlation_data = Some(vec![0; 70_000]);
    let packet = Packet::Publish(Publish {
        dup: false,
        qos: QoS::AtMostOnce,
        retain: false,
        topic: Some(TopicName::new("a").expect("主题名应合法")),
        packet_id: None,
        properties,
        payload: Vec::new(),
    });
    match packet.encode() {
        Err(EncodeError::StringTooLong { field, len }) => {
            assert_eq!(field, "二进制数据");
            assert_eq!(len, 70_000);
        }
        other => panic!("期望 StringTooLong，实际 {other:?}"),
    }
}

// ===========================================================================
// 固定种子 xorshift fuzz
// ===========================================================================

/// 固定种子 xorshift64：可复现的伪随机序列。
struct XorShift64 {
    state: u64,
}

impl XorShift64 {
    fn new(seed: u64) -> Self {
        assert_ne!(seed, 0, "xorshift 种子不能为 0");
        Self { state: seed }
    }

    fn next_u64(&mut self) -> u64 {
        let mut value = self.state;
        value ^= value << 13;
        value ^= value >> 7;
        value ^= value << 17;
        self.state = value;
        value
    }

    fn next_byte(&mut self) -> u8 {
        (self.next_u64() >> 32) as u8
    }

    fn below(&mut self, bound: usize) -> usize {
        assert!(bound > 0);
        (self.next_u64() % bound as u64) as usize
    }
}

/// fuzz 单次检查：decode 不 panic；成功解码后 re-encode → re-decode 结构相等。
fn fuzz_check(bytes: &[u8], round: usize) {
    match Packet::decode(bytes) {
        Ok((packet, used)) => {
            assert!(
                used <= bytes.len(),
                "第 {round} 轮：decode 消耗字节数 {used} 超过输入长度 {}：{}",
                bytes.len(),
                hex(bytes)
            );
            let encoded = match packet.encode() {
                Ok(encoded) => encoded,
                Err(err) => panic!(
                    "第 {round} 轮：解码成功但编码失败：{}，错误 {err:?}",
                    hex(bytes)
                ),
            };
            assert_eq!(
                packet
                    .encoded_len()
                    .expect("解码成功报文的 encoded_len 不应失败"),
                encoded.len(),
                "第 {round} 轮：encoded_len 不一致：{}",
                hex(bytes)
            );
            match Packet::decode(&encoded) {
                Ok((decoded_again, used_again)) => {
                    assert_eq!(
                        decoded_again,
                        packet,
                        "第 {round} 轮：重编码再解码结构不相等：{}",
                        hex(bytes)
                    );
                    assert_eq!(used_again, encoded.len());
                }
                Err(err) => panic!(
                    "第 {round} 轮：重编码结果无法解码：{}，错误 {err:?}",
                    hex(&encoded)
                ),
            }
            // 声明总长等于 encoded.len()，因此限制为总长时必须成功。
            match Packet::decode_with_limit(bytes, encoded.len() as u32) {
                Ok((limited, _)) => assert_eq!(limited, packet),
                Err(err) => panic!(
                    "第 {round} 轮：decode_with_limit 在总长等于限制时应成功：{}，错误 {err:?}",
                    hex(bytes)
                ),
            }
        }
        Err(DecodeError::Incomplete { needed }) => {
            assert!(
                needed > bytes.len(),
                "第 {round} 轮：Incomplete.needed={needed} 应大于输入长度 {}：{}",
                bytes.len(),
                hex(bytes)
            );
        }
        Err(_) => {
            // Malformed / ProtocolError / UnsupportedProtocolVersion / ... 都是合法拒绝。
        }
    }
}

#[test]
fn fuzz_随机字节解码不panic() {
    let mut rng = XorShift64::new(0x5EED_1234_ABCD_9876);
    for round in 0..5_000 {
        let len = rng.below(96);
        let mut bytes = vec![0_u8; len];
        for byte in &mut bytes {
            *byte = rng.next_byte();
        }
        fuzz_check(&bytes, round);
    }
}

#[test]
fn fuzz_黄金向量变异解码不panic() {
    let corpus = wire_corpus();
    let mut rng = XorShift64::new(0xDEAD_BEEF_0BAD_F00D);
    for round in 0..4_000 {
        let base = &corpus[rng.below(corpus.len())];
        let mut bytes = base.clone();
        match rng.next_u64() % 4 {
            0 => {
                // 随机翻位。
                let flips = 1 + rng.below(4);
                for _ in 0..flips {
                    if bytes.is_empty() {
                        break;
                    }
                    let index = rng.below(bytes.len());
                    let bit = rng.below(8) as u32;
                    bytes[index] ^= 1 << bit;
                }
            }
            1 => {
                // 随机截断（0 前后缀均可能）。
                let keep = rng.below(bytes.len() + 1);
                bytes.truncate(keep);
            }
            2 => {
                // 随机追加字节。
                let extra = rng.below(8);
                for _ in 0..extra {
                    bytes.push(rng.next_byte());
                }
            }
            _ => {
                // 随机替换一个字节。
                if !bytes.is_empty() {
                    let index = rng.below(bytes.len());
                    bytes[index] = rng.next_byte();
                }
            }
        }
        fuzz_check(&bytes, round);
    }
}
