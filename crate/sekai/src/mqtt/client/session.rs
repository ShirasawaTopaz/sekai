//! 客户端内部会话状态。
//!
//! 本模块只保存状态并提供纯内存操作，不读取时钟、不做 I/O。

use alloc::collections::{BTreeMap, BTreeSet, VecDeque};
use alloc::string::String;
use alloc::vec::Vec;

use crate::mqtt::packet::{Packet, Publish};
use crate::mqtt::qos::QoS;
use crate::mqtt::topic::{Subscription, TopicFilter, TopicName};

/// 出站 QoS 报文的在途阶段。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(clippy::enum_variant_names)] // 三个变体都以 Await 开头，语义清晰，内部类型无需改名。
pub(crate) enum InflightState {
    /// 已发送 PUBLISH，等待 PUBACK。
    AwaitPuback,
    /// 已发送 PUBLISH，等待 PUBREC。
    AwaitPubrec,
    /// 已发送 PUBREL，等待 PUBCOMP。
    AwaitPubcomp,
}

/// 一条在途的出站 QoS>0 发布。
#[derive(Debug)]
pub(crate) struct Inflight {
    /// 原始完整报文：完整主题、不含主题别名，供重连后重发使用。
    pub publish: Publish,
    /// 当前所处的阶段。
    pub state: InflightState,
}

/// 因服务端 Receive Maximum 限制而排队等待发送的发布（包标识符已分配）。
#[derive(Debug)]
pub(crate) struct QueuedPublish {
    /// 已分配的包标识符。
    pub packet_id: u16,
    /// 原始完整报文（不含主题别名）。
    pub publish: Publish,
}

/// 等待 SUBACK 的订阅请求。
#[derive(Debug)]
pub(crate) struct PendingSubscribe {
    /// 按线上顺序保存的订阅项，用于把 SUBACK 原因码映射回对应订阅。
    pub subscriptions: Vec<Subscription>,
}

/// 等待 UNSUBACK 的取消订阅请求。
#[derive(Debug)]
pub(crate) struct PendingUnsubscribe {
    /// 按线上顺序保存的主题过滤器，用于把 UNSUBACK 原因码映射回对应过滤器。
    pub filters: Vec<TopicFilter>,
}

/// 会话级状态：网络连接断开后可能仍然保留。
#[derive(Debug)]
pub(crate) struct SessionState {
    /// 在途的 QoS>0 出站发布，key 为包标识符。
    pub inflight: BTreeMap<u16, Inflight>,
    /// 等待发送的 QoS>0 发布。
    pub queued: VecDeque<QueuedPublish>,
    /// 入站 QoS2 已收到 PUBLISH、尚未收到 PUBREL 的包标识符集合。
    pub inbound_qos2: BTreeSet<u16>,
    /// 已授权订阅意图，key 为过滤器字符串；用于会话未恢复时自动重订阅。
    pub subscriptions: BTreeMap<String, Subscription>,
    /// 已被占用的包标识符。
    used_ids: BTreeSet<u16>,
    /// 下一次分配包标识符的起点（1..=65535）。
    next_id: u16,
}

impl Default for SessionState {
    fn default() -> Self {
        Self {
            inflight: BTreeMap::new(),
            queued: VecDeque::new(),
            inbound_qos2: BTreeSet::new(),
            subscriptions: BTreeMap::new(),
            used_ids: BTreeSet::new(),
            next_id: 1,
        }
    }
}

impl SessionState {
    /// 分配一个未被占用的包标识符；1..=65535 全部占用时返回 `None`。
    pub(crate) fn allocate_id(&mut self) -> Option<u16> {
        if self.used_ids.len() >= usize::from(u16::MAX) {
            return None;
        }
        let mut candidate = if self.next_id == 0 { 1 } else { self.next_id };
        loop {
            if !self.used_ids.contains(&candidate) {
                self.used_ids.insert(candidate);
                self.next_id = if candidate == u16::MAX {
                    1
                } else {
                    candidate + 1
                };
                return Some(candidate);
            }
            candidate = if candidate == u16::MAX {
                1
            } else {
                candidate + 1
            };
        }
    }

    /// 释放包标识符（幂等）。
    pub(crate) fn release_id(&mut self, packet_id: u16) {
        self.used_ids.remove(&packet_id);
    }

    /// 清空会话状态，返回被丢弃的在途出站包标识符（升序）。
    ///
    /// 待发送队列（`queued`）不在此清空：其中保存的是尚未发出的应用消息，
    /// 网络断开重连后仍可继续发送。
    pub(crate) fn reset(&mut self) -> Vec<u16> {
        let lost: Vec<u16> = self.inflight.keys().copied().collect();
        for packet_id in &lost {
            self.used_ids.remove(packet_id);
        }
        self.inflight.clear();
        self.inbound_qos2.clear();
        lost
    }
}

/// 服务端在 CONNACK 中声明的能力，默认值对应 MQTT 5.0 规范缺省值。
#[derive(Debug, Clone)]
pub(crate) struct ServerCapabilities {
    /// 服务端可同时接收的未确认 QoS>0 报文数上限。
    pub receive_maximum: u16,
    /// 服务端可接收的最大报文长度。
    pub maximum_packet_size: u32,
    /// 服务端可接收的主题别名数量上限。
    pub topic_alias_maximum: u16,
    /// 服务端支持的最大 QoS。
    pub maximum_qos: QoS,
    /// 是否支持保留消息。
    pub retain_available: bool,
    /// 是否支持通配符订阅。
    pub wildcard_subscription_available: bool,
    /// 是否支持订阅标识符。
    pub subscription_identifier_available: bool,
    /// 是否支持共享订阅。
    pub shared_subscription_available: bool,
}

impl Default for ServerCapabilities {
    fn default() -> Self {
        Self {
            receive_maximum: u16::MAX,
            maximum_packet_size: 268_435_460,
            topic_alias_maximum: 0,
            maximum_qos: QoS::ExactlyOnce,
            retain_available: true,
            wildcard_subscription_available: true,
            subscription_identifier_available: true,
            shared_subscription_available: true,
        }
    }
}

/// 连接级状态：每次网络连接重建时清空。
#[derive(Debug)]
pub(crate) struct ConnectionState {
    /// 待发送报文队列。
    pub outgoing: VecDeque<Packet>,
    /// 等待 SUBACK 的请求。
    pub pending_subscribe: BTreeMap<u16, PendingSubscribe>,
    /// 等待 UNSUBACK 的请求。
    pub pending_unsubscribe: BTreeMap<u16, PendingUnsubscribe>,
    /// 出站主题别名映射（主题字符串 → 别名）。
    pub outbound_aliases: BTreeMap<String, u16>,
    /// 下一个可分配的出站主题别名（从 1 开始）。
    pub next_outbound_alias: u32,
    /// 入站主题别名映射（别名 → 主题）。
    pub inbound_aliases: BTreeMap<u16, TopicName>,
    /// 服务端能力。
    pub server: ServerCapabilities,
    /// 当前生效的 Keep Alive 间隔（毫秒），CONNACK 的 Server Keep Alive 可覆盖。
    pub keep_alive_ms: u64,
    /// 等待 CONNACK 的截止时刻（毫秒）。
    pub connect_deadline: u64,
    /// 最近一次取出报文的时刻（毫秒）。
    pub last_sent: u64,
    /// 已发出 PINGREQ 时等待 PINGRESP 的截止时刻（毫秒）。
    pub ping_deadline: Option<u64>,
}

impl Default for ConnectionState {
    fn default() -> Self {
        Self {
            outgoing: VecDeque::new(),
            pending_subscribe: BTreeMap::new(),
            pending_unsubscribe: BTreeMap::new(),
            outbound_aliases: BTreeMap::new(),
            next_outbound_alias: 1,
            inbound_aliases: BTreeMap::new(),
            server: ServerCapabilities::default(),
            keep_alive_ms: 0,
            connect_deadline: 0,
            last_sent: 0,
            ping_deadline: None,
        }
    }
}
