//! 内存 MQTT 5 broker：把多个 `sekai::mqtt::client::Client` 端点桥接在一起。
//!
//! 只实现测试需要的子集：
//!
//! - CONNECT → CONNACK（session_present=false，声明 Receive Maximum /
//!   Maximum Packet Size / Retain Available / Maximum QoS / 通配符订阅能力）；
//! - SUBSCRIBE → SUBACK（QoS1 granted），并回放匹配的 retained 消息；
//! - PUBLISH QoS1 路由（按通配符匹配所有订阅者）+ 给发布者 PUBACK；
//! - retained 消息存储（空载荷删除）；
//! - PINGREQ → PINGRESP；DISCONNECT；UNSUBSCRIBE → UNSUBACK；
//! - 记录所有 PUBLISH / CONNECT / SUBSCRIBE / CONNACK 供测试断言；
//! - 模拟故障：断开连接、触发遗嘱、静默某端点的出站消息、重放历史发布。
//!
//! 不实现 TLS、QoS2、持久会话、ACL；这些不是本层测试目标。

use std::collections::{HashMap, VecDeque};

use sekai::mqtt::packet::{
    Ack, Connack, Connect, Disconnect, Packet, Publish, Suback, Subscribe, Unsuback, Unsubscribe,
    Will,
};
use sekai::mqtt::property::Properties;
use sekai::mqtt::qos::QoS;
use sekai::mqtt::reason::ReasonCode;
use sekai::mqtt::topic::{Subscription, TopicName};
use sekai::sekai_stdin::Stdin;
use sekai::sekai_stdout::Stdout;

/// broker 中的一个连接端点标识（按加入顺序分配）。
pub type ClientId = usize;

/// 约定：服务端（ControlPlane）恒为 0 号端点。
pub const SERVER_CLIENT: ClientId = 0;

/// broker 声明的 Receive Maximum。
pub const BROKER_RECEIVE_MAXIMUM: u16 = 8;
/// broker 声明的 Maximum Packet Size。
pub const BROKER_MAXIMUM_PACKET_SIZE: u32 = 16 * 1024;

/// 一条经过 broker 的 PUBLISH 记录（路由前保存）。
#[derive(Debug, Clone)]
pub struct RecordedPublish {
    /// 发布该消息的端点。
    pub from: ClientId,
    /// 主题。
    pub topic: String,
    /// QoS。
    pub qos: QoS,
    /// RETAIN 标志。
    pub retain: bool,
    /// DUP 标志。
    pub dup: bool,
    /// 包标识符（QoS0 为 None）。
    pub packet_id: Option<u16>,
    /// MQTT 属性（含 content_type）。
    pub properties: Properties,
    /// 原始载荷。
    pub payload: Vec<u8>,
    /// 实际送达的端点列表。
    pub delivered_to: Vec<ClientId>,
}

impl RecordedPublish {
    /// content_type 属性。
    pub fn content_type(&self) -> Option<&str> {
        self.properties.content_type.as_deref()
    }

    /// 尝试按 Stdin 解码载荷。
    pub fn decode_stdin(&self) -> Option<Stdin> {
        Stdin::decode(&self.payload).ok()
    }

    /// 尝试按 Stdout 解码载荷。
    pub fn decode_stdout(&self) -> Option<Stdout> {
        Stdout::decode(&self.payload).ok()
    }
}

/// 兼容别名。
pub type PublishRecord = RecordedPublish;

/// CONNECT 记录。
#[derive(Debug, Clone)]
pub struct ConnectRecord {
    /// 端点。
    pub client: ClientId,
    /// Client ID。
    pub client_id: String,
    /// Clean Start。
    pub clean_start: bool,
    /// Keep Alive。
    pub keep_alive: u16,
    /// Session Expiry Interval 属性。
    pub session_expiry_interval: Option<u32>,
    /// Receive Maximum 属性。
    pub receive_maximum: Option<u16>,
    /// Maximum Packet Size 属性。
    pub maximum_packet_size: Option<u32>,
    /// 遗嘱。
    pub will: Option<Will>,
    /// 用户名（不记录密码）。
    pub username: Option<String>,
}

/// SUBSCRIBE 记录。
#[derive(Debug, Clone)]
pub struct SubscribeRecord {
    /// 端点。
    pub client: ClientId,
    /// 包标识符。
    pub packet_id: u16,
    /// 订阅项（保持线上顺序）。
    pub subscriptions: Vec<Subscription>,
}

/// CONNACK 记录。
#[derive(Debug, Clone)]
pub struct ConnackRecord {
    /// 端点。
    pub client: ClientId,
    /// 发出的 CONNACK。
    pub connack: Connack,
}

/// broker 错误（测试用：出现即视为测试 bug）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BrokerError {
    /// 未知端点。
    UnknownClient(ClientId),
    /// 端点尚未 CONNECT。
    NotConnected(ClientId),
    /// 不支持的报文/状态。
    Protocol(String),
}

impl std::fmt::Display for BrokerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownClient(client) => write!(f, "未知 broker 端点 {client}"),
            Self::NotConnected(client) => write!(f, "端点 {client} 尚未 CONNECT"),
            Self::Protocol(message) => write!(f, "broker 协议错误：{message}"),
        }
    }
}

impl std::error::Error for BrokerError {}

struct BrokerClient {
    connected: bool,
    subscriptions: Vec<Subscription>,
    outbox: VecDeque<Packet>,
    will: Option<Will>,
    /// 静默：丢弃该端点发出的应用报文（模拟Worker卡死/失联）。
    silenced: bool,
}

/// 内存 MQTT 5 broker。
/// retained 消息（含最初发布者，便于记录回放来源）。
#[derive(Debug, Clone)]
struct RetainedMessage {
    publish: Publish,
    from: ClientId,
}

pub struct MemoryBroker {
    clients: Vec<BrokerClient>,
    retained: HashMap<String, RetainedMessage>,
    publishes: Vec<RecordedPublish>,
    connects: Vec<ConnectRecord>,
    subscribes: Vec<SubscribeRecord>,
    connacks: Vec<ConnackRecord>,
    next_packet_id: u16,
}

impl Default for MemoryBroker {
    fn default() -> Self {
        Self::new()
    }
}

impl MemoryBroker {
    /// 创建空 broker（尚无端点）。
    pub fn new() -> Self {
        Self {
            clients: Vec::new(),
            retained: HashMap::new(),
            publishes: Vec::new(),
            connects: Vec::new(),
            subscribes: Vec::new(),
            connacks: Vec::new(),
            next_packet_id: 1,
        }
    }

    /// 注册一个端点（初始未连接），返回端点 id。
    pub fn add_client(&mut self) -> ClientId {
        let id = self.clients.len();
        self.clients.push(BrokerClient {
            connected: false,
            subscriptions: Vec::new(),
            outbox: VecDeque::new(),
            will: None,
            silenced: false,
        });
        id
    }

    /// 端点数量。
    pub fn client_count(&self) -> usize {
        self.clients.len()
    }

    /// 端点是否已连接。
    pub fn is_connected(&self, client: ClientId) -> bool {
        self.clients
            .get(client)
            .map(|entry| entry.connected)
            .unwrap_or(false)
    }

    /// 取走一条待投递给该端点的报文。
    pub fn take_delivery(&mut self, client: ClientId) -> Option<Packet> {
        self.clients.get_mut(client)?.outbox.pop_front()
    }

    /// 该端点待投递报文数。
    pub fn pending_deliveries(&self, client: ClientId) -> usize {
        self.clients
            .get(client)
            .map(|entry| entry.outbox.len())
            .unwrap_or(0)
    }

    /// 是否有任何待投递报文。
    pub fn has_pending(&self) -> bool {
        self.clients.iter().any(|entry| !entry.outbox.is_empty())
    }

    /// 所有 PUBLISH 记录（按经过顺序）。
    pub fn publishes(&self) -> &[RecordedPublish] {
        &self.publishes
    }

    /// 最近一条 PUBLISH 记录。
    pub fn last_publish(&self) -> Option<&RecordedPublish> {
        self.publishes.last()
    }

    /// CONNECT 记录。
    pub fn connects(&self) -> &[ConnectRecord] {
        &self.connects
    }

    /// SUBSCRIBE 记录。
    pub fn subscribes(&self) -> &[SubscribeRecord] {
        &self.subscribes
    }

    /// CONNACK 记录。
    pub fn connacks(&self) -> &[ConnackRecord] {
        &self.connacks
    }

    /// retained 消息。
    pub fn retained(&self, topic: &str) -> Option<&Publish> {
        self.retained.get(topic).map(|message| &message.publish)
    }

    /// retained 主题数量。
    pub fn retained_count(&self) -> usize {
        self.retained.len()
    }

    /// 端点的遗嘱。
    pub fn will(&self, client: ClientId) -> Option<&Will> {
        self.clients.get(client)?.will.as_ref()
    }

    /// 设置端点静默：静默后该端点发出的应用报文被静默丢弃（保留连接）。
    pub fn set_silenced(&mut self, client: ClientId, silenced: bool) -> Result<(), BrokerError> {
        self.client_mut(client)?.silenced = silenced;
        Ok(())
    }

    /// 端点是否处于静默。
    pub fn is_silenced(&self, client: ClientId) -> bool {
        self.clients
            .get(client)
            .map(|entry| entry.silenced)
            .unwrap_or(false)
    }

    /// 模拟传输断开：清空连接与订阅；`trigger_will` 为真时发布遗嘱。
    pub fn disconnect_client(
        &mut self,
        client: ClientId,
        trigger_will: bool,
    ) -> Result<(), BrokerError> {
        let will = {
            let entry = self.client_mut(client)?;
            let was_connected = entry.connected;
            entry.connected = false;
            entry.subscriptions.clear();
            entry.outbox.clear();
            entry.silenced = false;
            if was_connected {
                entry.will.take()
            } else {
                None
            }
        };
        if trigger_will && let Some(will) = will {
            let publish = Publish {
                dup: false,
                qos: will.qos,
                retain: will.retain,
                topic: Some(will.topic),
                packet_id: None,
                properties: will.properties,
                payload: will.payload,
            };
            self.route(client, publish);
        }
        Ok(())
    }

    /// 模拟传输断开（不触发遗嘱）。
    pub fn reset_client(&mut self, client: ClientId) -> Result<(), BrokerError> {
        self.disconnect_client(client, false)
    }

    /// 直接注入一条 PUBLISH（模拟Worker/外部发布），返回记录索引。
    pub fn inject_publish(
        &mut self,
        from: ClientId,
        topic: &str,
        payload: Vec<u8>,
        retain: bool,
        content_type: Option<&str>,
    ) -> Result<usize, BrokerError> {
        self.check_client(from)?;
        let mut properties = Properties::new();
        properties.content_type = content_type.map(String::from);
        let publish = Publish {
            dup: false,
            qos: QoS::AtLeastOnce,
            retain,
            topic: Some(
                TopicName::new(topic)
                    .map_err(|error| BrokerError::Protocol(String::from(error)))?,
            ),
            packet_id: None,
            properties,
            payload,
        };
        let index = self.publishes.len();
        self.route(from, publish);
        Ok(index)
    }

    /// 重放一条历史 PUBLISH 记录（模拟同一帧的重复投递），返回新记录索引。
    pub fn replay_publish(&mut self, index: usize) -> Result<usize, BrokerError> {
        let Some(record) = self.publishes.get(index).cloned() else {
            return Err(BrokerError::Protocol(format!("没有第 {index} 条发布记录")));
        };
        let mut publish = Publish {
            dup: true,
            qos: record.qos,
            retain: record.retain,
            topic: Some(
                TopicName::new(record.topic.clone())
                    .map_err(|error| BrokerError::Protocol(String::from(error)))?,
            ),
            packet_id: None,
            properties: record.properties.clone(),
            payload: record.payload.clone(),
        };
        if publish.qos != QoS::AtMostOnce {
            publish.packet_id = Some(self.allocate_packet_id());
        }
        let new_index = self.publishes.len();
        self.route(record.from, publish);
        Ok(new_index)
    }

    /// 处理一条来自端点的报文。
    pub fn submit(&mut self, client: ClientId, packet: Packet) -> Result<(), BrokerError> {
        if matches!(packet, Packet::Connect(_)) {
            let Packet::Connect(connect) = packet else {
                unreachable!();
            };
            return self.handle_connect(client, connect);
        }
        self.check_client(client)?;
        if !self
            .clients
            .get(client)
            .map(|entry| entry.connected)
            .unwrap_or(false)
        {
            return Err(BrokerError::NotConnected(client));
        }
        if self.is_silenced(client) {
            // 静默端点：丢弃其出站应用报文；控制类报文仍处理。
            match packet {
                Packet::Disconnect(disconnect) => {
                    return self.handle_disconnect(client, disconnect);
                }
                Packet::Pingreq => {
                    self.push(client, Packet::Pingresp);
                    return Ok(());
                }
                _ => return Ok(()),
            }
        }
        match packet {
            Packet::Publish(publish) => self.handle_publish(client, publish),
            Packet::Subscribe(subscribe) => self.handle_subscribe(client, subscribe),
            Packet::Unsubscribe(unsubscribe) => self.handle_unsubscribe(client, unsubscribe),
            Packet::Puback(_)
            | Packet::Pubrec(_)
            | Packet::Pubrel(_)
            | Packet::Pubcomp(_)
            | Packet::Auth(_) => Ok(()),
            Packet::Pingreq => {
                self.push(client, Packet::Pingresp);
                Ok(())
            }
            Packet::Pingresp => Ok(()),
            Packet::Disconnect(disconnect) => self.handle_disconnect(client, disconnect),
            other => Err(BrokerError::Protocol(format!(
                "broker 不支持客户端报文 {}",
                other.packet_type()
            ))),
        }
    }

    // ---- 内部 ----

    fn check_client(&self, client: ClientId) -> Result<(), BrokerError> {
        if client < self.clients.len() {
            Ok(())
        } else {
            Err(BrokerError::UnknownClient(client))
        }
    }

    fn client_mut(&mut self, client: ClientId) -> Result<&mut BrokerClient, BrokerError> {
        self.clients
            .get_mut(client)
            .ok_or(BrokerError::UnknownClient(client))
    }

    fn push(&mut self, client: ClientId, packet: Packet) {
        if let Some(entry) = self.clients.get_mut(client) {
            entry.outbox.push_back(packet);
        }
    }

    fn allocate_packet_id(&mut self) -> u16 {
        let mut candidate = self.next_packet_id;
        loop {
            let id = candidate;
            candidate = if candidate == u16::MAX {
                1
            } else {
                candidate + 1
            };
            if id != 0 {
                self.next_packet_id = candidate;
                return id;
            }
        }
    }

    fn handle_connect(&mut self, client: ClientId, connect: Connect) -> Result<(), BrokerError> {
        self.check_client(client)?;
        self.connects.push(ConnectRecord {
            client,
            client_id: connect.client_id.clone(),
            clean_start: connect.clean_start,
            keep_alive: connect.keep_alive,
            session_expiry_interval: connect.properties.session_expiry_interval,
            receive_maximum: connect.properties.receive_maximum,
            maximum_packet_size: connect.properties.maximum_packet_size,
            will: connect.will.clone(),
            username: connect.username.clone(),
        });
        let entry = self.client_mut(client)?;
        entry.connected = true;
        entry.subscriptions.clear();
        entry.outbox.clear();
        entry.silenced = false;
        entry.will = connect.will;

        let mut properties = Properties::new();
        properties.receive_maximum = Some(BROKER_RECEIVE_MAXIMUM);
        properties.maximum_packet_size = Some(BROKER_MAXIMUM_PACKET_SIZE);
        properties.retain_available = Some(true);
        properties.wildcard_subscription_available = Some(true);
        properties.topic_alias_maximum = Some(0);
        properties.maximum_qos = Some(1);
        let connack = Connack {
            session_present: false,
            reason_code: ReasonCode::Success,
            properties,
        };
        self.connacks.push(ConnackRecord {
            client,
            connack: connack.clone(),
        });
        self.push(client, Packet::Connack(connack));
        Ok(())
    }

    fn handle_subscribe(
        &mut self,
        client: ClientId,
        subscribe: Subscribe,
    ) -> Result<(), BrokerError> {
        let mut reason_codes = Vec::with_capacity(subscribe.subscriptions.len());
        for subscription in &subscribe.subscriptions {
            let granted = if subscription.options.qos >= QoS::AtLeastOnce {
                QoS::AtLeastOnce
            } else {
                QoS::AtMostOnce
            };
            reason_codes.push(match granted {
                QoS::AtMostOnce => ReasonCode::Success,
                _ => ReasonCode::GrantedQos1,
            });
            let entry = self.client_mut(client)?;
            entry
                .subscriptions
                .retain(|existing| existing.filter.as_str() != subscription.filter.as_str());
            entry.subscriptions.push(subscription.clone());
        }
        self.subscribes.push(SubscribeRecord {
            client,
            packet_id: subscribe.packet_id,
            subscriptions: subscribe.subscriptions.clone(),
        });
        self.push(
            client,
            Packet::Suback(Suback {
                packet_id: subscribe.packet_id,
                properties: Properties::new(),
                reason_codes,
            }),
        );

        // retained 回放（retain_handling=2 时不回放）。
        let mut replays: Vec<(String, Publish, QoS, ClientId)> = Vec::new();
        for subscription in &subscribe.subscriptions {
            if subscription.options.retain_handling == 2 {
                continue;
            }
            let mut matched: Vec<(String, Publish, ClientId)> = self
                .retained
                .iter()
                .filter(|(topic, _)| topic_matches_filter(subscription.filter.as_str(), topic))
                .map(|(topic, message)| (topic.clone(), message.publish.clone(), message.from))
                .collect();
            matched.sort_by(|left, right| left.0.cmp(&right.0));
            for (topic, publish, from) in matched {
                let qos = std::cmp::min(publish.qos, subscription.options.qos);
                replays.push((topic, publish, qos, from));
            }
        }
        for (topic, publish, qos, from) in replays {
            let mut forwarded = publish;
            forwarded.qos = qos;
            forwarded.retain = true;
            forwarded.dup = false;
            forwarded.packet_id = if qos == QoS::AtMostOnce {
                None
            } else {
                Some(self.allocate_packet_id())
            };
            self.publishes.push(RecordedPublish {
                from,
                topic,
                qos: forwarded.qos,
                retain: true,
                dup: false,
                packet_id: forwarded.packet_id,
                properties: forwarded.properties.clone(),
                payload: forwarded.payload.clone(),
                delivered_to: vec![client],
            });
            self.push(client, Packet::Publish(forwarded));
        }
        Ok(())
    }

    fn handle_unsubscribe(
        &mut self,
        client: ClientId,
        unsubscribe: Unsubscribe,
    ) -> Result<(), BrokerError> {
        let filters: Vec<String> = unsubscribe
            .filters
            .iter()
            .map(|filter| String::from(filter.as_str()))
            .collect();
        let entry = self.client_mut(client)?;
        entry.subscriptions.retain(|existing| {
            !filters
                .iter()
                .any(|filter| filter == existing.filter.as_str())
        });
        self.push(
            client,
            Packet::Unsuback(Unsuback {
                packet_id: unsubscribe.packet_id,
                properties: Properties::new(),
                reason_codes: vec![ReasonCode::Success; filters.len()],
            }),
        );
        Ok(())
    }

    fn handle_publish(&mut self, client: ClientId, publish: Publish) -> Result<(), BrokerError> {
        let qos = publish.qos;
        let packet_id = publish.packet_id;
        self.route(client, publish);
        if qos != QoS::AtMostOnce
            && let Some(packet_id) = packet_id
        {
            self.push(
                client,
                Packet::Puback(Ack {
                    packet_id,
                    reason_code: ReasonCode::Success,
                    properties: Properties::new(),
                }),
            );
        }
        Ok(())
    }

    fn handle_disconnect(
        &mut self,
        client: ClientId,
        _disconnect: Disconnect,
    ) -> Result<(), BrokerError> {
        let entry = self.client_mut(client)?;
        entry.connected = false;
        entry.subscriptions.clear();
        entry.outbox.clear();
        entry.will = None;
        entry.silenced = false;
        Ok(())
    }

    /// 存储 retained（空载荷 = 删除）并路由给所有匹配订阅者。
    fn route(&mut self, from: ClientId, publish: Publish) {
        let Some(topic) = publish
            .topic
            .as_ref()
            .map(|topic| String::from(topic.as_str()))
        else {
            return;
        };
        if publish.retain {
            if publish.payload.is_empty() {
                self.retained.remove(&topic);
            } else {
                self.retained.insert(
                    topic.clone(),
                    RetainedMessage {
                        publish: publish.clone(),
                        from,
                    },
                );
            }
        }

        let mut deliveries: Vec<(ClientId, QoS)> = Vec::new();
        for (index, entry) in self.clients.iter().enumerate() {
            if !entry.connected {
                continue;
            }
            let mut granted: Option<QoS> = None;
            for subscription in &entry.subscriptions {
                if subscription.options.no_local && index == from {
                    continue;
                }
                if !topic_matches_filter(subscription.filter.as_str(), &topic) {
                    continue;
                }
                let qos = std::cmp::min(publish.qos, subscription.options.qos);
                granted = Some(match granted {
                    Some(current) if current >= qos => current,
                    _ => qos,
                });
            }
            if let Some(qos) = granted {
                deliveries.push((index, qos));
            }
        }

        let mut delivered_to = Vec::with_capacity(deliveries.len());
        for (index, qos) in deliveries {
            let mut forwarded = publish.clone();
            forwarded.retain = false;
            forwarded.dup = false;
            forwarded.qos = qos;
            forwarded.packet_id = if qos == QoS::AtMostOnce {
                None
            } else {
                Some(self.allocate_packet_id())
            };
            self.push(index, Packet::Publish(forwarded));
            delivered_to.push(index);
        }

        self.publishes.push(RecordedPublish {
            from,
            topic,
            qos: publish.qos,
            retain: publish.retain,
            dup: publish.dup,
            packet_id: publish.packet_id,
            properties: publish.properties,
            payload: publish.payload,
            delivered_to,
        });
    }
}

/// MQTT 主题过滤器匹配（支持 `+` / `#` 与 `$share/{name}/` 前缀）。
pub fn topic_matches_filter(filter: &str, topic: &str) -> bool {
    let filter = match filter.strip_prefix("$share/") {
        Some(rest) => match rest.split_once('/') {
            Some((_name, filter)) => filter,
            None => return false,
        },
        None => filter,
    };
    let mut filter_levels = filter.split('/');
    let mut topic_levels = topic.split('/');
    loop {
        match (filter_levels.next(), topic_levels.next()) {
            (Some("#"), _) => return true,
            (Some("+"), None) => return false,
            (Some("+"), Some(_)) => {}
            (Some(expected), Some(actual)) => {
                if expected != actual {
                    return false;
                }
            }
            (Some(_), None) => return false,
            (None, Some(_)) => return false,
            (None, None) => return true,
        }
    }
}
