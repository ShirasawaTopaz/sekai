//! MQTT 报文与控制平面协议共用的基础编解码工具。
//!
//! 只在本 crate 内部使用：
//! - [Reader]：从完整报文体中按顺序读取基本字段；
//! - [Sink]：编码输出抽象。`CountSink` 只统计长度，`VecSink` 写入字节，
//!   两者共用同一套编码逻辑，避免“计算长度”和“真正编码”两份实现分叉。

use alloc::string::String;
use alloc::vec::Vec;

use crate::mqtt::error::{DecodeError, EncodeError};
use crate::mqtt::varint;

/// 从字节切片顺序读取 MQTT 基本类型。
pub(crate) struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    pub(crate) fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    /// 剩余可读字节数。
    pub(crate) fn remaining(&self) -> usize {
        self.buf.len() - self.pos
    }

    /// 是否已读完。
    pub(crate) fn is_empty(&self) -> bool {
        self.remaining() == 0
    }

    /// 读取 `len` 个字节；长度不足返回 `Malformed`。
    pub(crate) fn take(&mut self, len: usize) -> Result<&'a [u8], DecodeError> {
        if self.remaining() < len {
            return Err(DecodeError::Malformed("字段长度超出报文剩余长度"));
        }
        let start = self.pos;
        self.pos += len;
        Ok(&self.buf[start..self.pos])
    }

    /// 读取一个无符号字节。
    pub(crate) fn u8(&mut self) -> Result<u8, DecodeError> {
        Ok(self.take(1)?[0])
    }

    /// 读取大端无符号 16 位整数。
    pub(crate) fn u16(&mut self) -> Result<u16, DecodeError> {
        let bytes = self.take(2)?;
        Ok(u16::from_be_bytes([bytes[0], bytes[1]]))
    }

    /// 读取大端无符号 32 位整数。
    pub(crate) fn u32(&mut self) -> Result<u32, DecodeError> {
        let bytes = self.take(4)?;
        Ok(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    /// 读取大端无符号 64 位整数。
    pub(crate) fn u64(&mut self) -> Result<u64, DecodeError> {
        let bytes = self.take(8)?;
        Ok(u64::from_be_bytes([
            bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
        ]))
    }

    /// 读取 Variable Byte Integer。
    ///
    /// 报文体内部不会出现“字节不完整”的情况，因此把 `Incomplete`
    /// 转成 `Malformed`：报文体已被报文总长度截断，说明编码非法。
    pub(crate) fn varint(&mut self) -> Result<u32, DecodeError> {
        match varint::decode(&self.buf[self.pos..]) {
            Ok((value, used)) => {
                self.pos += used;
                Ok(value)
            }
            Err(DecodeError::Incomplete { .. }) => {
                Err(DecodeError::Malformed("Variable Byte Integer 未终止"))
            }
            Err(err) => Err(err),
        }
    }

    /// 读取带 u16 长度前缀的原始字节。
    pub(crate) fn string_bytes(&mut self) -> Result<&'a [u8], DecodeError> {
        let len = self.u16()? as usize;
        self.take(len)
    }

    /// 读取 UTF-8 字符串（u16 长度前缀），校验合法 UTF-8 且不含 U+0000。
    pub(crate) fn utf8(&mut self) -> Result<String, DecodeError> {
        let raw = self.string_bytes()?;
        decode_utf8(raw)
    }

    /// 读取二进制数据（u16 长度前缀）。
    pub(crate) fn binary(&mut self) -> Result<Vec<u8>, DecodeError> {
        let raw = self.string_bytes()?;
        let mut out = Vec::new();
        out.try_reserve_exact(raw.len())
            .map_err(|_| DecodeError::AllocationFailed)?;
        out.extend_from_slice(raw);
        Ok(out)
    }

    /// 断言报文体已恰好读完；有多余字节返回 `Malformed`。
    pub(crate) fn finish(&self) -> Result<(), DecodeError> {
        if self.is_empty() {
            Ok(())
        } else {
            Err(DecodeError::Malformed("报文末尾存在多余字节"))
        }
    }
}

/// 解码 UTF-8 字符串：必须是合法 UTF-8 且不含 U+0000。
pub(crate) fn decode_utf8(raw: &[u8]) -> Result<String, DecodeError> {
    let text = core::str::from_utf8(raw).map_err(|_| DecodeError::Malformed("UTF-8 字符串非法"))?;
    if text.as_bytes().contains(&0) {
        return Err(DecodeError::Malformed("UTF-8 字符串包含 U+0000"));
    }
    let mut out = String::new();
    out.try_reserve_exact(text.len())
        .map_err(|_| DecodeError::AllocationFailed)?;
    out.push_str(text);
    Ok(out)
}

/// 编码输出目标。
pub(crate) trait Sink {
    /// 追加一段字节。
    fn write(&mut self, bytes: &[u8]);
}

/// 只统计长度的 sink，用于在真正编码前计算报文体长度。
pub(crate) struct CountSink {
    len: usize,
}

impl CountSink {
    pub(crate) fn new() -> Self {
        Self { len: 0 }
    }

    pub(crate) fn len(&self) -> usize {
        self.len
    }
}

impl Sink for CountSink {
    fn write(&mut self, bytes: &[u8]) {
        self.len += bytes.len();
    }
}

/// 写入 `Vec` 的 sink。
///
/// 正常路径下调用方已经通过 `Vec::try_reserve_exact` 预留足够容量，
/// 因此这里使用 `extend_from_slice` 不会触发额外分配。
pub(crate) struct VecSink<'a> {
    out: &'a mut Vec<u8>,
}

impl<'a> VecSink<'a> {
    pub(crate) fn new(out: &'a mut Vec<u8>) -> Self {
        Self { out }
    }
}

impl Sink for VecSink<'_> {
    fn write(&mut self, bytes: &[u8]) {
        self.out.extend_from_slice(bytes);
    }
}

/// 写入一个无符号字节。
pub(crate) fn write_u8<S: Sink + ?Sized>(sink: &mut S, value: u8) {
    sink.write(&[value]);
}

/// 写入大端无符号 16 位整数。
pub(crate) fn write_u16<S: Sink + ?Sized>(sink: &mut S, value: u16) {
    sink.write(&value.to_be_bytes());
}

/// 写入大端无符号 32 位整数。
pub(crate) fn write_u32<S: Sink + ?Sized>(sink: &mut S, value: u32) {
    sink.write(&value.to_be_bytes());
}

/// 写入大端无符号 64 位整数。
pub(crate) fn write_u64<S: Sink + ?Sized>(sink: &mut S, value: u64) {
    sink.write(&value.to_be_bytes());
}

/// 写入 Variable Byte Integer（调用方需保证 `value <= varint::MAX`）。
pub(crate) fn write_varint<S: Sink + ?Sized>(sink: &mut S, value: u32) {
    debug_assert!(value <= varint::MAX, "Variable Byte Integer 超出范围");
    let mut value = value;
    loop {
        let mut byte = (value % 128) as u8;
        value /= 128;
        if value > 0 {
            byte |= 0x80;
        }
        sink.write(&[byte]);
        if value == 0 {
            break;
        }
    }
}

/// 写入带 u16 长度前缀的 UTF-8 字符串。
pub(crate) fn write_utf8<S: Sink + ?Sized>(sink: &mut S, value: &str) -> Result<(), EncodeError> {
    let len = value.len();
    if len > u16::MAX as usize {
        return Err(EncodeError::StringTooLong {
            field: "UTF-8 字符串",
            len,
        });
    }
    if value.as_bytes().contains(&0) {
        return Err(EncodeError::Invalid("UTF-8 字符串不能包含 U+0000"));
    }
    write_u16(sink, len as u16);
    sink.write(value.as_bytes());
    Ok(())
}

/// 写入带 u16 长度前缀的二进制数据。
pub(crate) fn write_binary<S: Sink + ?Sized>(
    sink: &mut S,
    value: &[u8],
) -> Result<(), EncodeError> {
    let len = value.len();
    if len > u16::MAX as usize {
        return Err(EncodeError::StringTooLong {
            field: "二进制数据",
            len,
        });
    }
    write_u16(sink, len as u16);
    sink.write(value);
    Ok(())
}

/// 写入非零包 ID。
pub(crate) fn write_packet_id<S: Sink + ?Sized>(
    sink: &mut S,
    value: u16,
) -> Result<(), EncodeError> {
    if value == 0 {
        return Err(EncodeError::Invalid("包 ID 不能为 0"));
    }
    write_u16(sink, value);
    Ok(())
}
