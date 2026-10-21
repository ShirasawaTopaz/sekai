//! 报文层单元测试：golden 向量、round-trip、最短编码与错误分级。

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use crate::mqtt::error::{DecodeError, EncodeError};
use crate::mqtt::qos::QoS;
use crate::mqtt::reason::ReasonCode;
use crate::mqtt::topic::{Subscription, SubscriptionOptions, TopicFilter, TopicName};

use super::*;

fn topic(name: &str) -> TopicName {
    TopicName::new(String::from(name)).expect("测试主题应合法")
}

fn filter(name: &str) -> TopicFilter {
    TopicFilter::new(String::from(name)).expect("测试过滤器应合法")
}

fn empty_props() -> Properties {
    Properties::new()
}

fn subscription(name: &str, qos: QoS) -> Subscription {
    Subscription {
        filter: filter(name),
        options: SubscriptionOptions::new(qos, false, false, 0).expect("订阅选项应合法"),
    }
}

fn ack(packet_id: u16) -> Ack {
    Ack {
        packet_id,
        reason_code: ReasonCode::Success,
        properties: empty_props(),
    }
}

#[test]
fn connect_golden_vector() {
    let packet = Packet::Connect(Connect {
        clean_start: true,
        keep_alive: 60,
        client_id: String::from("cid"),
        will: None,
        username: Some(String::from("u")),
        password: Some(vec![1, 2, 3]),
        properties: empty_props(),
    });
    let expected = vec![
        0x10, 0x18, // 固定头：CONNECT，remaining length = 24
        0x00, 0x04, b'M', b'Q', b'T', b'T', // 协议名
        0x05, // 协议版本
        0xC2, // clean start + username + password
        0x00, 0x3C, // keep alive = 60
        0x00, // 属性长度 = 0
        0x00, 0x03, b'c', b'i', b'd', // client id
        0x00, 0x01, b'u', // username
        0x00, 0x03, 1, 2, 3, // password
    ];
    assert_eq!(packet.encode().unwrap(), expected);
    let (decoded, consumed) = Packet::decode(&expected).unwrap();
    assert_eq!(decoded, packet);
    assert_eq!(consumed, expected.len());
}

#[test]
fn connect_with_will_golden_vector() {
    let packet = Packet::Connect(Connect {
        clean_start: false,
        keep_alive: 0,
        client_id: String::from("c"),
        will: Some(Will {
            topic: topic("w"),
            payload: vec![0x70],
            qos: QoS::AtLeastOnce,
            retain: true,
            properties: empty_props(),
        }),
        username: None,
        password: None,
        properties: empty_props(),
    });
    let expected = vec![
        0x10, 0x15, // remaining length = 21
        0x00, 0x04, b'M', b'Q', b'T', b'T', 0x05, 0x2C, // will + will qos 1 + will retain
        0x00, 0x00, // keep alive = 0
        0x00, // CONNECT 属性长度
        0x00, 0x01, b'c', // client id
        0x00, // Will 属性长度
        0x00, 0x01, b'w', // Will 主题
        0x00, 0x01, 0x70, // Will 载荷
    ];
    assert_eq!(packet.encode().unwrap(), expected);
    assert_eq!(Packet::decode(&expected).unwrap().0, packet);
}

#[test]
fn connack_golden_vector() {
    let packet = Packet::Connack(Connack {
        session_present: true,
        reason_code: ReasonCode::Success,
        properties: empty_props(),
    });
    let expected = vec![0x20, 0x03, 0x01, 0x00, 0x00];
    assert_eq!(packet.encode().unwrap(), expected);
    assert_eq!(Packet::decode(&expected).unwrap().0, packet);
}

#[test]
fn publish_golden_vectors() {
    // QoS 0，普通主题
    let qos0 = Packet::Publish(Publish {
        dup: false,
        qos: QoS::AtMostOnce,
        retain: false,
        topic: Some(topic("a/b")),
        packet_id: None,
        properties: empty_props(),
        payload: vec![b'h', b'i'],
    });
    assert_eq!(
        qos0.encode().unwrap(),
        vec![0x30, 0x08, 0x00, 0x03, b'a', b'/', b'b', 0x00, b'h', b'i']
    );

    // QoS 1，带包 ID
    let qos1 = Packet::Publish(Publish {
        dup: false,
        qos: QoS::AtLeastOnce,
        retain: false,
        topic: Some(topic("t")),
        packet_id: Some(10),
        properties: empty_props(),
        payload: Vec::new(),
    });
    assert_eq!(
        qos1.encode().unwrap(),
        vec![0x32, 0x06, 0x00, 0x01, b't', 0x00, 0x0A, 0x00]
    );

    // 零长度主题 + Topic Alias
    let mut alias_props = empty_props();
    alias_props.topic_alias = Some(5);
    let alias = Packet::Publish(Publish {
        dup: false,
        qos: QoS::AtMostOnce,
        retain: false,
        topic: None,
        packet_id: None,
        properties: alias_props,
        payload: vec![0xAA],
    });
    assert_eq!(
        alias.encode().unwrap(),
        vec![0x30, 0x07, 0x00, 0x00, 0x03, 0x23, 0x00, 0x05, 0xAA]
    );

    for packet in [qos0, qos1, alias] {
        let bytes = packet.encode().unwrap();
        assert_eq!(Packet::decode(&bytes).unwrap().0, packet);
    }
}

#[test]
fn ack_shortest_forms() {
    // 成功且无属性 -> RL = 2
    assert_eq!(
        Packet::Puback(ack(7)).encode().unwrap(),
        vec![0x40, 0x02, 0x00, 0x07]
    );
    // 非成功原因码且无属性 -> RL = 3
    let mut failure = ack(7);
    failure.reason_code = ReasonCode::NoMatchingSubscribers;
    assert_eq!(
        Packet::Puback(failure.clone()).encode().unwrap(),
        vec![0x40, 0x03, 0x00, 0x07, 0x10]
    );
    // 带属性 -> RL >= 4（reason + 属性长度 + 属性）
    let mut with_props = ack(7);
    with_props.properties.reason_string = Some(String::from("x"));
    assert_eq!(
        Packet::Puback(with_props.clone()).encode().unwrap(),
        vec![0x40, 0x08, 0x00, 0x07, 0x00, 0x04, 0x1F, 0x00, 0x01, b'x']
    );
    // PUBREL 固定头 flags 必须是 0b0010
    assert_eq!(
        Packet::Pubrel(ack(7)).encode().unwrap(),
        vec![0x62, 0x02, 0x00, 0x07]
    );
    assert_eq!(
        Packet::Pubrec(ack(7)).encode().unwrap(),
        vec![0x50, 0x02, 0x00, 0x07]
    );
    assert_eq!(
        Packet::Pubcomp(ack(7)).encode().unwrap(),
        vec![0x70, 0x02, 0x00, 0x07]
    );
    for packet in [
        Packet::Puback(ack(7)),
        Packet::Puback(failure),
        Packet::Puback(with_props),
        Packet::Pubrel(ack(7)),
    ] {
        let bytes = packet.encode().unwrap();
        assert_eq!(Packet::decode(&bytes).unwrap().0, packet);
    }
}

#[test]
fn subscribe_golden_vector() {
    let packet = Packet::Subscribe(Subscribe {
        packet_id: 1,
        properties: empty_props(),
        subscriptions: vec![subscription("a/b", QoS::AtLeastOnce)],
    });
    let expected = vec![
        0x82, 0x09, 0x00, 0x01, 0x00, 0x00, 0x03, b'a', b'/', b'b', 0x01,
    ];
    assert_eq!(packet.encode().unwrap(), expected);
    assert_eq!(Packet::decode(&expected).unwrap().0, packet);
}

#[test]
fn suback_unsuback_golden_vectors() {
    let suback = Packet::Suback(Suback {
        packet_id: 1,
        properties: empty_props(),
        reason_codes: vec![ReasonCode::Success, ReasonCode::GrantedQos1],
    });
    assert_eq!(
        suback.encode().unwrap(),
        vec![0x90, 0x05, 0x00, 0x01, 0x00, 0x00, 0x01]
    );
    assert_eq!(Packet::decode(&suback.encode().unwrap()).unwrap().0, suback);

    let unsuback = Packet::Unsuback(Unsuback {
        packet_id: 2,
        properties: empty_props(),
        reason_codes: vec![ReasonCode::Success],
    });
    assert_eq!(
        unsuback.encode().unwrap(),
        vec![0xB0, 0x04, 0x00, 0x02, 0x00, 0x00]
    );
    assert_eq!(
        Packet::decode(&unsuback.encode().unwrap()).unwrap().0,
        unsuback
    );
}

#[test]
fn unsubscribe_golden_vector() {
    let packet = Packet::Unsubscribe(Unsubscribe {
        packet_id: 2,
        properties: empty_props(),
        filters: vec![filter("a/b")],
    });
    let expected = vec![0xA2, 0x08, 0x00, 0x02, 0x00, 0x00, 0x03, b'a', b'/', b'b'];
    assert_eq!(packet.encode().unwrap(), expected);
    assert_eq!(Packet::decode(&expected).unwrap().0, packet);
}

#[test]
fn ping_golden_vectors() {
    assert_eq!(Packet::Pingreq.encode().unwrap(), vec![0xC0, 0x00]);
    assert_eq!(Packet::Pingresp.encode().unwrap(), vec![0xD0, 0x00]);
    assert_eq!(Packet::decode(&[0xC0, 0x00]).unwrap().0, Packet::Pingreq);
    assert_eq!(Packet::decode(&[0xD0, 0x00]).unwrap().0, Packet::Pingresp);
}

#[test]
fn disconnect_and_auth_shortest_forms() {
    let default_disconnect = Packet::Disconnect(Disconnect {
        reason_code: ReasonCode::Success,
        properties: empty_props(),
    });
    assert_eq!(default_disconnect.encode().unwrap(), vec![0xE0, 0x00]);

    let will_disconnect = Packet::Disconnect(Disconnect {
        reason_code: ReasonCode::DisconnectWithWillMessage,
        properties: empty_props(),
    });
    assert_eq!(will_disconnect.encode().unwrap(), vec![0xE0, 0x01, 0x04]);

    let mut disconnect_props = empty_props();
    disconnect_props.session_expiry_interval = Some(60);
    let with_props = Packet::Disconnect(Disconnect {
        reason_code: ReasonCode::Success,
        properties: disconnect_props,
    });
    assert_eq!(
        with_props.encode().unwrap(),
        vec![0xE0, 0x07, 0x00, 0x05, 0x11, 0x00, 0x00, 0x00, 0x3C]
    );

    let default_auth = Packet::Auth(Auth {
        reason_code: ReasonCode::Success,
        properties: empty_props(),
    });
    assert_eq!(default_auth.encode().unwrap(), vec![0xF0, 0x00]);

    let continue_auth = Packet::Auth(Auth {
        reason_code: ReasonCode::ContinueAuthentication,
        properties: empty_props(),
    });
    assert_eq!(continue_auth.encode().unwrap(), vec![0xF0, 0x01, 0x18]);

    for packet in [
        default_disconnect,
        will_disconnect,
        with_props,
        default_auth,
        continue_auth,
    ] {
        let bytes = packet.encode().unwrap();
        assert_eq!(Packet::decode(&bytes).unwrap().0, packet);
    }
}

#[test]
fn round_trip_all_packet_types() {
    let mut publish_props = empty_props();
    publish_props.message_expiry_interval = Some(30);
    let packets = vec![
        Packet::Connect(Connect {
            clean_start: false,
            keep_alive: 30,
            client_id: String::from("client"),
            will: None,
            username: Some(String::from("user")),
            password: None,
            properties: empty_props(),
        }),
        Packet::Connack(Connack {
            session_present: false,
            reason_code: ReasonCode::Success,
            properties: empty_props(),
        }),
        Packet::Publish(Publish {
            dup: true,
            qos: QoS::ExactlyOnce,
            retain: true,
            topic: Some(topic("sensor/temp")),
            packet_id: Some(42),
            properties: publish_props,
            payload: vec![1, 2, 3, 4, 5],
        }),
        Packet::Puback(ack(1)),
        Packet::Pubrec(ack(2)),
        Packet::Pubrel(ack(3)),
        Packet::Pubcomp(ack(4)),
        Packet::Subscribe(Subscribe {
            packet_id: 5,
            properties: empty_props(),
            subscriptions: vec![
                subscription("a/+", QoS::AtLeastOnce),
                subscription("b/#", QoS::ExactlyOnce),
            ],
        }),
        Packet::Suback(Suback {
            packet_id: 5,
            properties: empty_props(),
            reason_codes: vec![ReasonCode::GrantedQos1, ReasonCode::GrantedQos2],
        }),
        Packet::Unsubscribe(Unsubscribe {
            packet_id: 6,
            properties: empty_props(),
            filters: vec![filter("a/+")],
        }),
        Packet::Unsuback(Unsuback {
            packet_id: 6,
            properties: empty_props(),
            reason_codes: vec![ReasonCode::NoSubscriptionExisted],
        }),
        Packet::Pingreq,
        Packet::Pingresp,
        Packet::Disconnect(Disconnect {
            reason_code: ReasonCode::Success,
            properties: empty_props(),
        }),
        Packet::Auth(Auth {
            reason_code: ReasonCode::Success,
            properties: empty_props(),
        }),
    ];

    for packet in packets {
        let bytes = packet.encode().unwrap();
        assert_eq!(packet.encoded_len().unwrap(), bytes.len());
        let (decoded, consumed) = Packet::decode(&bytes).unwrap();
        assert_eq!(decoded, packet, "解码结果应与原报文一致");
        assert_eq!(consumed, bytes.len());
        assert_eq!(
            bytes.len(),
            1 + varint::encoded_len((bytes.len() - 1 - 1) as u32) + (bytes.len() - 1 - 1)
        );
    }
}

#[test]
fn decode_consumes_exactly_one_packet() {
    let packet = Packet::Pingreq;
    let mut buf = packet.encode().unwrap();
    let single_len = buf.len();
    buf.extend_from_slice(&[0xDE, 0xAD]);
    let (decoded, consumed) = Packet::decode(&buf).unwrap();
    assert_eq!(decoded, Packet::Pingreq);
    assert_eq!(consumed, single_len);

    let connect = Packet::Connect(Connect {
        clean_start: true,
        keep_alive: 1,
        client_id: String::from("x"),
        will: None,
        username: None,
        password: None,
        properties: empty_props(),
    });
    let mut stream = connect.encode().unwrap();
    stream.extend_from_slice(&Packet::Pingresp.encode().unwrap());
    let (first, consumed) = Packet::decode(&stream).unwrap();
    assert_eq!(first, connect);
    let (second, _) = Packet::decode(&stream[consumed..]).unwrap();
    assert_eq!(second, Packet::Pingresp);
}

#[test]
fn incomplete_prefixes() {
    let packets = [
        Packet::Connect(Connect {
            clean_start: true,
            keep_alive: 60,
            client_id: String::from("cid"),
            will: None,
            username: Some(String::from("u")),
            password: Some(vec![1, 2, 3]),
            properties: empty_props(),
        }),
        Packet::Publish(Publish {
            dup: false,
            qos: QoS::AtLeastOnce,
            retain: false,
            topic: Some(topic("t")),
            packet_id: Some(10),
            properties: empty_props(),
            payload: vec![1, 2, 3],
        }),
        Packet::Subscribe(Subscribe {
            packet_id: 1,
            properties: empty_props(),
            subscriptions: vec![subscription("a/b", QoS::AtMostOnce)],
        }),
    ];
    for packet in packets {
        let bytes = packet.encode().unwrap();
        for prefix_len in 0..bytes.len() {
            match Packet::decode(&bytes[..prefix_len]) {
                Err(DecodeError::Incomplete { .. }) => {}
                other => panic!("前缀 {prefix_len} 应返回 Incomplete，实际：{other:?}"),
            }
        }
    }
}

#[test]
fn decode_with_limit_behaviour() {
    let packet = Packet::Connect(Connect {
        clean_start: true,
        keep_alive: 60,
        client_id: String::from("cid"),
        will: None,
        username: None,
        password: None,
        properties: empty_props(),
    });
    let bytes = packet.encode().unwrap();
    let total = bytes.len() as u32;
    assert!(Packet::decode_with_limit(&bytes, total).is_ok());
    match Packet::decode_with_limit(&bytes, total - 1) {
        Err(DecodeError::PacketTooLarge { size, max }) => {
            assert_eq!(size, total as usize);
            assert_eq!(max, total - 1);
        }
        other => panic!("应返回 PacketTooLarge，实际：{other:?}"),
    }
    // 总长已声明超过上限时应优先返回 PacketTooLarge（即使数据还不完整）
    match Packet::decode_with_limit(&bytes[..2], 2) {
        Err(DecodeError::PacketTooLarge { .. }) => {}
        other => panic!("应优先返回 PacketTooLarge，实际：{other:?}"),
    }
}

#[test]
fn malformed_cases() {
    // 报文类型 0
    assert!(matches!(
        Packet::decode(&[0x00, 0x00]),
        Err(DecodeError::Malformed(_))
    ));
    // 固定头 flags 非法：CONNECT 低 4 位必须为 0
    assert!(matches!(
        Packet::decode(&[0x11, 0x00]),
        Err(DecodeError::Malformed(_))
    ));
    // PUBLISH QoS 3
    assert!(matches!(
        Packet::decode(&[0x36, 0x00]),
        Err(DecodeError::Malformed(_))
    ));
    // PUBLISH QoS 0 + DUP
    assert!(matches!(
        Packet::decode(&[0x38, 0x00]),
        Err(DecodeError::Malformed(_))
    ));
    // PUBREL flags 非法
    assert!(matches!(
        Packet::decode(&[0x60, 0x00]),
        Err(DecodeError::Malformed(_))
    ));
    // PINGREQ 携带报文体
    assert!(matches!(
        Packet::decode(&[0xC0, 0x01, 0x00]),
        Err(DecodeError::Malformed(_))
    ));
    // PUBACK 未知原因码
    assert!(matches!(
        Packet::decode(&[0x40, 0x03, 0x00, 0x07, 0x03]),
        Err(DecodeError::Malformed(_))
    ));
    // 属性长度越界
    assert!(matches!(
        Packet::decode(&[0x40, 0x05, 0x00, 0x07, 0x00, 0x05, 0x1F]),
        Err(DecodeError::Malformed(_))
    ));
}

#[test]
fn protocol_error_cases() {
    // 包 ID 为 0
    assert!(matches!(
        Packet::decode(&[0x40, 0x02, 0x00, 0x00]),
        Err(DecodeError::ProtocolError(_))
    ));
    // 已知原因码但不允许用于 PUBACK
    assert!(matches!(
        Packet::decode(&[0x40, 0x03, 0x00, 0x07, 0x01]),
        Err(DecodeError::ProtocolError(_))
    ));
    // CONNACK 非成功原因码却设置 Session Present
    assert!(matches!(
        Packet::decode(&[0x20, 0x03, 0x01, 0x80, 0x00]),
        Err(DecodeError::ProtocolError(_))
    ));
    // 空 SUBSCRIBE
    assert!(matches!(
        Packet::decode(&[0x82, 0x03, 0x00, 0x01, 0x00]),
        Err(DecodeError::ProtocolError(_))
    ));
    // 空 SUBACK
    assert!(matches!(
        Packet::decode(&[0x90, 0x03, 0x00, 0x01, 0x00]),
        Err(DecodeError::ProtocolError(_))
    ));
    // 空 UNSUBSCRIBE
    assert!(matches!(
        Packet::decode(&[0xA2, 0x03, 0x00, 0x01, 0x00]),
        Err(DecodeError::ProtocolError(_))
    ));
    // 空 UNSUBACK
    assert!(matches!(
        Packet::decode(&[0xB0, 0x03, 0x00, 0x01, 0x00]),
        Err(DecodeError::ProtocolError(_))
    ));
    // SUBSCRIBE 空主题过滤器
    assert!(matches!(
        Packet::decode(&[0x82, 0x05, 0x00, 0x01, 0x00, 0x00, 0x00]),
        Err(DecodeError::ProtocolError(_))
    ));
    // 零长度主题且没有 Topic Alias
    assert!(matches!(
        Packet::decode(&[0x30, 0x03, 0x00, 0x00, 0x00]),
        Err(DecodeError::ProtocolError(_))
    ));
    // PUBLISH 主题包含通配符
    assert!(matches!(
        Packet::decode(&[0x30, 0x06, 0x00, 0x03, b'a', b'/', b'#', 0x00]),
        Err(DecodeError::ProtocolError(_))
    ));
}

#[test]
fn connect_decode_errors() {
    let connect = Packet::Connect(Connect {
        clean_start: true,
        keep_alive: 60,
        client_id: String::from("cid"),
        will: None,
        username: None,
        password: None,
        properties: empty_props(),
    });
    let mut bytes = connect.encode().unwrap();
    assert_eq!(&bytes[2..8], &[0x00, 0x04, b'M', b'Q', b'T', b'T']);

    // 协议名错误 -> Malformed
    let mut bad_name = bytes.clone();
    bad_name[4] = b'X';
    assert!(matches!(
        Packet::decode(&bad_name),
        Err(DecodeError::Malformed(_))
    ));

    // 协议版本错误 -> UnsupportedProtocolVersion
    let mut bad_version = bytes.clone();
    bad_version[8] = 4;
    assert!(matches!(
        Packet::decode(&bad_version),
        Err(DecodeError::UnsupportedProtocolVersion(4))
    ));

    // 保留位必须为 0
    let mut bad_flags = bytes.clone();
    bad_flags[9] |= 0x01;
    assert!(matches!(
        Packet::decode(&bad_flags),
        Err(DecodeError::Malformed(_))
    ));

    // Will flag 未设置但 Will QoS 位非 0
    let mut inconsistent_will = bytes.clone();
    inconsistent_will[9] |= 0x08;
    assert!(matches!(
        Packet::decode(&inconsistent_will),
        Err(DecodeError::Malformed(_))
    ));

    // 报文体末尾多余字节（把 Remaining Length 加一并补一个字节，使多余字节落在报文体内）
    bytes[1] += 1;
    bytes.push(0x00);
    assert!(matches!(
        Packet::decode(&bytes),
        Err(DecodeError::Malformed(_))
    ));
}

#[test]
fn encode_validation_cases() {
    // 空 SUBSCRIBE
    let empty_subscribe = Packet::Subscribe(Subscribe {
        packet_id: 1,
        properties: empty_props(),
        subscriptions: Vec::new(),
    });
    assert!(matches!(
        empty_subscribe.encoded_len(),
        Err(EncodeError::Invalid(_))
    ));

    // 空 SUBACK
    let empty_suback = Packet::Suback(Suback {
        packet_id: 1,
        properties: empty_props(),
        reason_codes: Vec::new(),
    });
    assert!(matches!(
        empty_suback.encode(),
        Err(EncodeError::Invalid(_))
    ));

    // SUBACK 中不允许的原因码
    let bad_suback = Packet::Suback(Suback {
        packet_id: 1,
        properties: empty_props(),
        reason_codes: vec![ReasonCode::NoMatchingSubscribers],
    });
    assert!(matches!(bad_suback.encode(), Err(EncodeError::Invalid(_))));

    // PUBACK 中不允许的原因码
    let mut bad_ack = ack(1);
    bad_ack.reason_code = ReasonCode::GrantedQos1;
    assert!(matches!(
        Packet::Puback(bad_ack).encode(),
        Err(EncodeError::Invalid(_))
    ));

    // QoS 0 的 PUBLISH 不能携带包 ID
    let qos0_with_id = Packet::Publish(Publish {
        dup: false,
        qos: QoS::AtMostOnce,
        retain: false,
        topic: Some(topic("t")),
        packet_id: Some(1),
        properties: empty_props(),
        payload: Vec::new(),
    });
    assert!(matches!(
        qos0_with_id.encode(),
        Err(EncodeError::Invalid(_))
    ));

    // QoS 0 的 PUBLISH 不能设置 DUP
    let qos0_dup = Packet::Publish(Publish {
        dup: true,
        qos: QoS::AtMostOnce,
        retain: false,
        topic: Some(topic("t")),
        packet_id: None,
        properties: empty_props(),
        payload: Vec::new(),
    });
    assert!(matches!(qos0_dup.encode(), Err(EncodeError::Invalid(_))));

    // 零长度主题但没有 Topic Alias
    let empty_topic = Packet::Publish(Publish {
        dup: false,
        qos: QoS::AtMostOnce,
        retain: false,
        topic: None,
        packet_id: None,
        properties: empty_props(),
        payload: Vec::new(),
    });
    assert!(matches!(empty_topic.encode(), Err(EncodeError::Invalid(_))));

    // Disconnect 中不允许的原因码
    let bad_disconnect = Packet::Disconnect(Disconnect {
        reason_code: ReasonCode::ServerUnavailable,
        properties: empty_props(),
    });
    assert!(matches!(
        bad_disconnect.encode(),
        Err(EncodeError::Invalid(_))
    ));

    // CONNACK 非成功原因码不能设置 Session Present
    let bad_connack = Packet::Connack(Connack {
        session_present: true,
        reason_code: ReasonCode::NotAuthorized,
        properties: empty_props(),
    });
    assert!(matches!(bad_connack.encode(), Err(EncodeError::Invalid(_))));
}
