//! MQTT 5.0 控制报文编解码。
//!
//! 本模块负责 15 种控制报文的线上格式解析与生成。校验分层：
//! - 本模块只做线上格式与通用协议校验；
//! - 角色相关、有状态的规则（Clean Start 与空 Client ID、CONNACK session present
//!   与客户端实际会话的一致性、包 ID 复用等）由客户端状态机负责。
//!
//! 编解码数据流：
//! 1. `FixedHeader::decode` 解析类型/flags/remaining length；
//! 2. `Packet::decode` 检查总长度并切出报文体；
//! 3. 各报文体 decoder 使用内部 `Reader`，必须恰好读完报文体；
//! 4. 固定头之外的多余字节不消费，返回给调用方以便处理连续流。

mod ack;
mod auth;
mod connack;
mod connect;
mod disconnect;
mod ping;
mod publish;
mod suback;
mod subscribe;
mod unsuback;
mod unsubscribe;

#[cfg(test)]
mod tests;

pub use ack::Ack;
pub use auth::Auth;
pub use connack::Connack;
pub use connect::{Connect, Will};
pub use disconnect::Disconnect;
pub use publish::Publish;
pub use suback::Suback;
pub use subscribe::Subscribe;
pub use unsuback::Unsuback;
pub use unsubscribe::Unsubscribe;

use alloc::vec::Vec;

use crate::mqtt::codec::{CountSink, Reader, Sink, VecSink, write_u8};
use crate::mqtt::error::{DecodeError, EncodeError};
use crate::mqtt::header::{FixedHeader, PacketType};
use crate::mqtt::property::{Properties, PropertyContext};
use crate::mqtt::reason::ReasonCode;
use crate::mqtt::varint;

/// 报文最大总长度：5 字节固定头 + 最大 Remaining Length（268,435,455）。
pub const MAX_PACKET_SIZE: usize = 268_435_460;

/// MQTT 5.0 控制报文。
///
/// 各变体大小差异由协议本身决定；装箱会破坏公开 API 的使用便利性，
/// 因此在这里允许该 lint。
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Packet {
    Connect(Connect),
    Connack(Connack),
    Publish(Publish),
    Puback(Ack),
    Pubrec(Ack),
    Pubrel(Ack),
    Pubcomp(Ack),
    Subscribe(Subscribe),
    Suback(Suback),
    Unsubscribe(Unsubscribe),
    Unsuback(Unsuback),
    Pingreq,
    Pingresp,
    Disconnect(Disconnect),
    Auth(Auth),
}

impl Packet {
    /// 从连续字节流中解码一个报文。
    ///
    /// 返回 `(报文, 消耗的字节数)`；固定头之外的多余字节留给调用方。
    /// 字节不足返回 `DecodeError::Incomplete`，此时不消费任何数据。
    pub fn decode(buf: &[u8]) -> Result<(Self, usize), DecodeError> {
        Self::decode_inner(buf, None)
    }

    /// 带最大总长度限制的解码；总长度超过 `max_packet_size` 返回 `PacketTooLarge`。
    pub fn decode_with_limit(
        buf: &[u8],
        max_packet_size: u32,
    ) -> Result<(Self, usize), DecodeError> {
        Self::decode_inner(buf, Some(max_packet_size))
    }

    fn decode_inner(
        buf: &[u8],
        max_packet_size: Option<u32>,
    ) -> Result<(Self, usize), DecodeError> {
        let (header, header_len) = FixedHeader::decode(buf)?;
        let total = header_len + header.remaining_length as usize;
        if let Some(max) = max_packet_size
            && total > max as usize
        {
            return Err(DecodeError::PacketTooLarge { size: total, max });
        }
        if buf.len() < total {
            return Err(DecodeError::Incomplete { needed: total });
        }
        let body = &buf[header_len..total];
        let packet = Self::decode_body(header.packet_type, header.flags, body)?;
        Ok((packet, total))
    }

    fn decode_body(packet_type: PacketType, flags: u8, body: &[u8]) -> Result<Self, DecodeError> {
        match packet_type {
            PacketType::Connect => Ok(Self::Connect(Connect::decode_body(body)?)),
            PacketType::Connack => Ok(Self::Connack(Connack::decode_body(body)?)),
            PacketType::Publish => Ok(Self::Publish(Publish::decode_body(flags, body)?)),
            PacketType::Puback => Ok(Self::Puback(Ack::decode_body(body, PacketType::Puback)?)),
            PacketType::Pubrec => Ok(Self::Pubrec(Ack::decode_body(body, PacketType::Pubrec)?)),
            PacketType::Pubrel => Ok(Self::Pubrel(Ack::decode_body(body, PacketType::Pubrel)?)),
            PacketType::Pubcomp => Ok(Self::Pubcomp(Ack::decode_body(body, PacketType::Pubcomp)?)),
            PacketType::Subscribe => Ok(Self::Subscribe(Subscribe::decode_body(body)?)),
            PacketType::Suback => Ok(Self::Suback(Suback::decode_body(body)?)),
            PacketType::Unsubscribe => Ok(Self::Unsubscribe(Unsubscribe::decode_body(body)?)),
            PacketType::Unsuback => Ok(Self::Unsuback(Unsuback::decode_body(body)?)),
            PacketType::Pingreq => {
                if body.is_empty() {
                    Ok(Self::Pingreq)
                } else {
                    Err(DecodeError::Malformed("PINGREQ 报文体必须为空"))
                }
            }
            PacketType::Pingresp => {
                if body.is_empty() {
                    Ok(Self::Pingresp)
                } else {
                    Err(DecodeError::Malformed("PINGRESP 报文体必须为空"))
                }
            }
            PacketType::Disconnect => Ok(Self::Disconnect(Disconnect::decode_body(body)?)),
            PacketType::Auth => Ok(Self::Auth(Auth::decode_body(body)?)),
        }
    }

    /// 报文类型。
    pub fn packet_type(&self) -> PacketType {
        match self {
            Self::Connect(_) => PacketType::Connect,
            Self::Connack(_) => PacketType::Connack,
            Self::Publish(_) => PacketType::Publish,
            Self::Puback(_) => PacketType::Puback,
            Self::Pubrec(_) => PacketType::Pubrec,
            Self::Pubrel(_) => PacketType::Pubrel,
            Self::Pubcomp(_) => PacketType::Pubcomp,
            Self::Subscribe(_) => PacketType::Subscribe,
            Self::Suback(_) => PacketType::Suback,
            Self::Unsubscribe(_) => PacketType::Unsubscribe,
            Self::Unsuback(_) => PacketType::Unsuback,
            Self::Pingreq => PacketType::Pingreq,
            Self::Pingresp => PacketType::Pingresp,
            Self::Disconnect(_) => PacketType::Disconnect,
            Self::Auth(_) => PacketType::Auth,
        }
    }

    /// 编码后的总长度（含固定头）。
    pub fn encoded_len(&self) -> Result<usize, EncodeError> {
        let body_len = self.body_len()?;
        Ok(1 + varint::encoded_len(body_len as u32) + body_len)
    }

    /// 编码为独立字节向量。
    pub fn encode(&self) -> Result<Vec<u8>, EncodeError> {
        let body_len = self.body_len()?;
        let total = 1 + varint::encoded_len(body_len as u32) + body_len;
        let mut out = Vec::new();
        out.try_reserve_exact(total)
            .map_err(|_| EncodeError::AllocationFailed)?;
        self.write_with_body_len(&mut out, body_len)?;
        Ok(out)
    }

    /// 追加编码到已有字节向量。
    pub fn encode_to(&self, out: &mut Vec<u8>) -> Result<(), EncodeError> {
        let body_len = self.body_len()?;
        let total = 1 + varint::encoded_len(body_len as u32) + body_len;
        out.try_reserve(total)
            .map_err(|_| EncodeError::AllocationFailed)?;
        self.write_with_body_len(out, body_len)
    }

    /// 用计数 sink 计算报文体长度；与真正编码共用同一套编码逻辑。
    fn body_len(&self) -> Result<usize, EncodeError> {
        let mut counter = CountSink::new();
        self.encode_body(&mut counter)?;
        let body_len = counter.len();
        if body_len > varint::MAX as usize {
            return Err(EncodeError::PacketTooLarge {
                size: 1 + 4 + body_len,
            });
        }
        Ok(body_len)
    }

    fn write_with_body_len(&self, out: &mut Vec<u8>, body_len: usize) -> Result<(), EncodeError> {
        let header = FixedHeader {
            packet_type: self.packet_type(),
            flags: self.flags(),
            remaining_length: body_len as u32,
        };
        // 先编码报文体到临时缓冲区，避免失败时在调用方的 `out` 里留下半截报文。
        let mut body = Vec::new();
        body.try_reserve_exact(body_len)
            .map_err(|_| EncodeError::AllocationFailed)?;
        self.encode_body(&mut VecSink::new(&mut body))?;
        header.encode_to(out)?;
        out.extend_from_slice(&body);
        Ok(())
    }

    fn flags(&self) -> u8 {
        match self {
            Self::Publish(publish) => publish.flags_byte(),
            _ => self.packet_type().default_flags(),
        }
    }

    fn encode_body<S: Sink + ?Sized>(&self, sink: &mut S) -> Result<(), EncodeError> {
        match self {
            Self::Connect(packet) => packet.encode_body(sink),
            Self::Connack(packet) => packet.encode_body(sink),
            Self::Publish(packet) => packet.encode_body(sink),
            Self::Puback(packet) => packet.encode_body(sink, PacketType::Puback),
            Self::Pubrec(packet) => packet.encode_body(sink, PacketType::Pubrec),
            Self::Pubrel(packet) => packet.encode_body(sink, PacketType::Pubrel),
            Self::Pubcomp(packet) => packet.encode_body(sink, PacketType::Pubcomp),
            Self::Subscribe(packet) => packet.encode_body(sink),
            Self::Suback(packet) => packet.encode_body(sink),
            Self::Unsubscribe(packet) => packet.encode_body(sink),
            Self::Unsuback(packet) => packet.encode_body(sink),
            Self::Pingreq | Self::Pingresp => Ok(()),
            Self::Disconnect(packet) => packet.encode_body(sink),
            Self::Auth(packet) => packet.encode_body(sink),
        }
    }
}

/// 解析包 ID，并校验非零。
pub(crate) fn decode_packet_id(reader: &mut Reader<'_>) -> Result<u16, DecodeError> {
    let value = reader.u16()?;
    if value == 0 {
        return Err(DecodeError::ProtocolError("包 ID 不能为 0"));
    }
    Ok(value)
}

/// 解析原因码：未知字节 -> `Malformed`；已知但该报文不允许 -> `ProtocolError`。
pub(crate) fn decode_reason_code(
    value: u8,
    packet_type: PacketType,
) -> Result<ReasonCode, DecodeError> {
    match ReasonCode::try_from_u8(value) {
        None => Err(DecodeError::Malformed("未知原因码")),
        Some(code) if !code.allowed_in(packet_type) => {
            Err(DecodeError::ProtocolError("原因码不允许用于该报文"))
        }
        Some(code) => Ok(code),
    }
}

/// 编码前校验原因码是否允许用于该报文。
pub(crate) fn encode_reason_code(
    code: ReasonCode,
    packet_type: PacketType,
) -> Result<(), EncodeError> {
    if !code.allowed_in(packet_type) {
        return Err(EncodeError::Invalid("原因码不允许用于该报文"));
    }
    Ok(())
}

/// PUBACK 系列报文对应的属性上下文。
pub(crate) fn ack_property_context(packet_type: PacketType) -> PropertyContext {
    match packet_type {
        PacketType::Puback => PropertyContext::Puback,
        PacketType::Pubrec => PropertyContext::Pubrec,
        PacketType::Pubrel => PropertyContext::Pubrel,
        PacketType::Pubcomp => PropertyContext::Pubcomp,
        _ => PropertyContext::Puback,
    }
}

/// 解析“可选原因码 + 可选属性段”的短报文体（PUBACK 系列、DISCONNECT、AUTH 共用）。
///
/// 长度分支（MQTT 5.0）：
/// - 无剩余字节：原因码取 `default_reason`，无属性；
/// - 1 字节：仅原因码，无属性；
/// - 2 字节及以上：原因码 + 属性段（属性段可为长度 0）。
pub(crate) fn decode_optional_reason_and_properties(
    reader: &mut Reader<'_>,
    packet_type: PacketType,
    default_reason: ReasonCode,
    context: PropertyContext,
) -> Result<(ReasonCode, Properties), DecodeError> {
    if reader.is_empty() {
        return Ok((default_reason, Properties::new()));
    }
    let reason_code = decode_reason_code(reader.u8()?, packet_type)?;
    if reader.is_empty() {
        return Ok((reason_code, Properties::new()));
    }
    let properties = Properties::decode(reader, context)?;
    reader.finish()?;
    Ok((reason_code, properties))
}

/// 编码“可选原因码 + 可选属性段”，使用最短合法形式。
pub(crate) fn encode_optional_reason_and_properties<S: Sink + ?Sized>(
    sink: &mut S,
    reason_code: ReasonCode,
    properties: &Properties,
    packet_type: PacketType,
    context: PropertyContext,
) -> Result<(), EncodeError> {
    encode_reason_code(reason_code, packet_type)?;
    if reason_code == ReasonCode::Success && properties.is_empty() {
        return Ok(());
    }
    write_u8(sink, reason_code.as_u8());
    if !properties.is_empty() {
        properties.encode_into(sink, context)?;
    }
    Ok(())
}
