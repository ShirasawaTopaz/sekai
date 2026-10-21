//! 客户端事件。
//!
//! 调用方通过 [`Client::next_event`](super::Client::next_event) 按产生顺序取出事件。

use alloc::vec::Vec;

use crate::mqtt::packet::{Ack, Auth, Connack, Disconnect, Publish, Suback, Unsuback};

/// 客户端对外暴露的事件。
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(clippy::large_enum_variant)] // 事件直接携带报文层结构体，尺寸由报文层决定。
pub enum Event {
    /// 连接建立成功，携带服务端的 CONNACK。
    Connected(Connack),
    /// 连接被服务端拒绝，携带拒绝用的 CONNACK。
    ConnectionRefused(Connack),
    /// 连接终止（服务端主动 DISCONNECT 时携带原始报文）。
    Disconnected(Disconnect),
    /// 入站应用消息；主题别名已解析，`topic` 必为 `Some`。
    Publish(Publish),
    /// 出站 QoS1 消息收到 PUBACK。
    Puback(Ack),
    /// 出站 QoS2 消息收到 PUBREC。
    Pubrec(Ack),
    /// 出站 QoS2 消息收到 PUBCOMP。
    Pubcomp(Ack),
    /// 收到 SUBACK。
    Suback(Suback),
    /// 收到 UNSUBACK。
    Unsuback(Unsuback),
    /// 收到 AUTH（增强认证透传）。
    Auth(Auth),
    /// 会话被重置，列出因此丢失的出站在途包标识符。
    SessionReset { lost_packet_ids: Vec<u16> },
}
