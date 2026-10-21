//! MQTT 5.0 客户端协议实现。
//!
//! 模块划分：
//! - `error`：解码/编码错误；
//! - `varint`：Variable Byte Integer；
//! - `qos`：QoS 枚举；
//! - `header`：固定头与报文类型；
//! - `reason`：原因码；
//! - `topic`：主题名、主题过滤器与订阅选项；
//! - `property`：属性；
//! - `packet`：15 种控制报文的编解码；
//! - `client`：客户端会话状态机。
//!
//! `codec` 是纯内部模块，不对外暴露。
//!
//! 本模块只使用 `core` + `alloc`，不依赖标准库、不包含 `unsafe`。
//!
//! # 使用示例
//!
//! 客户端状态机不绑定 I/O，调用方负责读写传输层：
//!
//! 1. `Client::connect` 后循环取 `Client::next_outgoing` 的报文写入传输；
//! 2. 收到字节后用 `Packet::decode` 解码并交给 `Client::handle_packet`；
//! 3. 定时调用 `Client::poll` 处理超时，可参考 `Client::next_deadline`；
//! 4. 用 `Client::next_event` 取出应用事件。

pub mod client;
mod codec;
pub mod error;
pub mod header;
pub mod packet;
pub mod property;
pub mod qos;
pub mod reason;
pub mod topic;
pub mod varint;

pub use client::{Client, ClientConfig, ClientError, Event, PublishOptions, State};
pub use error::{DecodeError, EncodeError};
pub use header::{FixedHeader, PacketType};
pub use packet::{
    Ack, Auth, Connack, Connect, Disconnect, Packet, Publish, Suback, Subscribe, Unsuback,
    Unsubscribe, Will,
};
pub use property::Properties;
pub use qos::QoS;
pub use reason::ReasonCode;
pub use topic::{Subscription, SubscriptionOptions, TopicFilter, TopicName};
