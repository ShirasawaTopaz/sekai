//! MQTT 5.0 客户端会话状态机。
//!
//! 本模块不做 I/O，也不读取时钟：所有超时判定都使用调用方传入的 `now`
//! （Unix 毫秒）。调用方负责：
//!
//! 1. 用 [`Client::connect`] 发起连接，把 [`Client::next_outgoing`] 取出的报文写入传输层；
//! 2. 把收到的报文交给 [`Client::handle_packet`]；
//! 3. 定时调用 [`Client::poll`]（可参考 [`Client::next_deadline`] 决定睡眠时长）；
//! 4. 传输断开后调用 [`Client::transport_closed`]。
//!
//! 报文层类型统一使用 `crate::mqtt::packet` 等模块路径，不依赖 `mqtt/mod.rs` 的 re-export。

mod config;
mod error;
mod event;
mod session;

pub use config::{ClientConfig, PublishOptions};
pub use error::ClientError;
pub use event::Event;

use alloc::collections::VecDeque;
use alloc::string::String;
use alloc::vec::Vec;

use crate::mqtt::error::EncodeError;
use crate::mqtt::packet::{
    Ack, Auth, Connack, Connect, Disconnect, Packet, Publish, Suback, Subscribe, Unsuback,
    Unsubscribe,
};
use crate::mqtt::property::Properties;
use crate::mqtt::qos::QoS;
use crate::mqtt::reason::ReasonCode;
use crate::mqtt::topic::{Subscription, TopicFilter, TopicName};

use session::{
    ConnectionState, Inflight, InflightState, PendingSubscribe, PendingUnsubscribe, QueuedPublish,
    ServerCapabilities, SessionState,
};

/// 客户端状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// 未连接。
    Disconnected,
    /// 已发出 CONNECT，等待 CONNACK。
    Connecting,
    /// 连接已建立，可以收发报文。
    Connected,
    /// 已发出 DISCONNECT，等待传输层关闭。
    Disconnecting,
}

/// MQTT 5.0 客户端会话状态机。
pub struct Client {
    /// 构造时传入的配置。
    config: ClientConfig,
    /// 当前状态。
    state: State,
    /// 会话级状态（可能跨网络连接保留）。
    session: SessionState,
    /// 连接级状态（每次网络连接重建时清空）。
    conn: ConnectionState,
    /// 待分发事件队列。
    events: VecDeque<Event>,
    /// 服务端在 CONNACK 中分配的 Client ID。
    assigned_client_id: Option<String>,
    /// 当前生效的会话过期时间（秒）；CONNACK / DISCONNECT 可覆盖配置值。
    session_expiry: Option<u32>,
}

impl Client {
    /// 用给定配置创建客户端；此时状态为 [`State::Disconnected`]。
    pub fn new(config: ClientConfig) -> Self {
        let keep_alive_ms = u64::from(config.keep_alive) * 1000;
        let session_expiry = config.session_expiry_interval;
        Self {
            config,
            state: State::Disconnected,
            session: SessionState::default(),
            conn: ConnectionState {
                keep_alive_ms,
                ..ConnectionState::default()
            },
            events: VecDeque::new(),
            assigned_client_id: None,
            session_expiry,
        }
    }

    /// 当前状态。
    pub fn state(&self) -> State {
        self.state
    }

    /// 服务端分配的 Client ID 优先，否则返回配置中的 Client ID。
    pub fn client_id(&self) -> &str {
        match &self.assigned_client_id {
            Some(client_id) => client_id.as_str(),
            None => self.config.client_id.as_str(),
        }
    }

    /// 供传输层使用的入站最大包长（客户端在 CONNECT 中广告的 Maximum Packet Size）。
    pub fn max_incoming_packet_size(&self) -> Option<u32> {
        self.config.maximum_packet_size
    }

    /// 发起连接：校验配置、构造 CONNECT 并入队，状态转为 [`State::Connecting`]。
    ///
    /// 若本次使用 Clean Start（必然丢弃旧会话）且本地仍有在途出站消息，
    /// 会立即发出 [`Event::SessionReset`]。
    pub fn connect(&mut self, now: u64) -> Result<(), ClientError> {
        if self.state != State::Disconnected {
            return Err(ClientError::AlreadyConnected);
        }
        self.validate_config()?;
        self.session_expiry = self.config.session_expiry_interval;
        if self.config.clean_start {
            // Clean Start 时服务端必然丢弃旧会话，本地也立即丢弃并通知调用方。
            let lost_packet_ids = self.session.reset();
            if !lost_packet_ids.is_empty() {
                self.events
                    .push_back(Event::SessionReset { lost_packet_ids });
            }
        }
        let connect = self.build_connect();
        self.conn.outgoing.push_back(Packet::Connect(connect));
        self.conn.keep_alive_ms = u64::from(self.config.keep_alive) * 1000;
        self.conn.connect_deadline = now.saturating_add(self.config.connect_timeout);
        self.conn.last_sent = now;
        self.conn.ping_deadline = None;
        self.assigned_client_id = None;
        self.state = State::Connecting;
        Ok(())
    }

    /// 处理一条来自服务端的报文。
    ///
    /// 返回 [`Err`] 表示协议错误或状态错误；其中协议错误会顺带把状态转为
    /// [`State::Disconnected`]（调用 [`Client::transport_closed`] 仍可继续清理传输资源）。
    pub fn handle_packet(&mut self, packet: Packet, now: u64) -> Result<(), ClientError> {
        let result = match self.state {
            State::Disconnected => return Err(ClientError::NotConnected),
            State::Connecting => self.handle_connecting(packet, now),
            State::Connected => self.handle_connected(packet, now),
            State::Disconnecting => self.handle_disconnecting(packet),
        };
        if result.is_err() {
            // 任何协议错误都视为连接已不可用。
            self.abort_connection();
        }
        result
    }

    /// 推进内部定时器：连接超时、Keep Alive 心跳与发送队列刷新。
    pub fn poll(&mut self, now: u64) -> Result<(), ClientError> {
        match self.state {
            State::Connecting => {
                if now >= self.conn.connect_deadline {
                    self.abort_connection();
                    return Err(ClientError::ConnectTimeout);
                }
                Ok(())
            }
            State::Connected => {
                if let Some(deadline) = self.conn.ping_deadline
                    && now >= deadline
                {
                    self.abort_connection();
                    return Err(ClientError::KeepAliveTimeout);
                }
                self.refresh_queued();
                if self.conn.keep_alive_ms > 0
                    && self.conn.ping_deadline.is_none()
                    && now.saturating_sub(self.conn.last_sent) >= self.conn.keep_alive_ms
                {
                    self.conn.outgoing.push_back(Packet::Pingreq);
                    self.conn.ping_deadline = Some(now.saturating_add(self.conn.keep_alive_ms));
                }
                Ok(())
            }
            State::Disconnected | State::Disconnecting => Ok(()),
        }
    }

    /// 取出下一条待发送报文并更新最近发送时刻；队列为空时返回 `None`。
    pub fn next_outgoing(&mut self, now: u64) -> Option<Packet> {
        let packet = self.conn.outgoing.pop_front()?;
        self.conn.last_sent = now;
        Some(packet)
    }

    /// 取出下一条事件；事件队列为空时返回 `None`。
    pub fn next_event(&mut self) -> Option<Event> {
        self.events.pop_front()
    }

    /// 下一次需要调用 [`Client::poll`] 的时刻（Unix 毫秒）；无需定时器时返回 `None`。
    pub fn next_deadline(&self) -> Option<u64> {
        match self.state {
            State::Connecting => Some(self.conn.connect_deadline),
            State::Connected => {
                if self.conn.keep_alive_ms == 0 {
                    return None;
                }
                match self.conn.ping_deadline {
                    Some(deadline) => Some(deadline),
                    None => Some(self.conn.last_sent.saturating_add(self.conn.keep_alive_ms)),
                }
            }
            State::Disconnected | State::Disconnecting => None,
        }
    }

    /// 发布一条应用消息。
    ///
    /// QoS>0 时返回包标识符；QoS 0 返回 `None`。
    pub fn publish(
        &mut self,
        topic: TopicName,
        payload: Vec<u8>,
        options: PublishOptions,
        _now: u64,
    ) -> Result<Option<u16>, ClientError> {
        if self.state != State::Connected {
            return Err(ClientError::NotConnected);
        }
        let qos = options.qos;
        if qos > self.conn.server.maximum_qos {
            return Err(ClientError::QosNotSupported(qos));
        }
        if options.retain && !self.conn.server.retain_available {
            return Err(ClientError::RetainNotSupported);
        }
        if options.properties.topic_alias.is_some() {
            return Err(ClientError::Protocol("发布属性中不允许调用方设置主题别名"));
        }
        let packet_id = match qos {
            QoS::AtMostOnce => None,
            QoS::AtLeastOnce | QoS::ExactlyOnce => Some(
                self.session
                    .allocate_id()
                    .ok_or(ClientError::PacketIdExhausted)?,
            ),
        };
        // canonical 保留完整主题、不含主题别名，用于在途重发；wire 是实际上线形式。
        let canonical = Publish {
            dup: false,
            qos,
            retain: options.retain,
            topic: Some(topic),
            packet_id,
            properties: options.properties,
            payload,
        };
        let mut wire = canonical.clone();
        self.apply_outbound_alias(&mut wire);
        let wire_packet = Packet::Publish(wire);
        let size = match wire_packet.encoded_len() {
            Ok(size) => size,
            Err(err) => {
                if let Some(id) = packet_id {
                    self.session.release_id(id);
                }
                return Err(ClientError::Encode(err));
            }
        };
        let max = self.conn.server.maximum_packet_size;
        if size as u64 > u64::from(max) {
            if let Some(id) = packet_id {
                self.session.release_id(id);
            }
            return Err(ClientError::PacketTooLarge { size, max });
        }
        let Some(packet_id) = packet_id else {
            // QoS 0：直接发送，不占用流控配额。
            self.conn.outgoing.push_back(wire_packet);
            return Ok(None);
        };
        if self.session.inflight.len() >= usize::from(self.conn.server.receive_maximum) {
            // 达到服务端 Receive Maximum：先排队，等配额释放后再发。
            self.session.queued.push_back(QueuedPublish {
                packet_id,
                publish: canonical,
            });
        } else {
            let state = if qos == QoS::AtLeastOnce {
                InflightState::AwaitPuback
            } else {
                InflightState::AwaitPubrec
            };
            self.session.inflight.insert(
                packet_id,
                Inflight {
                    publish: canonical,
                    state,
                },
            );
            self.conn.outgoing.push_back(wire_packet);
        }
        Ok(Some(packet_id))
    }

    /// 发送 SUBSCRIBE 报文，返回分配的包标识符。
    pub fn subscribe(
        &mut self,
        subscriptions: Vec<Subscription>,
        properties: Properties,
        _now: u64,
    ) -> Result<u16, ClientError> {
        if self.state != State::Connected {
            return Err(ClientError::NotConnected);
        }
        if subscriptions.is_empty() {
            return Err(ClientError::Protocol("订阅列表不能为空"));
        }
        for subscription in &subscriptions {
            if !self.conn.server.wildcard_subscription_available
                && subscription.filter.has_wildcard()
            {
                return Err(ClientError::WildcardSubscriptionNotSupported);
            }
            if !self.conn.server.shared_subscription_available && subscription.filter.is_shared() {
                return Err(ClientError::SharedSubscriptionNotSupported);
            }
        }
        if !properties.subscription_identifier.is_empty()
            && !self.conn.server.subscription_identifier_available
        {
            return Err(ClientError::SubscriptionIdentifiersNotSupported);
        }
        let packet_id = self
            .session
            .allocate_id()
            .ok_or(ClientError::PacketIdExhausted)?;
        let packet = Packet::Subscribe(Subscribe {
            packet_id,
            properties,
            subscriptions: subscriptions.clone(),
        });
        if let Err(err) = self.check_outgoing_size(&packet) {
            self.session.release_id(packet_id);
            return Err(err);
        }
        self.conn
            .pending_subscribe
            .insert(packet_id, PendingSubscribe { subscriptions });
        self.conn.outgoing.push_back(packet);
        Ok(packet_id)
    }

    /// 发送 UNSUBSCRIBE 报文，返回分配的包标识符。
    pub fn unsubscribe(
        &mut self,
        filters: Vec<TopicFilter>,
        properties: Properties,
        _now: u64,
    ) -> Result<u16, ClientError> {
        if self.state != State::Connected {
            return Err(ClientError::NotConnected);
        }
        if filters.is_empty() {
            return Err(ClientError::Protocol("取消订阅列表不能为空"));
        }
        let packet_id = self
            .session
            .allocate_id()
            .ok_or(ClientError::PacketIdExhausted)?;
        let packet = Packet::Unsubscribe(Unsubscribe {
            packet_id,
            properties,
            filters: filters.clone(),
        });
        if let Err(err) = self.check_outgoing_size(&packet) {
            self.session.release_id(packet_id);
            return Err(err);
        }
        self.conn
            .pending_unsubscribe
            .insert(packet_id, PendingUnsubscribe { filters });
        self.conn.outgoing.push_back(packet);
        Ok(packet_id)
    }

    /// 主动断开连接：构造 DISCONNECT 入队，状态转为 [`State::Disconnecting`]。
    ///
    /// 处于 [`State::Connecting`] 时直接放弃本次连接（不发送 DISCONNECT），
    /// 会话状态保留给下一次尝试。
    pub fn disconnect(
        &mut self,
        reason: ReasonCode,
        properties: Properties,
        _now: u64,
    ) -> Result<(), ClientError> {
        match self.state {
            State::Connected => {
                let connect_expiry = self.config.session_expiry_interval.unwrap_or(0);
                if connect_expiry == 0
                    && let Some(value) = properties.session_expiry_interval
                    && value != 0
                {
                    return Err(ClientError::Protocol(
                        "CONNECT 的会话过期时间为 0 时 DISCONNECT 不得改为非 0",
                    ));
                }
                if let Some(value) = properties.session_expiry_interval {
                    self.session_expiry = Some(value);
                }
                self.conn.outgoing.push_back(Packet::Disconnect(Disconnect {
                    reason_code: reason,
                    properties,
                }));
                self.state = State::Disconnecting;
                Ok(())
            }
            State::Connecting => {
                self.cleanup_connection();
                Ok(())
            }
            State::Disconnected | State::Disconnecting => Err(ClientError::NotConnected),
        }
    }

    /// 发送 AUTH 报文（增强认证响应），连接中或已连接状态可用。
    pub fn auth(&mut self, auth: Auth, _now: u64) -> Result<(), ClientError> {
        match self.state {
            State::Connecting | State::Connected => {
                self.conn.outgoing.push_back(Packet::Auth(auth));
                Ok(())
            }
            State::Disconnected | State::Disconnecting => Err(ClientError::NotConnected),
        }
    }

    /// 传输断开后由调用方调用：清理连接级状态，并按会话保留判定处理会话状态。
    pub fn transport_closed(&mut self, _now: u64) {
        let keep_session = self.session_retained();
        self.cleanup_connection();
        if !keep_session {
            let lost_packet_ids = self.session.reset();
            if !lost_packet_ids.is_empty() {
                self.events
                    .push_back(Event::SessionReset { lost_packet_ids });
            }
        }
    }

    // ---- 配置与构造 ----

    /// 校验配置中的非法组合。
    fn validate_config(&self) -> Result<(), ClientError> {
        if self.config.client_id.is_empty() && !self.config.clean_start {
            return Err(ClientError::InvalidConfig(
                "Client ID 为空时必须使用 Clean Start",
            ));
        }
        if self.config.receive_maximum == Some(0) {
            return Err(ClientError::InvalidConfig("Receive Maximum 不能为 0"));
        }
        if self.config.maximum_packet_size == Some(0) {
            return Err(ClientError::InvalidConfig("Maximum Packet Size 不能为 0"));
        }
        if self.config.authentication_data.is_some() && self.config.authentication_method.is_none()
        {
            return Err(ClientError::InvalidConfig(
                "携带 Authentication Data 时必须提供 Authentication Method",
            ));
        }
        // 注意：Clean Start 为真时 Session Expiry 可以任意（规范允许），因此不额外校验。
        Ok(())
    }

    /// 按配置构造 CONNECT 报文。
    fn build_connect(&self) -> Connect {
        let mut properties = Properties::new();
        properties.session_expiry_interval = self.config.session_expiry_interval;
        properties.receive_maximum = self.config.receive_maximum;
        properties.maximum_packet_size = self.config.maximum_packet_size;
        properties.topic_alias_maximum = self.config.topic_alias_maximum;
        properties.request_response_information = self.config.request_response_information;
        properties.request_problem_information = self.config.request_problem_information;
        properties.user_property = self.config.user_properties.clone();
        properties.authentication_method = self.config.authentication_method.clone();
        properties.authentication_data = self.config.authentication_data.clone();
        Connect {
            clean_start: self.config.clean_start,
            keep_alive: self.config.keep_alive,
            client_id: self.config.client_id.clone(),
            will: self.config.will.clone(),
            username: self.config.username.clone(),
            password: self.config.password.clone(),
            properties,
        }
    }

    // ---- 入站报文分发 ----

    /// 处理 `Connecting` 状态的入站报文。
    fn handle_connecting(&mut self, packet: Packet, now: u64) -> Result<(), ClientError> {
        match packet {
            Packet::Connack(connack) => self.handle_connack(connack, now),
            Packet::Auth(auth) => {
                self.events.push_back(Event::Auth(auth));
                Ok(())
            }
            Packet::Disconnect(disconnect) => {
                self.on_server_disconnect(disconnect);
                Ok(())
            }
            other => Err(ClientError::UnexpectedPacket(other.packet_type())),
        }
    }

    /// 处理 `Connected` 状态的入站报文。
    fn handle_connected(&mut self, packet: Packet, _now: u64) -> Result<(), ClientError> {
        match packet {
            Packet::Publish(publish) => self.handle_inbound_publish(publish),
            Packet::Puback(ack) => self.handle_puback(ack),
            Packet::Pubrec(ack) => self.handle_pubrec(ack),
            Packet::Pubrel(ack) => self.handle_pubrel(ack),
            Packet::Pubcomp(ack) => self.handle_pubcomp(ack),
            Packet::Suback(suback) => self.handle_suback(suback),
            Packet::Unsuback(unsuback) => self.handle_unsuback(unsuback),
            Packet::Pingresp => {
                // 未请求的 PINGRESP 直接忽略。
                self.conn.ping_deadline = None;
                Ok(())
            }
            Packet::Disconnect(disconnect) => {
                self.on_server_disconnect(disconnect);
                Ok(())
            }
            Packet::Auth(auth) => {
                self.events.push_back(Event::Auth(auth));
                Ok(())
            }
            Packet::Connack(_) => Err(ClientError::Protocol("已连接状态下再次收到 CONNACK")),
            other => Err(ClientError::UnexpectedPacket(other.packet_type())),
        }
    }

    /// 处理 `Disconnecting` 状态的入站报文：只接受服务端 DISCONNECT。
    fn handle_disconnecting(&mut self, packet: Packet) -> Result<(), ClientError> {
        match packet {
            Packet::Disconnect(disconnect) => {
                self.on_server_disconnect(disconnect);
                Ok(())
            }
            other => Err(ClientError::UnexpectedPacket(other.packet_type())),
        }
    }

    /// 处理 CONNACK（连接建立或拒绝）。
    fn handle_connack(&mut self, connack: Connack, now: u64) -> Result<(), ClientError> {
        if connack.reason_code != ReasonCode::Success {
            // 连接被拒绝：只清理连接级状态，会话状态保留给下一次尝试。
            self.events.push_back(Event::ConnectionRefused(connack));
            self.cleanup_connection();
            return Ok(());
        }
        if self.config.clean_start && connack.session_present {
            return Err(ClientError::Protocol(
                "Clean Start 为真时服务端不得声明会话已存在",
            ));
        }
        self.apply_connack_properties(&connack);
        self.state = State::Connected;
        self.conn.last_sent = now;
        self.conn.ping_deadline = None;
        if connack.session_present {
            // 会话恢复：重发在途 PUBLISH（DUP=1）与 PUBREL。
            self.resend_inflight();
        } else {
            let lost_packet_ids = self.session.reset();
            self.release_pending_requests();
            if !lost_packet_ids.is_empty() {
                self.events
                    .push_back(Event::SessionReset { lost_packet_ids });
            }
            // 会话不存在：按已授权订阅意图自动重订阅。
            self.resubscribe_all(now);
        }
        self.refresh_queued();
        self.events.push_back(Event::Connected(connack));
        Ok(())
    }

    /// 应用 CONNACK 中服务端声明的能力与信息。
    fn apply_connack_properties(&mut self, connack: &Connack) {
        let properties = &connack.properties;
        if let Some(value) = properties.receive_maximum {
            self.conn.server.receive_maximum = value;
        }
        if let Some(value) = properties.maximum_packet_size {
            self.conn.server.maximum_packet_size = value;
        }
        if let Some(value) = properties.topic_alias_maximum {
            self.conn.server.topic_alias_maximum = value;
        }
        if let Some(value) = properties.maximum_qos {
            self.conn.server.maximum_qos = match value {
                0 => QoS::AtMostOnce,
                _ => QoS::AtLeastOnce,
            };
        }
        if let Some(value) = properties.retain_available {
            self.conn.server.retain_available = value;
        }
        if let Some(value) = properties.wildcard_subscription_available {
            self.conn.server.wildcard_subscription_available = value;
        }
        if let Some(value) = properties.subscription_identifier_available {
            self.conn.server.subscription_identifier_available = value;
        }
        if let Some(value) = properties.shared_subscription_available {
            self.conn.server.shared_subscription_available = value;
        }
        if let Some(value) = properties.server_keep_alive {
            // 服务端可以覆盖客户端请求的 Keep Alive。
            self.conn.keep_alive_ms = u64::from(value) * 1000;
        }
        if let Some(client_id) = &properties.assigned_client_identifier {
            self.assigned_client_id = Some(client_id.clone());
        }
        if let Some(value) = properties.session_expiry_interval {
            self.session_expiry = Some(value);
        }
    }

    /// 重发在途出站消息：等待 PUBACK/PUBREC 的重发带 DUP 的 PUBLISH，等待 PUBCOMP 的重发 PUBREL。
    fn resend_inflight(&mut self) {
        let resends: Vec<(u16, InflightState, Publish)> = self
            .session
            .inflight
            .iter()
            .map(|(packet_id, inflight)| (*packet_id, inflight.state, inflight.publish.clone()))
            .collect();
        for (packet_id, state, mut publish) in resends {
            match state {
                InflightState::AwaitPuback | InflightState::AwaitPubrec => {
                    publish.dup = true;
                    self.conn.outgoing.push_back(Packet::Publish(publish));
                }
                InflightState::AwaitPubcomp => {
                    self.conn
                        .outgoing
                        .push_back(Packet::Pubrel(ack_packet(packet_id, ReasonCode::Success)));
                }
            }
        }
    }

    /// 会话未恢复时按已授权订阅意图自动重订阅（一次 SUBSCRIBE 携带全部过滤器）。
    fn resubscribe_all(&mut self, now: u64) {
        if self.session.subscriptions.is_empty() {
            return;
        }
        let subscriptions: Vec<Subscription> =
            self.session.subscriptions.values().cloned().collect();
        // 自动重订阅失败（例如包标识符耗尽或服务端能力变化）不影响连接本身。
        let _ = self.subscribe(subscriptions, Properties::new(), now);
    }

    // ---- 入站 QoS 流程 ----

    /// 处理入站 PUBLISH：解析主题别名、做入站长度/流控检查、自动应答。
    fn handle_inbound_publish(&mut self, mut publish: Publish) -> Result<(), ClientError> {
        // 先解析主题别名，保证后续处理看到的是完整主题。
        if let Some(alias) = publish.properties.topic_alias {
            let max_alias = self.config.topic_alias_maximum.unwrap_or(0);
            if alias == 0 || alias > max_alias {
                return Err(ClientError::TopicAliasInvalid);
            }
            match publish.topic.clone() {
                Some(topic) => {
                    // 非空主题 + 别名：更新映射。
                    self.conn.inbound_aliases.insert(alias, topic);
                }
                None => {
                    // 空主题：必须能从别名映射解析出主题。
                    let Some(topic) = self.conn.inbound_aliases.get(&alias) else {
                        return Err(ClientError::TopicAliasInvalid);
                    };
                    publish.topic = Some(topic.clone());
                }
            }
        } else if publish.topic.is_none() {
            return Err(ClientError::Protocol(
                "入站 PUBLISH 主题为空且未携带主题别名",
            ));
        }
        // 客户端声明的入站最大报文长度检查。
        if let Some(max) = self.config.maximum_packet_size {
            let size = publish_wire_len(&publish)?;
            if size as u64 > u64::from(max) {
                return Err(ClientError::PacketTooLarge { size, max });
            }
        }
        let packet_id = match publish.qos {
            QoS::AtMostOnce => None,
            QoS::AtLeastOnce | QoS::ExactlyOnce => Some(
                publish
                    .packet_id
                    .ok_or(ClientError::Protocol("QoS>0 的 PUBLISH 缺少包标识符"))?,
            ),
        };
        match publish.qos {
            QoS::AtMostOnce => {
                self.events.push_back(Event::Publish(publish));
            }
            QoS::AtLeastOnce => {
                self.events.push_back(Event::Publish(publish));
                let packet_id = packet_id.ok_or(ClientError::Protocol("缺少包标识符"))?;
                self.conn
                    .outgoing
                    .push_back(Packet::Puback(ack_packet(packet_id, ReasonCode::Success)));
            }
            QoS::ExactlyOnce => {
                let packet_id = packet_id.ok_or(ClientError::Protocol("缺少包标识符"))?;
                if self.session.inbound_qos2.contains(&packet_id) {
                    // 重复的入站 QoS2 PUBLISH：只重发 PUBREC，不重复投递。
                    self.conn
                        .outgoing
                        .push_back(Packet::Pubrec(ack_packet(packet_id, ReasonCode::Success)));
                    return Ok(());
                }
                let receive_maximum = self.config.receive_maximum.unwrap_or(u16::MAX);
                if self.session.inbound_qos2.len() >= usize::from(receive_maximum) {
                    return Err(ClientError::ReceiveMaximumExceeded);
                }
                self.session.inbound_qos2.insert(packet_id);
                self.events.push_back(Event::Publish(publish));
                self.conn
                    .outgoing
                    .push_back(Packet::Pubrec(ack_packet(packet_id, ReasonCode::Success)));
            }
        }
        Ok(())
    }

    /// 处理入站 PUBACK。
    fn handle_puback(&mut self, ack: Ack) -> Result<(), ClientError> {
        let Some(state) = self
            .session
            .inflight
            .get(&ack.packet_id)
            .map(|inflight| inflight.state)
        else {
            return Err(ClientError::UnknownPacketId(ack.packet_id));
        };
        if state != InflightState::AwaitPuback {
            return Err(ClientError::Protocol("PUBACK 与在途 QoS2 流程不匹配"));
        }
        self.session.inflight.remove(&ack.packet_id);
        self.session.release_id(ack.packet_id);
        self.refresh_queued();
        self.events.push_back(Event::Puback(ack));
        Ok(())
    }

    /// 处理入站 PUBREC（QoS2 出站流程第二步）。
    fn handle_pubrec(&mut self, ack: Ack) -> Result<(), ClientError> {
        let Some(state) = self
            .session
            .inflight
            .get(&ack.packet_id)
            .map(|inflight| inflight.state)
        else {
            return Err(ClientError::UnknownPacketId(ack.packet_id));
        };
        match state {
            InflightState::AwaitPubrec => {
                if ack.reason_code.is_error() {
                    // PUBREC 表示失败：释放配额与包标识符，不再发送 PUBREL。
                    self.session.inflight.remove(&ack.packet_id);
                    self.session.release_id(ack.packet_id);
                    self.refresh_queued();
                } else if let Some(inflight) = self.session.inflight.get_mut(&ack.packet_id) {
                    inflight.state = InflightState::AwaitPubcomp;
                    self.conn.outgoing.push_back(Packet::Pubrel(ack_packet(
                        ack.packet_id,
                        ReasonCode::Success,
                    )));
                }
                self.events.push_back(Event::Pubrec(ack));
                Ok(())
            }
            InflightState::AwaitPubcomp => {
                // 服务端重发的 PUBREC：重发 PUBREL，保持幂等。
                self.conn.outgoing.push_back(Packet::Pubrel(ack_packet(
                    ack.packet_id,
                    ReasonCode::Success,
                )));
                self.events.push_back(Event::Pubrec(ack));
                Ok(())
            }
            InflightState::AwaitPuback => {
                Err(ClientError::Protocol("PUBREC 与在途 QoS1 流程不匹配"))
            }
        }
    }

    /// 处理入站 PUBCOMP（QoS2 出站流程第三步）。
    fn handle_pubcomp(&mut self, ack: Ack) -> Result<(), ClientError> {
        let Some(state) = self
            .session
            .inflight
            .get(&ack.packet_id)
            .map(|inflight| inflight.state)
        else {
            return Err(ClientError::UnknownPacketId(ack.packet_id));
        };
        if state != InflightState::AwaitPubcomp {
            return Err(ClientError::Protocol("PUBCOMP 与在途状态不匹配"));
        }
        self.session.inflight.remove(&ack.packet_id);
        self.session.release_id(ack.packet_id);
        self.refresh_queued();
        self.events.push_back(Event::Pubcomp(ack));
        Ok(())
    }

    /// 处理入站 PUBREL（入站 QoS2 流程第二步）。
    fn handle_pubrel(&mut self, ack: Ack) -> Result<(), ClientError> {
        let reason_code = if self.session.inbound_qos2.remove(&ack.packet_id) {
            ReasonCode::Success
        } else {
            // 未知包标识符：按规范回 Packet Identifier not found。
            ReasonCode::PacketIdentifierNotFound
        };
        self.conn
            .outgoing
            .push_back(Packet::Pubcomp(ack_packet(ack.packet_id, reason_code)));
        Ok(())
    }

    // ---- 订阅流程 ----

    /// 处理入站 SUBACK。
    fn handle_suback(&mut self, suback: Suback) -> Result<(), ClientError> {
        let Some(pending) = self.conn.pending_subscribe.remove(&suback.packet_id) else {
            return Err(ClientError::UnknownPacketId(suback.packet_id));
        };
        if pending.subscriptions.len() != suback.reason_codes.len() {
            self.session.release_id(suback.packet_id);
            return Err(ClientError::Protocol("SUBACK 原因码数量与订阅数量不一致"));
        }
        for (subscription, reason_code) in
            pending.subscriptions.iter().zip(suback.reason_codes.iter())
        {
            if reason_code.is_error() {
                continue;
            }
            // 只有授予成功（QoS 0/1/2）的订阅才记为已授权订阅意图。
            let key = String::from(subscription.filter.as_str());
            self.session.subscriptions.insert(key, subscription.clone());
        }
        self.session.release_id(suback.packet_id);
        self.events.push_back(Event::Suback(suback));
        Ok(())
    }

    /// 处理入站 UNSUBACK。
    fn handle_unsuback(&mut self, unsuback: Unsuback) -> Result<(), ClientError> {
        let Some(pending) = self.conn.pending_unsubscribe.remove(&unsuback.packet_id) else {
            return Err(ClientError::UnknownPacketId(unsuback.packet_id));
        };
        if pending.filters.len() != unsuback.reason_codes.len() {
            self.session.release_id(unsuback.packet_id);
            return Err(ClientError::Protocol(
                "UNSUBACK 原因码数量与过滤器数量不一致",
            ));
        }
        for (filter, reason_code) in pending.filters.iter().zip(unsuback.reason_codes.iter()) {
            if reason_code.is_error() {
                continue;
            }
            self.session.subscriptions.remove(filter.as_str());
        }
        self.session.release_id(unsuback.packet_id);
        self.events.push_back(Event::Unsuback(unsuback));
        Ok(())
    }

    /// 服务端主动 DISCONNECT：清连接级状态，并按会话保留判定处理会话状态。
    fn on_server_disconnect(&mut self, disconnect: Disconnect) {
        let keep_session = self.session_retained();
        self.cleanup_connection();
        if !keep_session {
            let lost_packet_ids = self.session.reset();
            if !lost_packet_ids.is_empty() {
                self.events
                    .push_back(Event::SessionReset { lost_packet_ids });
            }
        }
        self.events.push_back(Event::Disconnected(disconnect));
    }

    // ---- 连接级辅助 ----

    /// 会话在断开后是否保留：非 Clean Start 且会话过期时间不为 0。
    fn session_retained(&self) -> bool {
        !self.config.clean_start && self.session_expiry != Some(0)
    }

    /// 校验待发送报文不超过服务端声明的最大报文长度。
    fn check_outgoing_size(&self, packet: &Packet) -> Result<(), ClientError> {
        let size = packet.encoded_len()?;
        let max = self.conn.server.maximum_packet_size;
        if size as u64 > u64::from(max) {
            return Err(ClientError::PacketTooLarge { size, max });
        }
        Ok(())
    }

    /// 清理连接级状态（不动会话级状态），状态转为 [`State::Disconnected`]。
    fn cleanup_connection(&mut self) {
        self.release_pending_requests();
        self.conn.outgoing.clear();
        self.conn.outbound_aliases.clear();
        self.conn.next_outbound_alias = 1;
        self.conn.inbound_aliases.clear();
        self.conn.server = ServerCapabilities::default();
        self.conn.keep_alive_ms = u64::from(self.config.keep_alive) * 1000;
        self.conn.ping_deadline = None;
        self.conn.connect_deadline = 0;
        self.assigned_client_id = None;
        self.state = State::Disconnected;
    }

    /// 释放并清空所有待响应请求。
    fn release_pending_requests(&mut self) {
        let subscribe_ids: Vec<u16> = self.conn.pending_subscribe.keys().copied().collect();
        for packet_id in subscribe_ids {
            self.session.release_id(packet_id);
        }
        self.conn.pending_subscribe.clear();
        let unsubscribe_ids: Vec<u16> = self.conn.pending_unsubscribe.keys().copied().collect();
        for packet_id in unsubscribe_ids {
            self.session.release_id(packet_id);
        }
        self.conn.pending_unsubscribe.clear();
    }

    /// 协议错误导致连接不可用：清理连接级状态，必要时重置会话。
    fn abort_connection(&mut self) {
        let keep_session = self.session_retained();
        self.cleanup_connection();
        if !keep_session {
            let lost_packet_ids = self.session.reset();
            if !lost_packet_ids.is_empty() {
                self.events
                    .push_back(Event::SessionReset { lost_packet_ids });
            }
        }
    }

    /// 刷新待发送队列：按服务端 Receive Maximum 把排队中的 PUBLISH 转入在途并发送。
    fn refresh_queued(&mut self) {
        while self.session.inflight.len() < usize::from(self.conn.server.receive_maximum) {
            let Some(queued) = self.session.queued.pop_front() else {
                return;
            };
            let canonical = queued.publish;
            if canonical.qos == QoS::AtMostOnce {
                // 正常流程不会出现（只有 QoS>0 才排队），防御性丢弃。
                self.session.release_id(queued.packet_id);
                continue;
            }
            let mut wire = canonical.clone();
            self.apply_outbound_alias(&mut wire);
            let state = if canonical.qos == QoS::AtLeastOnce {
                InflightState::AwaitPuback
            } else {
                InflightState::AwaitPubrec
            };
            self.session.inflight.insert(
                queued.packet_id,
                Inflight {
                    publish: canonical,
                    state,
                },
            );
            self.conn.outgoing.push_back(Packet::Publish(wire));
        }
    }

    /// 按出站主题别名规则改写 `wire`：可能清空主题并写入 TopicAlias 属性。
    ///
    /// 首次使用时保留完整主题；再次发布同一主题时发送空主题 + 别名。
    fn apply_outbound_alias(&mut self, wire: &mut Publish) {
        if !self.config.use_topic_alias {
            return;
        }
        let max_alias = self.conn.server.topic_alias_maximum;
        if max_alias == 0 {
            return;
        }
        let Some(topic) = &wire.topic else {
            return;
        };
        let key = String::from(topic.as_str());
        if let Some(alias) = self.conn.outbound_aliases.get(&key) {
            // 别名已存在：后续发布使用空主题 + 别名。
            wire.topic = None;
            wire.properties.topic_alias = Some(*alias);
            return;
        }
        if self.conn.next_outbound_alias <= u32::from(max_alias) {
            let alias = self.conn.next_outbound_alias as u16;
            self.conn.next_outbound_alias += 1;
            self.conn.outbound_aliases.insert(key, alias);
            // 首次发送保留完整主题，同时携带别名供服务端登记映射。
            wire.properties.topic_alias = Some(alias);
        }
        // 别名表已满：继续使用完整主题、不带别名。
    }
}

/// 计算 PUBLISH 报文的线上编码长度（不复制载荷数据）。
fn publish_wire_len(publish: &Publish) -> Result<usize, EncodeError> {
    let head_only = Packet::Publish(Publish {
        dup: publish.dup,
        qos: publish.qos,
        retain: publish.retain,
        topic: publish.topic.clone(),
        packet_id: publish.packet_id,
        properties: publish.properties.clone(),
        payload: Vec::new(),
    });
    Ok(head_only.encoded_len()? + publish.payload.len())
}

/// 构造 PUBACK / PUBREC / PUBREL / PUBCOMP 报文（给原因码、空属性）。
fn ack_packet(packet_id: u16, reason_code: ReasonCode) -> Ack {
    Ack {
        packet_id,
        reason_code,
        properties: Properties::new(),
    }
}
