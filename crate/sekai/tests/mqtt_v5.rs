//! MQTT 5.0 客户端状态机的脚本化集成测试。
//!
//! 测试不建立真实网络连接：手动构造服务端报文喂给 [`Client`]，
//! 用 `next_outgoing` / `next_event` 断言状态机输出，并用传入的 `now` 控制时间。

use sekai::mqtt::client::{Client, ClientConfig, ClientError, Event, PublishOptions, State};
use sekai::mqtt::header::PacketType;
use sekai::mqtt::packet::{Ack, Auth, Connack, Disconnect, Packet, Publish, Suback, Unsuback};
use sekai::mqtt::property::Properties;
use sekai::mqtt::qos::QoS;
use sekai::mqtt::reason::ReasonCode;
use sekai::mqtt::topic::{Subscription, SubscriptionOptions, TopicFilter, TopicName};

/// 统一使用的基准时刻（毫秒）。
const NOW: u64 = 1_000;

// ---- 构造辅助 ----

fn topic(value: &str) -> TopicName {
    TopicName::new(value).expect("测试用主题应当合法")
}

fn filter(value: &str) -> TopicFilter {
    TopicFilter::new(value).expect("测试用过滤器应当合法")
}

fn subscription(filter_value: &str, qos: QoS) -> Subscription {
    Subscription {
        filter: filter(filter_value),
        options: SubscriptionOptions::new(qos, false, false, 0).expect("测试用订阅选项应当合法"),
    }
}

fn publish_options(qos: QoS) -> PublishOptions {
    PublishOptions {
        qos,
        retain: false,
        properties: Properties::new(),
    }
}

fn base_config() -> ClientConfig {
    ClientConfig {
        client_id: String::from("client-1"),
        clean_start: false,
        keep_alive: 60,
        connect_timeout: 1_000,
        session_expiry_interval: Some(300),
        ..ClientConfig::default()
    }
}

fn connack(session_present: bool, properties: Properties) -> Packet {
    Packet::Connack(Connack {
        session_present,
        reason_code: ReasonCode::Success,
        properties,
    })
}

fn refused_connack(reason_code: ReasonCode) -> Packet {
    Packet::Connack(Connack {
        session_present: false,
        reason_code,
        properties: Properties::new(),
    })
}

fn ack(packet_id: u16, reason_code: ReasonCode) -> Ack {
    Ack {
        packet_id,
        reason_code,
        properties: Properties::new(),
    }
}

fn suback_packet(packet_id: u16, reason_codes: Vec<ReasonCode>) -> Packet {
    Packet::Suback(Suback {
        packet_id,
        properties: Properties::new(),
        reason_codes,
    })
}

fn unsuback_packet(packet_id: u16, reason_codes: Vec<ReasonCode>) -> Packet {
    Packet::Unsuback(Unsuback {
        packet_id,
        properties: Properties::new(),
        reason_codes,
    })
}

fn inbound_publish(
    topic_value: Option<&str>,
    qos: QoS,
    packet_id: Option<u16>,
    payload: &[u8],
    properties: Properties,
) -> Packet {
    Packet::Publish(Publish {
        dup: false,
        qos,
        retain: false,
        topic: topic_value.map(topic),
        packet_id,
        properties,
        payload: payload.to_vec(),
    })
}

/// 建立连接并处理成功 CONNACK，吞掉 Connected 事件。
fn connect_with(client: &mut Client, connack_properties: Properties, session_present: bool) {
    client.connect(NOW).expect("发起连接应当成功");
    assert_eq!(client.state(), State::Connecting);
    assert!(matches!(
        client.next_outgoing(NOW),
        Some(Packet::Connect(_))
    ));
    client
        .handle_packet(connack(session_present, connack_properties), NOW)
        .expect("处理成功 CONNACK 应当成功");
    assert_eq!(client.state(), State::Connected);
    assert!(matches!(client.next_event(), Some(Event::Connected(_))));
}

/// 新建客户端并完成一次正常连接。
fn connected(
    config: ClientConfig,
    connack_properties: Properties,
    session_present: bool,
) -> Client {
    let mut client = Client::new(config);
    connect_with(&mut client, connack_properties, session_present);
    client
}

// ---- 出站报文断言辅助 ----

fn expect_publish(packet: Option<Packet>) -> Publish {
    match packet {
        Some(Packet::Publish(publish)) => publish,
        other => panic!("期望 PUBLISH 报文，实际为 {:?}", other),
    }
}

fn expect_puback(packet: Option<Packet>) -> Ack {
    match packet {
        Some(Packet::Puback(packet)) => packet,
        other => panic!("期望 PUBACK 报文，实际为 {:?}", other),
    }
}

fn expect_pubrec(packet: Option<Packet>) -> Ack {
    match packet {
        Some(Packet::Pubrec(packet)) => packet,
        other => panic!("期望 PUBREC 报文，实际为 {:?}", other),
    }
}

fn expect_pubrel(packet: Option<Packet>) -> Ack {
    match packet {
        Some(Packet::Pubrel(packet)) => packet,
        other => panic!("期望 PUBREL 报文，实际为 {:?}", other),
    }
}

fn expect_pubcomp(packet: Option<Packet>) -> Ack {
    match packet {
        Some(Packet::Pubcomp(packet)) => packet,
        other => panic!("期望 PUBCOMP 报文，实际为 {:?}", other),
    }
}

fn expect_event(client: &mut Client) -> Event {
    client.next_event().expect("期望存在事件")
}

// ==== 1. 连接生命周期 ====

#[test]
fn connect_constructs_connect_packet() {
    let mut config = base_config();
    config.keep_alive = 30;
    config.connect_timeout = 500;
    config.session_expiry_interval = Some(120);
    config.receive_maximum = Some(10);
    config.maximum_packet_size = Some(4096);
    config.topic_alias_maximum = Some(4);
    config.request_response_information = Some(true);
    config.request_problem_information = Some(false);
    config.user_properties = vec![(String::from("k"), String::from("v"))];
    config.username = Some(String::from("user"));
    config.password = Some(vec![1, 2, 3]);
    config.authentication_method = Some(String::from("SCRAM-SHA-256"));
    config.authentication_data = Some(vec![9, 8]);
    let mut client = Client::new(config);
    assert_eq!(client.state(), State::Disconnected);
    assert_eq!(client.client_id(), "client-1");
    assert_eq!(client.max_incoming_packet_size(), Some(4096));

    client.connect(100).expect("发起连接应当成功");
    assert_eq!(client.state(), State::Connecting);
    assert_eq!(client.next_deadline(), Some(600));

    match client.next_outgoing(100) {
        Some(Packet::Connect(connect)) => {
            assert_eq!(connect.client_id, "client-1");
            assert!(!connect.clean_start);
            assert_eq!(connect.keep_alive, 30);
            assert_eq!(connect.username.as_deref(), Some("user"));
            assert_eq!(connect.password.as_deref(), Some(&[1u8, 2, 3][..]));
            assert!(connect.will.is_none());
            assert_eq!(connect.properties.session_expiry_interval, Some(120));
            assert_eq!(connect.properties.receive_maximum, Some(10));
            assert_eq!(connect.properties.maximum_packet_size, Some(4096));
            assert_eq!(connect.properties.topic_alias_maximum, Some(4));
            assert_eq!(connect.properties.request_response_information, Some(true));
            assert_eq!(connect.properties.request_problem_information, Some(false));
            assert_eq!(
                connect.properties.user_property,
                vec![(String::from("k"), String::from("v"))]
            );
            assert_eq!(
                connect.properties.authentication_method.as_deref(),
                Some("SCRAM-SHA-256")
            );
            assert_eq!(connect.properties.authentication_data, Some(vec![9, 8]));
        }
        other => panic!("期望 CONNECT 报文，实际为 {:?}", other),
    }
    assert!(client.next_outgoing(100).is_none());
}

#[test]
fn connect_rejects_invalid_config() {
    // Client ID 为空且 Clean Start 为假
    let mut config = base_config();
    config.client_id = String::new();
    assert_eq!(
        Client::new(config).connect(NOW).unwrap_err(),
        ClientError::InvalidConfig("Client ID 为空时必须使用 Clean Start")
    );

    // Receive Maximum 为 0
    let mut config = base_config();
    config.receive_maximum = Some(0);
    assert!(matches!(
        Client::new(config).connect(NOW).unwrap_err(),
        ClientError::InvalidConfig(_)
    ));

    // Maximum Packet Size 为 0
    let mut config = base_config();
    config.maximum_packet_size = Some(0);
    assert!(matches!(
        Client::new(config).connect(NOW).unwrap_err(),
        ClientError::InvalidConfig(_)
    ));

    // 携带 Authentication Data 但没有 Authentication Method
    let mut config = base_config();
    config.authentication_data = Some(vec![1]);
    assert!(matches!(
        Client::new(config).connect(NOW).unwrap_err(),
        ClientError::InvalidConfig(_)
    ));

    // 空 Client ID + Clean Start 合法
    let mut config = base_config();
    config.client_id = String::new();
    config.clean_start = true;
    let mut client = Client::new(config);
    client.connect(NOW).expect("Clean Start 时允许空 Client ID");
    assert_eq!(client.state(), State::Connecting);
}

#[test]
fn connect_twice_is_already_connected() {
    let mut client = Client::new(base_config());
    client.connect(NOW).expect("首次连接应当成功");
    assert_eq!(client.connect(NOW), Err(ClientError::AlreadyConnected));
}

#[test]
fn connack_success_applies_server_properties() {
    let mut properties = Properties::new();
    properties.assigned_client_identifier = Some(String::from("assigned-id"));
    properties.server_keep_alive = Some(5);
    properties.receive_maximum = Some(7);
    properties.maximum_packet_size = Some(2048);
    properties.topic_alias_maximum = Some(3);
    properties.maximum_qos = Some(1);
    properties.retain_available = Some(false);
    properties.wildcard_subscription_available = Some(false);
    properties.subscription_identifier_available = Some(false);
    properties.shared_subscription_available = Some(false);
    properties.session_expiry_interval = Some(77);
    properties.reason_string = Some(String::from("ok"));

    let mut client = Client::new(base_config());
    client.connect(0).expect("发起连接");
    let _ = client.next_outgoing(0);
    client
        .handle_packet(connack(false, properties), 10)
        .expect("处理 CONNACK");
    assert_eq!(client.state(), State::Connected);
    // 服务端分配的 Client ID 优先
    assert_eq!(client.client_id(), "assigned-id");
    // Server Keep Alive = 5 秒：下一次截止时刻为 last_sent(10) + 5000
    assert_eq!(client.next_deadline(), Some(5_010));
    match expect_event(&mut client) {
        Event::Connected(received) => {
            assert_eq!(received.properties.receive_maximum, Some(7));
            assert_eq!(
                received.properties.assigned_client_identifier.as_deref(),
                Some("assigned-id")
            );
        }
        other => panic!("期望 Connected 事件，实际为 {:?}", other),
    }
}

#[test]
fn connack_session_present_is_accepted() {
    let mut client = Client::new(base_config());
    client.connect(NOW).expect("发起连接");
    let _ = client.next_outgoing(NOW);
    client
        .handle_packet(connack(true, Properties::new()), NOW)
        .expect("非 Clean Start 时允许会话存在");
    assert_eq!(client.state(), State::Connected);
    assert!(matches!(client.next_event(), Some(Event::Connected(_))));
}

#[test]
fn connack_refused_emits_event() {
    let mut client = Client::new(base_config());
    client.connect(NOW).expect("发起连接");
    let _ = client.next_outgoing(NOW);
    client
        .handle_packet(refused_connack(ReasonCode::NotAuthorized), NOW)
        .expect("处理拒绝 CONNACK 不返回错误");
    assert_eq!(client.state(), State::Disconnected);
    match expect_event(&mut client) {
        Event::ConnectionRefused(received) => {
            assert_eq!(received.reason_code, ReasonCode::NotAuthorized);
        }
        other => panic!("期望 ConnectionRefused 事件，实际为 {:?}", other),
    }
    assert!(client.next_event().is_none());
}

#[test]
fn clean_start_with_session_present_is_protocol_error() {
    let mut config = base_config();
    config.clean_start = true;
    let mut client = Client::new(config);
    client.connect(NOW).expect("发起连接");
    let _ = client.next_outgoing(NOW);
    assert!(matches!(
        client.handle_packet(connack(true, Properties::new()), NOW),
        Err(ClientError::Protocol(_))
    ));
    assert_eq!(client.state(), State::Disconnected);
}

#[test]
fn connect_timeout_is_reported() {
    let mut config = base_config();
    config.connect_timeout = 1_000;
    let mut client = Client::new(config);
    client.connect(0).expect("发起连接");
    assert_eq!(client.next_deadline(), Some(1_000));
    client.poll(999).expect("未到截止时刻不应超时");
    assert_eq!(client.poll(1_000).unwrap_err(), ClientError::ConnectTimeout);
    assert_eq!(client.state(), State::Disconnected);
    assert_eq!(client.next_deadline(), None);
}

#[test]
fn state_and_unexpected_packet_errors() {
    // 未连接时收到任何报文都是 NotConnected
    let mut client = Client::new(base_config());
    assert_eq!(
        client.handle_packet(Packet::Pingresp, NOW),
        Err(ClientError::NotConnected)
    );

    // Connecting 状态只接受 CONNACK / AUTH / DISCONNECT
    client.connect(NOW).expect("发起连接");
    let _ = client.next_outgoing(NOW);
    assert_eq!(
        client.handle_packet(Packet::Pingresp, NOW),
        Err(ClientError::UnexpectedPacket(PacketType::Pingresp))
    );
    assert_eq!(client.state(), State::Disconnected);

    // 已连接状态再次收到 CONNACK 是协议错误
    let mut client = connected(base_config(), Properties::new(), false);
    assert!(matches!(
        client.handle_packet(connack(false, Properties::new()), NOW),
        Err(ClientError::Protocol(_))
    ));
    assert_eq!(client.state(), State::Disconnected);

    // 已连接状态收到客户端报文是意外报文
    let mut client = connected(base_config(), Properties::new(), false);
    assert_eq!(
        client.handle_packet(Packet::Pingreq, NOW),
        Err(ClientError::UnexpectedPacket(PacketType::Pingreq))
    );
    assert_eq!(client.state(), State::Disconnected);

    // 未连接时调用发布/订阅/断开均返回 NotConnected
    let mut client = Client::new(base_config());
    assert_eq!(
        client.publish(
            topic("a"),
            Vec::new(),
            publish_options(QoS::AtMostOnce),
            NOW
        ),
        Err(ClientError::NotConnected)
    );
    assert_eq!(
        client.subscribe(
            vec![subscription("a", QoS::AtMostOnce)],
            Properties::new(),
            NOW
        ),
        Err(ClientError::NotConnected)
    );
    assert_eq!(
        client.unsubscribe(vec![filter("a")], Properties::new(), NOW),
        Err(ClientError::NotConnected)
    );
    assert_eq!(
        client.disconnect(ReasonCode::Success, Properties::new(), NOW),
        Err(ClientError::NotConnected)
    );
}

// ==== 2. 订阅流程 ====

#[test]
fn subscribe_then_suback_grants_and_auto_resubscribes() {
    let mut client = connected(base_config(), Properties::new(), false);
    let subscriptions = vec![
        subscription("a/b", QoS::AtLeastOnce),
        subscription("c/#", QoS::ExactlyOnce),
    ];
    let packet_id = client
        .subscribe(subscriptions.clone(), Properties::new(), NOW)
        .expect("订阅应当成功");
    assert_eq!(packet_id, 1);
    match client.next_outgoing(NOW) {
        Some(Packet::Subscribe(request)) => {
            assert_eq!(request.packet_id, packet_id);
            assert_eq!(request.subscriptions.len(), 2);
            assert_eq!(request.subscriptions[0].filter.as_str(), "a/b");
            assert_eq!(request.subscriptions[0].options.qos, QoS::AtLeastOnce);
            assert_eq!(request.subscriptions[1].filter.as_str(), "c/#");
            assert_eq!(request.subscriptions[1].options.qos, QoS::ExactlyOnce);
        }
        other => panic!("期望 SUBSCRIBE 报文，实际为 {:?}", other),
    }
    client
        .handle_packet(
            suback_packet(
                packet_id,
                vec![ReasonCode::GrantedQos1, ReasonCode::GrantedQos2],
            ),
            NOW,
        )
        .expect("处理 SUBACK 应当成功");
    assert!(matches!(expect_event(&mut client), Event::Suback(_)));

    // 断开后会话保留；下一次连接会话不存在时自动重订阅全部已授权过滤器
    client.transport_closed(NOW);
    assert_eq!(client.state(), State::Disconnected);
    connect_with(&mut client, Properties::new(), false);
    match client.next_outgoing(NOW) {
        Some(Packet::Subscribe(request)) => {
            assert_eq!(request.packet_id, 2);
            let filters: Vec<&str> = request
                .subscriptions
                .iter()
                .map(|item| item.filter.as_str())
                .collect();
            assert_eq!(filters, vec!["a/b", "c/#"]);
        }
        other => panic!("期望自动重订阅的 SUBSCRIBE 报文，实际为 {:?}", other),
    }
    client
        .handle_packet(
            suback_packet(2, vec![ReasonCode::GrantedQos1, ReasonCode::GrantedQos2]),
            NOW,
        )
        .expect("处理重订阅 SUBACK 应当成功");
    assert!(matches!(expect_event(&mut client), Event::Suback(_)));
}

#[test]
fn suback_failure_is_not_granted() {
    let mut client = connected(base_config(), Properties::new(), false);
    let packet_id = client
        .subscribe(
            vec![subscription("deny/me", QoS::AtLeastOnce)],
            Properties::new(),
            NOW,
        )
        .expect("订阅应当成功");
    let _ = client.next_outgoing(NOW);
    client
        .handle_packet(
            suback_packet(packet_id, vec![ReasonCode::NotAuthorized]),
            NOW,
        )
        .expect("处理失败 SUBACK 应当成功");
    assert!(matches!(expect_event(&mut client), Event::Suback(_)));

    // 失败订阅不应被记为已授权订阅：会话未恢复时不触发自动重订阅
    client.transport_closed(NOW);
    connect_with(&mut client, Properties::new(), false);
    assert!(client.next_outgoing(NOW).is_none());
}

#[test]
fn suback_for_unknown_packet_id_is_error() {
    let mut client = connected(base_config(), Properties::new(), false);
    assert_eq!(
        client.handle_packet(suback_packet(99, vec![ReasonCode::Success]), NOW),
        Err(ClientError::UnknownPacketId(99))
    );
    assert_eq!(client.state(), State::Disconnected);
}

#[test]
fn subscribe_rejects_empty_list() {
    let mut client = connected(base_config(), Properties::new(), false);
    assert!(matches!(
        client.subscribe(Vec::new(), Properties::new(), NOW),
        Err(ClientError::Protocol(_))
    ));
    assert!(matches!(
        client.unsubscribe(Vec::new(), Properties::new(), NOW),
        Err(ClientError::Protocol(_))
    ));
    assert_eq!(client.state(), State::Connected);
}

#[test]
fn unsubscribe_and_unsuback_removes_granted_subscription() {
    let mut client = connected(base_config(), Properties::new(), false);
    let subscribe_id = client
        .subscribe(
            vec![subscription("a/b", QoS::AtLeastOnce)],
            Properties::new(),
            NOW,
        )
        .expect("订阅应当成功");
    let _ = client.next_outgoing(NOW);
    client
        .handle_packet(
            suback_packet(subscribe_id, vec![ReasonCode::GrantedQos1]),
            NOW,
        )
        .expect("处理 SUBACK 应当成功");
    let _ = expect_event(&mut client);

    let unsubscribe_id = client
        .unsubscribe(vec![filter("a/b")], Properties::new(), NOW)
        .expect("取消订阅应当成功");
    match client.next_outgoing(NOW) {
        Some(Packet::Unsubscribe(request)) => {
            assert_eq!(request.packet_id, unsubscribe_id);
            assert_eq!(request.filters.len(), 1);
            assert_eq!(request.filters[0].as_str(), "a/b");
        }
        other => panic!("期望 UNSUBSCRIBE 报文，实际为 {:?}", other),
    }
    client
        .handle_packet(
            unsuback_packet(unsubscribe_id, vec![ReasonCode::Success]),
            NOW,
        )
        .expect("处理 UNSUBACK 应当成功");
    assert!(matches!(expect_event(&mut client), Event::Unsuback(_)));

    // 订阅意图已移除：会话未恢复时不再自动重订阅
    client.transport_closed(NOW);
    connect_with(&mut client, Properties::new(), false);
    assert!(client.next_outgoing(NOW).is_none());
}

#[test]
fn unsuback_for_unknown_packet_id_is_error() {
    let mut client = connected(base_config(), Properties::new(), false);
    assert_eq!(
        client.handle_packet(unsuback_packet(42, vec![ReasonCode::Success]), NOW),
        Err(ClientError::UnknownPacketId(42))
    );
    assert_eq!(client.state(), State::Disconnected);
}

// ==== 3. QoS 出站 ====

#[test]
fn publish_qos0_sends_directly() {
    let mut client = connected(base_config(), Properties::new(), false);
    let result = client
        .publish(
            topic("t/0"),
            b"hello".to_vec(),
            publish_options(QoS::AtMostOnce),
            NOW,
        )
        .expect("QoS0 发布应当成功");
    assert_eq!(result, None);
    let publish = expect_publish(client.next_outgoing(NOW));
    assert_eq!(publish.qos, QoS::AtMostOnce);
    assert_eq!(publish.packet_id, None);
    assert!(!publish.dup);
    assert!(!publish.retain);
    assert_eq!(
        publish.topic.as_ref().map(|item| item.as_str()),
        Some("t/0")
    );
    assert_eq!(publish.payload, b"hello".to_vec());
    assert!(client.next_outgoing(NOW).is_none());
}

#[test]
fn publish_qos1_puback_flow() {
    let mut client = connected(base_config(), Properties::new(), false);
    let packet_id = client
        .publish(
            topic("t/1"),
            b"x".to_vec(),
            publish_options(QoS::AtLeastOnce),
            NOW,
        )
        .expect("QoS1 发布应当成功")
        .expect("QoS1 应分配包标识符");
    assert_eq!(packet_id, 1);
    let publish = expect_publish(client.next_outgoing(NOW));
    assert_eq!(publish.packet_id, Some(packet_id));
    assert_eq!(publish.qos, QoS::AtLeastOnce);

    client
        .handle_packet(Packet::Puback(ack(packet_id, ReasonCode::Success)), NOW)
        .expect("处理 PUBACK 应当成功");
    assert!(matches!(
        expect_event(&mut client),
        Event::Puback(received) if received.packet_id == packet_id
    ));
    assert!(client.next_outgoing(NOW).is_none());
}

#[test]
fn publish_qos1_failure_ack_releases_inflight() {
    let mut client = connected(base_config(), Properties::new(), false);
    let packet_id = client
        .publish(
            topic("t/1"),
            Vec::new(),
            publish_options(QoS::AtLeastOnce),
            NOW,
        )
        .expect("发布应当成功")
        .expect("应分配包标识符");
    let _ = client.next_outgoing(NOW);
    client
        .handle_packet(
            Packet::Puback(ack(packet_id, ReasonCode::UnspecifiedError)),
            NOW,
        )
        .expect("处理失败 PUBACK 应当成功");
    assert!(matches!(
        expect_event(&mut client),
        Event::Puback(received) if received.reason_code == ReasonCode::UnspecifiedError
    ));
    // 已释放：会话恢复时不会再重发这条发布
    client.transport_closed(NOW);
    connect_with(&mut client, Properties::new(), true);
    assert!(client.next_outgoing(NOW).is_none());
}

#[test]
fn publish_qos2_full_flow() {
    let mut client = connected(base_config(), Properties::new(), false);
    let packet_id = client
        .publish(
            topic("t/2"),
            b"qos2".to_vec(),
            publish_options(QoS::ExactlyOnce),
            NOW,
        )
        .expect("QoS2 发布应当成功")
        .expect("QoS2 应分配包标识符");
    assert_eq!(packet_id, 1);
    let publish = expect_publish(client.next_outgoing(NOW));
    assert_eq!(publish.qos, QoS::ExactlyOnce);
    assert_eq!(publish.packet_id, Some(packet_id));

    client
        .handle_packet(Packet::Pubrec(ack(packet_id, ReasonCode::Success)), NOW)
        .expect("处理 PUBREC 应当成功");
    let pubrel = expect_pubrel(client.next_outgoing(NOW));
    assert_eq!(pubrel.packet_id, packet_id);
    assert_eq!(pubrel.reason_code, ReasonCode::Success);
    assert!(matches!(
        expect_event(&mut client),
        Event::Pubrec(received) if received.packet_id == packet_id
    ));

    client
        .handle_packet(Packet::Pubcomp(ack(packet_id, ReasonCode::Success)), NOW)
        .expect("处理 PUBCOMP 应当成功");
    assert!(matches!(
        expect_event(&mut client),
        Event::Pubcomp(received) if received.packet_id == packet_id
    ));
    assert!(client.next_outgoing(NOW).is_none());
}

#[test]
fn publish_qos2_pubrec_failure_skips_pubrel() {
    let mut client = connected(base_config(), Properties::new(), false);
    let packet_id = client
        .publish(
            topic("t/2"),
            Vec::new(),
            publish_options(QoS::ExactlyOnce),
            NOW,
        )
        .expect("发布应当成功")
        .expect("应分配包标识符");
    let _ = client.next_outgoing(NOW);
    client
        .handle_packet(
            Packet::Pubrec(ack(packet_id, ReasonCode::QuotaExceeded)),
            NOW,
        )
        .expect("处理失败 PUBREC 应当成功");
    assert!(matches!(
        expect_event(&mut client),
        Event::Pubrec(received) if received.reason_code == ReasonCode::QuotaExceeded
    ));
    // 失败时不应发送 PUBREL
    assert!(client.next_outgoing(NOW).is_none());
    // 在途记录已释放
    client.transport_closed(NOW);
    connect_with(&mut client, Properties::new(), true);
    assert!(client.next_outgoing(NOW).is_none());
}

#[test]
fn no_retransmit_while_connected() {
    let mut client = connected(base_config(), Properties::new(), false);
    let _ = client
        .publish(
            topic("t/1"),
            Vec::new(),
            publish_options(QoS::AtLeastOnce),
            NOW,
        )
        .expect("发布应当成功");
    let _ = client.next_outgoing(NOW);

    // 到达 Keep Alive 阈值后只发 PINGREQ，不重发 PUBLISH
    client.poll(NOW + 60_000).expect("轮询应当成功");
    assert!(matches!(
        client.next_outgoing(NOW + 60_000),
        Some(Packet::Pingreq)
    ));
    client.poll(NOW + 61_000).expect("轮询应当成功");
    assert!(client.next_outgoing(NOW + 61_000).is_none());
}

#[test]
fn reconnect_resends_inflight_publish_and_pubrel() {
    let mut client = connected(base_config(), Properties::new(), false);
    let qos1_id = client
        .publish(
            topic("q/1"),
            b"a".to_vec(),
            publish_options(QoS::AtLeastOnce),
            NOW,
        )
        .expect("发布应当成功")
        .expect("应分配包标识符");
    let _ = client.next_outgoing(NOW);
    let qos2_id = client
        .publish(
            topic("q/2"),
            b"b".to_vec(),
            publish_options(QoS::ExactlyOnce),
            NOW,
        )
        .expect("发布应当成功")
        .expect("应分配包标识符");
    let _ = client.next_outgoing(NOW);
    client
        .handle_packet(Packet::Pubrec(ack(qos2_id, ReasonCode::Success)), NOW)
        .expect("处理 PUBREC 应当成功");
    let _ = expect_event(&mut client);
    let _ = expect_pubrel(client.next_outgoing(NOW));

    client.transport_closed(NOW);
    // 会话保留：不应有 SessionReset
    assert!(client.next_event().is_none());

    connect_with(&mut client, Properties::new(), true);
    let resend = expect_publish(client.next_outgoing(NOW));
    assert!(resend.dup);
    assert_eq!(resend.packet_id, Some(qos1_id));
    assert_eq!(resend.qos, QoS::AtLeastOnce);
    assert_eq!(resend.topic.as_ref().map(|item| item.as_str()), Some("q/1"));
    let pubrel = expect_pubrel(client.next_outgoing(NOW));
    assert_eq!(pubrel.packet_id, qos2_id);
    assert!(client.next_outgoing(NOW).is_none());
}

#[test]
fn ack_state_mismatches_are_errors() {
    // QoS1 在途收到 PUBREC
    let mut client = connected(base_config(), Properties::new(), false);
    let packet_id = client
        .publish(
            topic("t"),
            Vec::new(),
            publish_options(QoS::AtLeastOnce),
            NOW,
        )
        .unwrap()
        .unwrap();
    let _ = client.next_outgoing(NOW);
    assert!(matches!(
        client.handle_packet(Packet::Pubrec(ack(packet_id, ReasonCode::Success)), NOW),
        Err(ClientError::Protocol(_))
    ));

    // QoS2 在途（等待 PUBREC）收到 PUBACK
    let mut client = connected(base_config(), Properties::new(), false);
    let packet_id = client
        .publish(
            topic("t"),
            Vec::new(),
            publish_options(QoS::ExactlyOnce),
            NOW,
        )
        .unwrap()
        .unwrap();
    let _ = client.next_outgoing(NOW);
    assert!(matches!(
        client.handle_packet(Packet::Puback(ack(packet_id, ReasonCode::Success)), NOW),
        Err(ClientError::Protocol(_))
    ));

    // 未知包标识符
    let mut client = connected(base_config(), Properties::new(), false);
    assert_eq!(
        client.handle_packet(Packet::Puback(ack(999, ReasonCode::Success)), NOW),
        Err(ClientError::UnknownPacketId(999))
    );
    let mut client = connected(base_config(), Properties::new(), false);
    assert_eq!(
        client.handle_packet(Packet::Pubcomp(ack(999, ReasonCode::Success)), NOW),
        Err(ClientError::UnknownPacketId(999))
    );
}

// ==== 4. QoS 入站 ====

#[test]
fn inbound_qos0_emits_publish_event() {
    let mut client = connected(base_config(), Properties::new(), false);
    client
        .handle_packet(
            inbound_publish(
                Some("in/0"),
                QoS::AtMostOnce,
                None,
                b"payload",
                Properties::new(),
            ),
            NOW,
        )
        .expect("处理入站 QoS0 应当成功");
    match expect_event(&mut client) {
        Event::Publish(publish) => {
            assert_eq!(publish.qos, QoS::AtMostOnce);
            assert_eq!(
                publish.topic.as_ref().map(|item| item.as_str()),
                Some("in/0")
            );
            assert_eq!(publish.payload, b"payload".to_vec());
        }
        other => panic!("期望 Publish 事件，实际为 {:?}", other),
    }
    assert!(client.next_outgoing(NOW).is_none());
}

#[test]
fn inbound_qos1_auto_puback() {
    let mut client = connected(base_config(), Properties::new(), false);
    client
        .handle_packet(
            inbound_publish(
                Some("in/1"),
                QoS::AtLeastOnce,
                Some(5),
                b"p",
                Properties::new(),
            ),
            NOW,
        )
        .expect("处理入站 QoS1 应当成功");
    assert!(matches!(expect_event(&mut client), Event::Publish(_)));
    let puback = expect_puback(client.next_outgoing(NOW));
    assert_eq!(puback.packet_id, 5);
    assert_eq!(puback.reason_code, ReasonCode::Success);
}

#[test]
fn inbound_qos2_dedup_and_pubrel() {
    let mut client = connected(base_config(), Properties::new(), false);
    let publish = inbound_publish(
        Some("in/2"),
        QoS::ExactlyOnce,
        Some(7),
        b"p",
        Properties::new(),
    );
    client
        .handle_packet(publish.clone(), NOW)
        .expect("首次入站 QoS2 应当成功");
    assert!(matches!(expect_event(&mut client), Event::Publish(_)));
    let pubrec = expect_pubrec(client.next_outgoing(NOW));
    assert_eq!(pubrec.packet_id, 7);
    assert_eq!(pubrec.reason_code, ReasonCode::Success);

    // 重复的 QoS2 PUBLISH：只重发 PUBREC，不重复投递
    client
        .handle_packet(publish, NOW)
        .expect("重复入站 QoS2 应当成功");
    assert!(client.next_event().is_none());
    let pubrec = expect_pubrec(client.next_outgoing(NOW));
    assert_eq!(pubrec.packet_id, 7);

    // 收到 PUBREL：回 PUBCOMP 并结束入站流程
    client
        .handle_packet(Packet::Pubrel(ack(7, ReasonCode::Success)), NOW)
        .expect("处理 PUBREL 应当成功");
    let pubcomp = expect_pubcomp(client.next_outgoing(NOW));
    assert_eq!(pubcomp.packet_id, 7);
    assert_eq!(pubcomp.reason_code, ReasonCode::Success);

    // 重复 PUBREL：只回 PUBCOMP（包标识符已不存在）
    client
        .handle_packet(Packet::Pubrel(ack(7, ReasonCode::Success)), NOW)
        .expect("处理重复 PUBREL 应当成功");
    let pubcomp = expect_pubcomp(client.next_outgoing(NOW));
    assert_eq!(pubcomp.packet_id, 7);
    assert_eq!(pubcomp.reason_code, ReasonCode::PacketIdentifierNotFound);
    assert!(client.next_event().is_none());
}

#[test]
fn inbound_pubrel_unknown_replies_packet_id_not_found() {
    let mut client = connected(base_config(), Properties::new(), false);
    client
        .handle_packet(Packet::Pubrel(ack(9, ReasonCode::Success)), NOW)
        .expect("处理未知 PUBREL 应当成功（仅回 PUBCOMP）");
    let pubcomp = expect_pubcomp(client.next_outgoing(NOW));
    assert_eq!(pubcomp.packet_id, 9);
    assert_eq!(pubcomp.reason_code, ReasonCode::PacketIdentifierNotFound);
}

// ==== 5. 流控与报文长度 ====

#[test]
fn server_receive_maximum_queues_publishes() {
    let mut properties = Properties::new();
    properties.receive_maximum = Some(1);
    let mut client = connected(base_config(), properties, false);
    let first_id = client
        .publish(
            topic("q/1"),
            b"a".to_vec(),
            publish_options(QoS::AtLeastOnce),
            NOW,
        )
        .expect("发布应当成功")
        .expect("应分配包标识符");
    assert_eq!(first_id, 1);
    let _ = expect_publish(client.next_outgoing(NOW));

    // Receive Maximum=1 已占满：第二条先排队，不发送
    let second_id = client
        .publish(
            topic("q/2"),
            b"b".to_vec(),
            publish_options(QoS::AtLeastOnce),
            NOW,
        )
        .expect("发布应当成功")
        .expect("应分配包标识符");
    assert_eq!(second_id, 2);
    assert!(client.next_outgoing(NOW).is_none());

    // 配额释放后自动发送排队的发布
    client
        .handle_packet(Packet::Puback(ack(first_id, ReasonCode::Success)), NOW)
        .expect("处理 PUBACK 应当成功");
    let publish = expect_publish(client.next_outgoing(NOW));
    assert_eq!(publish.packet_id, Some(second_id));
    assert_eq!(
        publish.topic.as_ref().map(|item| item.as_str()),
        Some("q/2")
    );
}

#[test]
fn client_receive_maximum_limits_inbound_qos2() {
    let mut config = base_config();
    config.receive_maximum = Some(2);
    let mut client = connected(config, Properties::new(), false);
    client
        .handle_packet(
            inbound_publish(
                Some("in/1"),
                QoS::ExactlyOnce,
                Some(1),
                b"a",
                Properties::new(),
            ),
            NOW,
        )
        .expect("第一条入站 QoS2 应当成功");
    client
        .handle_packet(
            inbound_publish(
                Some("in/2"),
                QoS::ExactlyOnce,
                Some(2),
                b"b",
                Properties::new(),
            ),
            NOW,
        )
        .expect("第二条入站 QoS2 应当成功");
    assert_eq!(
        client.handle_packet(
            inbound_publish(
                Some("in/3"),
                QoS::ExactlyOnce,
                Some(3),
                b"c",
                Properties::new()
            ),
            NOW,
        ),
        Err(ClientError::ReceiveMaximumExceeded)
    );
    assert_eq!(client.state(), State::Disconnected);
}

#[test]
fn outbound_maximum_packet_size_is_enforced() {
    let mut properties = Properties::new();
    properties.maximum_packet_size = Some(30);
    let mut client = connected(base_config(), properties, false);
    let error = client
        .publish(
            topic("topic/x"),
            vec![0u8; 100],
            publish_options(QoS::AtMostOnce),
            NOW,
        )
        .unwrap_err();
    match error {
        ClientError::PacketTooLarge { size, max } => {
            assert!(size > 30);
            assert_eq!(max, 30);
        }
        other => panic!("期望 PacketTooLarge，实际为 {:?}", other),
    }
    // 发布被拒不应断开连接
    assert_eq!(client.state(), State::Connected);
}

#[test]
fn inbound_maximum_packet_size_is_enforced() {
    let mut config = base_config();
    config.maximum_packet_size = Some(30);
    let mut client = connected(config, Properties::new(), false);
    let error = client
        .handle_packet(
            inbound_publish(
                Some("topic/x"),
                QoS::AtMostOnce,
                None,
                &[0u8; 100],
                Properties::new(),
            ),
            NOW,
        )
        .unwrap_err();
    assert!(matches!(error, ClientError::PacketTooLarge { .. }));
    assert_eq!(client.state(), State::Disconnected);
}

#[test]
fn control_packets_obey_server_maximum_packet_size() {
    let mut properties = Properties::new();
    properties.maximum_packet_size = Some(10);
    let mut client = connected(base_config(), properties, false);
    assert!(matches!(
        client.subscribe(
            vec![subscription("topic/filter", QoS::AtMostOnce)],
            Properties::new(),
            NOW,
        ),
        Err(ClientError::PacketTooLarge { .. })
    ));
    assert!(matches!(
        client.unsubscribe(vec![filter("topic/filter")], Properties::new(), NOW),
        Err(ClientError::PacketTooLarge { .. })
    ));
    assert_eq!(client.state(), State::Connected);
}

// ==== 6. 主题别名 ====

fn alias_config() -> ClientConfig {
    let mut config = base_config();
    config.use_topic_alias = true;
    config
}

fn alias_connack_properties(maximum: u16) -> Properties {
    let mut properties = Properties::new();
    properties.topic_alias_maximum = Some(maximum);
    properties
}

#[test]
fn outbound_topic_alias_first_full_then_empty_topic() {
    let mut client = connected(alias_config(), alias_connack_properties(2), false);
    let _ = client
        .publish(
            topic("a/b"),
            b"1".to_vec(),
            publish_options(QoS::AtMostOnce),
            NOW,
        )
        .expect("发布应当成功");
    let first = expect_publish(client.next_outgoing(NOW));
    assert_eq!(first.topic.as_ref().map(|item| item.as_str()), Some("a/b"));
    assert_eq!(first.properties.topic_alias, Some(1));

    let _ = client
        .publish(
            topic("a/b"),
            b"2".to_vec(),
            publish_options(QoS::AtMostOnce),
            NOW,
        )
        .expect("发布应当成功");
    let second = expect_publish(client.next_outgoing(NOW));
    assert!(second.topic.is_none());
    assert_eq!(second.properties.topic_alias, Some(1));

    // 新主题分配下一个别名
    let _ = client
        .publish(
            topic("c/d"),
            b"3".to_vec(),
            publish_options(QoS::AtMostOnce),
            NOW,
        )
        .expect("发布应当成功");
    let third = expect_publish(client.next_outgoing(NOW));
    assert_eq!(third.topic.as_ref().map(|item| item.as_str()), Some("c/d"));
    assert_eq!(third.properties.topic_alias, Some(2));
}

#[test]
fn outbound_topic_alias_table_full() {
    let mut client = connected(alias_config(), alias_connack_properties(1), false);
    let _ = client
        .publish(
            topic("a"),
            Vec::new(),
            publish_options(QoS::AtMostOnce),
            NOW,
        )
        .expect("发布应当成功");
    let first = expect_publish(client.next_outgoing(NOW));
    assert_eq!(first.properties.topic_alias, Some(1));

    // 别名表已满：新主题使用完整主题且不带别名
    let _ = client
        .publish(
            topic("b"),
            Vec::new(),
            publish_options(QoS::AtMostOnce),
            NOW,
        )
        .expect("发布应当成功");
    let second = expect_publish(client.next_outgoing(NOW));
    assert_eq!(second.topic.as_ref().map(|item| item.as_str()), Some("b"));
    assert_eq!(second.properties.topic_alias, None);

    // 已登记的主题继续使用空主题 + 别名
    let _ = client
        .publish(
            topic("a"),
            Vec::new(),
            publish_options(QoS::AtMostOnce),
            NOW,
        )
        .expect("发布应当成功");
    let third = expect_publish(client.next_outgoing(NOW));
    assert!(third.topic.is_none());
    assert_eq!(third.properties.topic_alias, Some(1));
}

#[test]
fn outbound_topic_alias_rebuilt_after_reconnect() {
    let mut client = connected(alias_config(), alias_connack_properties(2), false);
    let _ = client
        .publish(
            topic("a/b"),
            Vec::new(),
            publish_options(QoS::AtMostOnce),
            NOW,
        )
        .expect("发布应当成功");
    let _ = expect_publish(client.next_outgoing(NOW));

    client.transport_closed(NOW);
    connect_with(&mut client, alias_connack_properties(2), true);
    let _ = client
        .publish(
            topic("a/b"),
            Vec::new(),
            publish_options(QoS::AtMostOnce),
            NOW,
        )
        .expect("发布应当成功");
    let publish = expect_publish(client.next_outgoing(NOW));
    assert_eq!(
        publish.topic.as_ref().map(|item| item.as_str()),
        Some("a/b")
    );
    assert_eq!(publish.properties.topic_alias, Some(1));
}

#[test]
fn inbound_topic_alias_resolution() {
    let mut config = base_config();
    config.topic_alias_maximum = Some(2);
    let mut client = connected(config, Properties::new(), false);

    let mut properties = Properties::new();
    properties.topic_alias = Some(1);
    client
        .handle_packet(
            inbound_publish(Some("sensor/1"), QoS::AtMostOnce, None, b"a", properties),
            NOW,
        )
        .expect("处理带别名的入站发布应当成功");
    match expect_event(&mut client) {
        Event::Publish(publish) => {
            assert_eq!(
                publish.topic.as_ref().map(|item| item.as_str()),
                Some("sensor/1")
            );
        }
        other => panic!("期望 Publish 事件，实际为 {:?}", other),
    }

    // 空主题 + 已登记别名
    let mut alias_only = Properties::new();
    alias_only.topic_alias = Some(1);
    client
        .handle_packet(
            inbound_publish(None, QoS::AtMostOnce, None, b"b", alias_only),
            NOW,
        )
        .expect("空主题 + 已知别名应当解析成功");
    match expect_event(&mut client) {
        Event::Publish(publish) => {
            assert_eq!(
                publish.topic.as_ref().map(|item| item.as_str()),
                Some("sensor/1")
            );
        }
        other => panic!("期望 Publish 事件，实际为 {:?}", other),
    }
}

#[test]
fn inbound_topic_alias_invalid_cases() {
    // 超过客户端声明上限
    let mut config = base_config();
    config.topic_alias_maximum = Some(2);
    let mut client = connected(config, Properties::new(), false);
    let mut properties = Properties::new();
    properties.topic_alias = Some(3);
    assert_eq!(
        client
            .handle_packet(
                inbound_publish(Some("t"), QoS::AtMostOnce, None, b"", properties),
                NOW,
            )
            .unwrap_err(),
        ClientError::TopicAliasInvalid
    );
    assert_eq!(client.state(), State::Disconnected);

    // 别名 0 非法
    let mut config = base_config();
    config.topic_alias_maximum = Some(2);
    let mut client = connected(config, Properties::new(), false);
    let mut properties = Properties::new();
    properties.topic_alias = Some(0);
    assert_eq!(
        client
            .handle_packet(
                inbound_publish(Some("t"), QoS::AtMostOnce, None, b"", properties),
                NOW,
            )
            .unwrap_err(),
        ClientError::TopicAliasInvalid
    );

    // 空主题 + 未知别名
    let mut config = base_config();
    config.topic_alias_maximum = Some(2);
    let mut client = connected(config, Properties::new(), false);
    let mut properties = Properties::new();
    properties.topic_alias = Some(1);
    assert_eq!(
        client
            .handle_packet(
                inbound_publish(None, QoS::AtMostOnce, None, b"", properties),
                NOW,
            )
            .unwrap_err(),
        ClientError::TopicAliasInvalid
    );

    // 空主题且未携带别名
    let mut client = connected(base_config(), Properties::new(), false);
    assert!(matches!(
        client.handle_packet(
            inbound_publish(None, QoS::AtMostOnce, None, b"", Properties::new()),
            NOW,
        ),
        Err(ClientError::Protocol(_))
    ));

    // 客户端未声明 Topic Alias Maximum 时任何别名都非法
    let mut client = connected(base_config(), Properties::new(), false);
    let mut properties = Properties::new();
    properties.topic_alias = Some(1);
    assert_eq!(
        client
            .handle_packet(
                inbound_publish(Some("t"), QoS::AtMostOnce, None, b"", properties),
                NOW,
            )
            .unwrap_err(),
        ClientError::TopicAliasInvalid
    );
}

#[test]
fn user_supplied_topic_alias_is_rejected() {
    let mut client = connected(base_config(), Properties::new(), false);
    let mut properties = Properties::new();
    properties.topic_alias = Some(1);
    let options = PublishOptions {
        qos: QoS::AtMostOnce,
        retain: false,
        properties,
    };
    assert!(matches!(
        client.publish(topic("t"), Vec::new(), options, NOW),
        Err(ClientError::Protocol(_))
    ));
    // 调用参数错误不应断开连接
    assert_eq!(client.state(), State::Connected);
}

// ==== 7. Keep Alive ====

#[test]
fn keepalive_sends_pingreq_and_clears_on_pingresp() {
    let mut config = base_config();
    config.keep_alive = 10;
    let mut client = connected(config, Properties::new(), false);
    assert_eq!(client.next_deadline(), Some(NOW + 10_000));

    client.poll(NOW + 9_999).expect("轮询应当成功");
    assert!(client.next_outgoing(NOW + 9_999).is_none());

    client.poll(NOW + 10_000).expect("轮询应当成功");
    assert!(matches!(
        client.next_outgoing(NOW + 10_000),
        Some(Packet::Pingreq)
    ));
    assert_eq!(client.next_deadline(), Some(NOW + 20_000));

    client
        .handle_packet(Packet::Pingresp, NOW + 10_001)
        .expect("处理 PINGRESP 应当成功");
    assert_eq!(client.next_deadline(), Some(NOW + 20_000));

    client.poll(NOW + 19_999).expect("轮询应当成功");
    assert!(client.next_outgoing(NOW + 19_999).is_none());
    client.poll(NOW + 20_000).expect("轮询应当成功");
    assert!(matches!(
        client.next_outgoing(NOW + 20_000),
        Some(Packet::Pingreq)
    ));
}

#[test]
fn keepalive_timeout_is_reported() {
    let mut config = base_config();
    config.keep_alive = 10;
    let mut client = connected(config, Properties::new(), false);
    client.poll(NOW + 10_000).expect("轮询应当成功");
    let _ = client.next_outgoing(NOW + 10_000);
    assert_eq!(
        client.poll(NOW + 20_000).unwrap_err(),
        ClientError::KeepAliveTimeout
    );
    assert_eq!(client.state(), State::Disconnected);
}

#[test]
fn keepalive_zero_disables_ping() {
    let mut config = base_config();
    config.keep_alive = 0;
    let mut client = connected(config, Properties::new(), false);
    assert_eq!(client.next_deadline(), None);
    client.poll(NOW + 10_000_000).expect("轮询应当成功");
    assert!(client.next_outgoing(NOW + 10_000_000).is_none());
}

#[test]
fn server_keep_alive_overrides_client_value() {
    let mut properties = Properties::new();
    properties.server_keep_alive = Some(3);
    let mut client = connected(base_config(), properties, false);
    assert_eq!(client.next_deadline(), Some(NOW + 3_000));
    client.poll(NOW + 3_000).expect("轮询应当成功");
    assert!(matches!(
        client.next_outgoing(NOW + 3_000),
        Some(Packet::Pingreq)
    ));
}

// ==== 8. 服务端能力与错误映射 ====

#[test]
fn server_capability_checks() {
    let mut properties = Properties::new();
    properties.maximum_qos = Some(0);
    properties.retain_available = Some(false);
    properties.wildcard_subscription_available = Some(false);
    properties.shared_subscription_available = Some(false);
    properties.subscription_identifier_available = Some(false);
    let mut client = connected(base_config(), properties, false);

    // Maximum QoS = 0：拒绝 QoS1
    assert_eq!(
        client
            .publish(
                topic("t"),
                Vec::new(),
                publish_options(QoS::AtLeastOnce),
                NOW
            )
            .unwrap_err(),
        ClientError::QosNotSupported(QoS::AtLeastOnce)
    );
    // QoS0 仍然允许
    assert_eq!(
        client
            .publish(
                topic("t"),
                Vec::new(),
                publish_options(QoS::AtMostOnce),
                NOW
            )
            .unwrap(),
        None
    );

    // Retain Available = false
    let mut retain_options = publish_options(QoS::AtMostOnce);
    retain_options.retain = true;
    assert_eq!(
        client
            .publish(topic("t"), Vec::new(), retain_options, NOW)
            .unwrap_err(),
        ClientError::RetainNotSupported
    );

    // 通配符订阅
    assert_eq!(
        client
            .subscribe(
                vec![subscription("a/#", QoS::AtMostOnce)],
                Properties::new(),
                NOW,
            )
            .unwrap_err(),
        ClientError::WildcardSubscriptionNotSupported
    );

    // 共享订阅
    assert_eq!(
        client
            .subscribe(
                vec![subscription("$share/group/topic", QoS::AtMostOnce)],
                Properties::new(),
                NOW,
            )
            .unwrap_err(),
        ClientError::SharedSubscriptionNotSupported
    );

    // 订阅标识符
    let mut subscribe_properties = Properties::new();
    subscribe_properties.subscription_identifier = vec![1];
    assert_eq!(
        client
            .subscribe(
                vec![subscription("plain/topic", QoS::AtMostOnce)],
                subscribe_properties,
                NOW,
            )
            .unwrap_err(),
        ClientError::SubscriptionIdentifiersNotSupported
    );

    // 调用参数错误不应断开连接
    assert_eq!(client.state(), State::Connected);
}

#[test]
fn client_error_reason_codes() {
    use sekai::mqtt::error::{DecodeError, EncodeError};

    assert_eq!(
        ClientError::UnexpectedPacket(PacketType::Puback).reason_code(),
        Some(ReasonCode::ProtocolError)
    );
    assert_eq!(
        ClientError::Protocol("x").reason_code(),
        Some(ReasonCode::ProtocolError)
    );
    assert_eq!(
        ClientError::UnknownPacketId(1).reason_code(),
        Some(ReasonCode::PacketIdentifierNotFound)
    );
    assert_eq!(
        ClientError::PacketTooLarge { size: 10, max: 5 }.reason_code(),
        Some(ReasonCode::PacketTooLarge)
    );
    assert_eq!(
        ClientError::QosNotSupported(QoS::ExactlyOnce).reason_code(),
        Some(ReasonCode::QosNotSupported)
    );
    assert_eq!(
        ClientError::RetainNotSupported.reason_code(),
        Some(ReasonCode::RetainNotSupported)
    );
    assert_eq!(
        ClientError::ReceiveMaximumExceeded.reason_code(),
        Some(ReasonCode::ReceiveMaximumExceeded)
    );
    assert_eq!(
        ClientError::TopicAliasInvalid.reason_code(),
        Some(ReasonCode::TopicAliasInvalid)
    );
    assert_eq!(
        ClientError::Decode(DecodeError::Malformed("x")).reason_code(),
        Some(ReasonCode::MalformedPacket)
    );
    assert_eq!(
        ClientError::Decode(DecodeError::ProtocolError("x")).reason_code(),
        Some(ReasonCode::ProtocolError)
    );
    assert_eq!(
        ClientError::Decode(DecodeError::UnsupportedProtocolVersion(4)).reason_code(),
        Some(ReasonCode::UnsupportedProtocolVersion)
    );
    assert_eq!(ClientError::NotConnected.reason_code(), None);
    assert_eq!(
        ClientError::Encode(EncodeError::Invalid("x")).reason_code(),
        None
    );
}

// ==== 9. 会话保留与恢复 ====

#[test]
fn session_retained_after_transport_closed() {
    let mut client = connected(base_config(), Properties::new(), false);
    let packet_id = client
        .publish(
            topic("q"),
            b"p".to_vec(),
            publish_options(QoS::AtLeastOnce),
            NOW,
        )
        .expect("发布应当成功")
        .expect("应分配包标识符");
    let _ = expect_publish(client.next_outgoing(NOW));

    client.transport_closed(NOW);
    assert_eq!(client.state(), State::Disconnected);
    // 会话保留：不应有 SessionReset
    assert!(client.next_event().is_none());

    connect_with(&mut client, Properties::new(), true);
    let publish = expect_publish(client.next_outgoing(NOW));
    assert!(publish.dup);
    assert_eq!(publish.packet_id, Some(packet_id));
}

#[test]
fn session_not_retained_emits_session_reset() {
    let mut config = base_config();
    config.session_expiry_interval = Some(0);
    let mut client = connected(config, Properties::new(), false);
    let packet_id = client
        .publish(
            topic("q"),
            Vec::new(),
            publish_options(QoS::AtLeastOnce),
            NOW,
        )
        .expect("发布应当成功")
        .expect("应分配包标识符");
    let _ = client.next_outgoing(NOW);

    client.transport_closed(NOW);
    assert_eq!(
        client.next_event(),
        Some(Event::SessionReset {
            lost_packet_ids: vec![packet_id]
        })
    );
    // 会话未保留：重连后不重发
    connect_with(&mut client, Properties::new(), false);
    assert!(client.next_outgoing(NOW).is_none());
}

#[test]
fn clean_start_resets_session_immediately() {
    let mut config = base_config();
    config.clean_start = true;
    let mut client = connected(config, Properties::new(), false);
    let packet_id = client
        .publish(
            topic("q"),
            Vec::new(),
            publish_options(QoS::AtLeastOnce),
            NOW,
        )
        .expect("发布应当成功")
        .expect("应分配包标识符");
    let _ = client.next_outgoing(NOW);
    client.transport_closed(NOW);
    assert_eq!(
        client.next_event(),
        Some(Event::SessionReset {
            lost_packet_ids: vec![packet_id]
        })
    );
}

#[test]
fn queued_publish_survives_reconnect() {
    let mut properties = Properties::new();
    properties.receive_maximum = Some(1);
    let mut client = connected(base_config(), properties, false);
    let first_id = client
        .publish(
            topic("q/1"),
            b"a".to_vec(),
            publish_options(QoS::AtLeastOnce),
            NOW,
        )
        .expect("发布应当成功")
        .expect("应分配包标识符");
    let _ = client.next_outgoing(NOW);
    let second_id = client
        .publish(
            topic("q/2"),
            b"b".to_vec(),
            publish_options(QoS::AtLeastOnce),
            NOW,
        )
        .expect("发布应当成功")
        .expect("应分配包标识符");
    assert!(client.next_outgoing(NOW).is_none());

    client.transport_closed(NOW);
    let mut reconnect_properties = Properties::new();
    reconnect_properties.receive_maximum = Some(1);
    connect_with(&mut client, reconnect_properties, true);

    // 在途消息带 DUP 重发；排队的消息仍在等待配额
    let resend = expect_publish(client.next_outgoing(NOW));
    assert!(resend.dup);
    assert_eq!(resend.packet_id, Some(first_id));
    assert!(client.next_outgoing(NOW).is_none());

    // 配额释放后排队的消息发出（首次发送，DUP=0）
    client
        .handle_packet(Packet::Puback(ack(first_id, ReasonCode::Success)), NOW)
        .expect("处理 PUBACK 应当成功");
    let queued = expect_publish(client.next_outgoing(NOW));
    assert!(!queued.dup);
    assert_eq!(queued.packet_id, Some(second_id));
}

#[test]
fn pending_requests_are_cleared_on_transport_closed() {
    let mut client = connected(base_config(), Properties::new(), false);
    let subscribe_id = client
        .subscribe(
            vec![subscription("a/b", QoS::AtMostOnce)],
            Properties::new(),
            NOW,
        )
        .expect("订阅应当成功");
    let _ = client.next_outgoing(NOW);
    client.transport_closed(NOW);
    connect_with(&mut client, Properties::new(), false);
    // 旧请求已清除：其 SUBACK 视为未知包标识符
    assert_eq!(
        client.handle_packet(suback_packet(subscribe_id, vec![ReasonCode::Success]), NOW),
        Err(ClientError::UnknownPacketId(subscribe_id))
    );
}

#[test]
fn client_disconnect_flow() {
    let mut client = connected(base_config(), Properties::new(), false);
    client
        .disconnect(ReasonCode::Success, Properties::new(), NOW)
        .expect("主动断开应当成功");
    assert_eq!(client.state(), State::Disconnecting);
    assert_eq!(client.next_deadline(), None);
    match client.next_outgoing(NOW) {
        Some(Packet::Disconnect(disconnect)) => {
            assert_eq!(disconnect.reason_code, ReasonCode::Success);
        }
        other => panic!("期望 DISCONNECT 报文，实际为 {:?}", other),
    }
    client.transport_closed(NOW);
    assert_eq!(client.state(), State::Disconnected);
    assert!(client.next_event().is_none());

    // CONNECT 的 Session Expiry=0 时，DISCONNECT 不允许改为非 0
    let mut config = base_config();
    config.session_expiry_interval = Some(0);
    let mut client = connected(config, Properties::new(), false);
    let mut properties = Properties::new();
    properties.session_expiry_interval = Some(10);
    assert!(matches!(
        client.disconnect(ReasonCode::Success, properties, NOW),
        Err(ClientError::Protocol(_))
    ));
    assert_eq!(client.state(), State::Connected);

    // Connecting 状态直接放弃本次连接
    let mut client = Client::new(base_config());
    client.connect(NOW).expect("发起连接");
    let _ = client.next_outgoing(NOW);
    client
        .disconnect(ReasonCode::Success, Properties::new(), NOW)
        .expect("连接中放弃应当成功");
    assert_eq!(client.state(), State::Disconnected);
}

#[test]
fn server_disconnect_terminates_connection() {
    // 会话保留：不发 SessionReset
    let mut client = connected(base_config(), Properties::new(), false);
    let disconnect = Disconnect {
        reason_code: ReasonCode::ServerShuttingDown,
        properties: Properties::new(),
    };
    client
        .handle_packet(Packet::Disconnect(disconnect.clone()), NOW)
        .expect("处理服务端 DISCONNECT 应当成功");
    assert_eq!(client.state(), State::Disconnected);
    assert_eq!(client.next_event(), Some(Event::Disconnected(disconnect)));

    // 会话不保留：先发 SessionReset 再发 Disconnected
    let mut config = base_config();
    config.session_expiry_interval = Some(0);
    let mut client = connected(config, Properties::new(), false);
    let packet_id = client
        .publish(
            topic("q"),
            Vec::new(),
            publish_options(QoS::AtLeastOnce),
            NOW,
        )
        .expect("发布应当成功")
        .expect("应分配包标识符");
    let _ = client.next_outgoing(NOW);
    let disconnect = Disconnect {
        reason_code: ReasonCode::ServerShuttingDown,
        properties: Properties::new(),
    };
    client
        .handle_packet(Packet::Disconnect(disconnect.clone()), NOW)
        .expect("处理服务端 DISCONNECT 应当成功");
    assert_eq!(
        client.next_event(),
        Some(Event::SessionReset {
            lost_packet_ids: vec![packet_id]
        })
    );
    assert_eq!(client.next_event(), Some(Event::Disconnected(disconnect)));
}

// ==== 10. 增强认证 AUTH ====

#[test]
fn auth_passthrough() {
    let mut config = base_config();
    config.authentication_method = Some(String::from("SCRAM-SHA-256"));
    config.authentication_data = Some(vec![1, 2]);
    let mut client = Client::new(config);
    client.connect(NOW).expect("发起连接");
    match client.next_outgoing(NOW) {
        Some(Packet::Connect(connect)) => {
            assert_eq!(
                connect.properties.authentication_method.as_deref(),
                Some("SCRAM-SHA-256")
            );
            assert_eq!(connect.properties.authentication_data, Some(vec![1, 2]));
        }
        other => panic!("期望 CONNECT 报文，实际为 {:?}", other),
    }

    // Connecting 状态：AUTH 事件透传
    let incoming = Auth {
        reason_code: ReasonCode::ContinueAuthentication,
        properties: Properties::new(),
    };
    client
        .handle_packet(Packet::Auth(incoming.clone()), NOW)
        .expect("处理 AUTH 应当成功");
    assert_eq!(client.next_event(), Some(Event::Auth(incoming)));

    // 客户端 AUTH 响应
    let response = Auth {
        reason_code: ReasonCode::ContinueAuthentication,
        properties: Properties::new(),
    };
    client
        .auth(response.clone(), NOW)
        .expect("发送 AUTH 应当成功");
    match client.next_outgoing(NOW) {
        Some(Packet::Auth(packet)) => assert_eq!(packet, response),
        other => panic!("期望 AUTH 报文，实际为 {:?}", other),
    }

    // 完成握手后，Connected 状态同样透传
    client
        .handle_packet(connack(false, Properties::new()), NOW)
        .expect("处理 CONNACK 应当成功");
    let _ = expect_event(&mut client);
    let reauth = Auth {
        reason_code: ReasonCode::ReAuthenticate,
        properties: Properties::new(),
    };
    client
        .handle_packet(Packet::Auth(reauth.clone()), NOW)
        .expect("处理 AUTH 应当成功");
    assert_eq!(client.next_event(), Some(Event::Auth(reauth)));

    // 未连接时发送 AUTH 是错误
    let mut client = Client::new(base_config());
    assert_eq!(
        client.auth(
            Auth {
                reason_code: ReasonCode::Success,
                properties: Properties::new(),
            },
            NOW,
        ),
        Err(ClientError::NotConnected)
    );
}

// ==== 11. 包标识符 ====

#[test]
fn packet_id_exhaustion_and_release() {
    let mut properties = Properties::new();
    properties.receive_maximum = Some(1);
    let mut client = connected(base_config(), properties, false);
    let mut last_id = 0;
    for index in 1..=u32::from(u16::MAX) {
        let packet_id = client
            .publish(
                topic("q"),
                Vec::new(),
                publish_options(QoS::AtLeastOnce),
                NOW,
            )
            .expect("包标识符尚未耗尽")
            .expect("QoS1 应分配包标识符");
        assert_eq!(u32::from(packet_id), index);
        last_id = packet_id;
    }
    assert_eq!(last_id, u16::MAX);
    assert_eq!(
        client
            .publish(
                topic("q"),
                Vec::new(),
                publish_options(QoS::AtLeastOnce),
                NOW
            )
            .unwrap_err(),
        ClientError::PacketIdExhausted
    );

    // 释放一个包标识符后可以继续分配
    client
        .handle_packet(Packet::Puback(ack(1, ReasonCode::Success)), NOW)
        .expect("处理 PUBACK 应当成功");
    let _ = expect_event(&mut client);
    let reused = client
        .publish(
            topic("q"),
            Vec::new(),
            publish_options(QoS::AtLeastOnce),
            NOW,
        )
        .expect("释放后应能继续分配")
        .expect("QoS1 应分配包标识符");
    assert_eq!(reused, 1);
}

#[test]
fn packet_ids_are_unique_across_flows() {
    let mut client = connected(base_config(), Properties::new(), false);
    let publish_id = client
        .publish(
            topic("q"),
            Vec::new(),
            publish_options(QoS::AtLeastOnce),
            NOW,
        )
        .expect("发布应当成功")
        .expect("应分配包标识符");
    let subscribe_id = client
        .subscribe(
            vec![subscription("a/b", QoS::AtMostOnce)],
            Properties::new(),
            NOW,
        )
        .expect("订阅应当成功");
    let unsubscribe_id = client
        .unsubscribe(vec![filter("a/b")], Properties::new(), NOW)
        .expect("取消订阅应当成功");
    assert_ne!(publish_id, subscribe_id);
    assert_ne!(publish_id, unsubscribe_id);
    assert_ne!(subscribe_id, unsubscribe_id);

    // 各自完成后释放
    client
        .handle_packet(Packet::Puback(ack(publish_id, ReasonCode::Success)), NOW)
        .expect("处理 PUBACK 应当成功");
    client
        .handle_packet(suback_packet(subscribe_id, vec![ReasonCode::Success]), NOW)
        .expect("处理 SUBACK 应当成功");
    client
        .handle_packet(
            unsuback_packet(unsubscribe_id, vec![ReasonCode::Success]),
            NOW,
        )
        .expect("处理 UNSUBACK 应当成功");
    assert_eq!(client.state(), State::Connected);
}
