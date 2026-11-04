//! Worker控制平面的输入流（Stdin）帧。
//!
//! `Stdin` 是 Router 下发给 Worker 的命令信封；载荷按 [`Kind`] 解释：
//! `Task`、`ToolCall`、`Jump`。帧格式（大端）：
//!
//! ```text
//! 偏移  字段         类型   说明
//! 0     version      u8     协议版本，必须为 1
//! 1     kind         u8     1=Task 2=ToolCall 3=Jump
//! 2     id           u64    命令 id，非 0
//! 10    timestamp    u64    服务端 Unix 毫秒；0=未知
//! 18    delay_ms     u32    相对收到时刻的延迟，0=立即
//! 22    ddl_ms       u32    执行预算，0=使用默认值
//! 26    payload_len  u32    载荷长度，≤ 4096
//! 30    payload      [u8]   类型相关载荷
//! ```
//!
//! 本模块只负责严格的编解码与基础语义校验，不执行命令。

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;

use crate::mqtt::codec::{Reader, Sink, VecSink, write_u8, write_u32, write_u64, write_utf8};
use crate::sekai_wire::{
    MAX_DDL_MS, MAX_DELAY_MS, MAX_PAYLOAD, PROTOCOL_VERSION, ProtocolEncodeError, ProtocolError,
    validate_name, validate_target,
};

/// Stdin 帧固定头长度（字节）。
pub const STDIN_HEADER_LEN: usize = 30;

/// Stdin 命令类型。
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// 下发任务。
    Task = 1,
    /// 调用工具。
    ToolCall = 2,
    /// 迁移控制链路。
    Jump = 3,
}

impl Kind {
    /// 线上 u8 值。
    pub const fn as_u8(self) -> u8 {
        self as u8
    }

    /// 由线上值构造；未知值返回 [`ProtocolError::UnknownTag`]。
    pub const fn from_u8(value: u8) -> Result<Self, ProtocolError> {
        Ok(match value {
            1 => Self::Task,
            2 => Self::ToolCall,
            3 => Self::Jump,
            other => return Err(ProtocolError::UnknownTag(other)),
        })
    }
}

impl fmt::Display for Kind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Task => write!(f, "Task"),
            Self::ToolCall => write!(f, "ToolCall"),
            Self::Jump => write!(f, "Jump"),
        }
    }
}

/// Stdin 帧。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stdin {
    /// 命令 id；由服务端按Worker单调分配，不能为 0。
    pub id: u64,
    /// 命令类型。
    pub kind: Kind,
    /// 服务端 Unix 毫秒；0 表示未知。
    pub timestamp: u64,
    /// 相对收到时刻的延迟（毫秒）；0 表示立即。
    pub delay_ms: u32,
    /// 执行预算（毫秒）；0 表示使用默认值。
    pub ddl_ms: u32,
    /// 类型相关载荷。
    pub payload: Vec<u8>,
}

impl Stdin {
    /// 构造一个 Stdin 帧并做基础校验。
    pub fn new(
        id: u64,
        kind: Kind,
        timestamp: u64,
        delay_ms: u32,
        ddl_ms: u32,
        payload: Vec<u8>,
    ) -> Result<Self, ProtocolError> {
        let frame = Self {
            id,
            kind,
            timestamp,
            delay_ms,
            ddl_ms,
            payload,
        };
        frame.validate_new()?;
        Ok(frame)
    }

    /// 编码为独立字节向量。
    pub fn encode(&self) -> Result<Vec<u8>, ProtocolEncodeError> {
        self.validate_encode()?;
        let total = STDIN_HEADER_LEN.checked_add(self.payload.len()).ok_or(
            ProtocolEncodeError::TooLarge {
                len: usize::MAX,
                max: MAX_PAYLOAD,
            },
        )?;
        let mut out = Vec::new();
        out.try_reserve_exact(total)
            .map_err(|_| ProtocolEncodeError::AllocationFailed)?;
        {
            let mut sink = VecSink::new(&mut out);
            write_u8(&mut sink, PROTOCOL_VERSION);
            write_u8(&mut sink, self.kind.as_u8());
            write_u64(&mut sink, self.id);
            write_u64(&mut sink, self.timestamp);
            write_u32(&mut sink, self.delay_ms);
            write_u32(&mut sink, self.ddl_ms);
            write_u32(&mut sink, self.payload.len() as u32);
            sink.write(&self.payload);
        }
        Ok(out)
    }

    /// 从完整帧解码；不会消费帧之外的字节。
    ///
    /// 返回 [`StdinError`]，其中 `cmd_id`/`kind` 在头部可解析时会带上，
    /// 便于运行时回 `Failed` 事件。
    pub fn decode(frame: &[u8]) -> Result<Self, StdinError> {
        let mut cmd_id = None;
        let mut kind = None;
        let result = (|| -> Result<Self, ProtocolError> {
            let mut reader = Reader::new(frame);
            let version = reader.u8()?;
            if version != PROTOCOL_VERSION {
                return Err(ProtocolError::UnsupportedVersion(version));
            }
            let parsed_kind = Kind::from_u8(reader.u8()?)?;
            kind = Some(parsed_kind);
            let parsed_id = reader.u64()?;
            cmd_id = Some(parsed_id);
            let timestamp = reader.u64()?;
            let delay_ms = reader.u32()?;
            let ddl_ms = reader.u32()?;
            let payload_len = reader.u32()? as usize;
            if payload_len > MAX_PAYLOAD {
                return Err(ProtocolError::PayloadTooLarge {
                    len: payload_len,
                    max: MAX_PAYLOAD,
                });
            }
            let raw = reader.take(payload_len)?;
            let mut payload = Vec::new();
            payload
                .try_reserve_exact(raw.len())
                .map_err(|_| ProtocolError::AllocationFailed)?;
            payload.extend_from_slice(raw);
            if !reader.is_empty() {
                return Err(ProtocolError::TrailingBytes);
            }
            Self::new(parsed_id, parsed_kind, timestamp, delay_ms, ddl_ms, payload)
        })();
        result.map_err(|error| StdinError {
            cmd_id,
            kind,
            error,
        })
    }

    fn validate_new(&self) -> Result<(), ProtocolError> {
        if self.id == 0 {
            return Err(ProtocolError::InvalidField("命令 id 不能为 0"));
        }
        if self.delay_ms > MAX_DELAY_MS {
            return Err(ProtocolError::InvalidField("delay_ms 超过 24 小时"));
        }
        if self.ddl_ms > MAX_DDL_MS {
            return Err(ProtocolError::InvalidField("ddl_ms 超过 5 分钟"));
        }
        if self.payload.is_empty() {
            return Err(ProtocolError::InvalidField("载荷不能为空"));
        }
        if self.payload.len() > MAX_PAYLOAD {
            return Err(ProtocolError::PayloadTooLarge {
                len: self.payload.len(),
                max: MAX_PAYLOAD,
            });
        }
        Ok(())
    }

    fn validate_encode(&self) -> Result<(), ProtocolEncodeError> {
        if self.id == 0 {
            return Err(ProtocolEncodeError::InvalidField("命令 id 不能为 0"));
        }
        if self.delay_ms > MAX_DELAY_MS {
            return Err(ProtocolEncodeError::InvalidField("delay_ms 超过 24 小时"));
        }
        if self.ddl_ms > MAX_DDL_MS {
            return Err(ProtocolEncodeError::InvalidField("ddl_ms 超过 5 分钟"));
        }
        if self.payload.is_empty() {
            return Err(ProtocolEncodeError::InvalidField("载荷不能为空"));
        }
        if self.payload.len() > MAX_PAYLOAD {
            return Err(ProtocolEncodeError::TooLarge {
                len: self.payload.len(),
                max: MAX_PAYLOAD,
            });
        }
        Ok(())
    }
}

/// Stdin 解码错误，携带可解析出的命令 id/类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StdinError {
    /// 头部解析出的命令 id；头部不完整时为 `None`。
    pub cmd_id: Option<u64>,
    /// 头部解析出的命令类型；头部不完整时为 `None`。
    pub kind: Option<Kind>,
    /// 具体错误。
    pub error: ProtocolError,
}

impl fmt::Display for StdinError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match (self.cmd_id, self.kind) {
            (Some(id), Some(kind)) => write!(f, "{kind} 命令 {id} 解码失败：{}", self.error),
            _ => write!(f, "Stdin 解码失败：{}", self.error),
        }
    }
}

impl core::error::Error for StdinError {}

/// 编码「名称 + 参数」形式的载荷。
fn encode_named(name: &str, args: &[u8]) -> Result<Vec<u8>, ProtocolEncodeError> {
    validate_name(name).map_err(ProtocolEncodeError::InvalidField)?;
    let total = 2usize
        .checked_add(name.len())
        .and_then(|value| value.checked_add(args.len()))
        .ok_or(ProtocolEncodeError::TooLarge {
            len: usize::MAX,
            max: MAX_PAYLOAD,
        })?;
    if total > MAX_PAYLOAD {
        return Err(ProtocolEncodeError::TooLarge {
            len: total,
            max: MAX_PAYLOAD,
        });
    }
    let mut out = Vec::new();
    out.try_reserve_exact(total)
        .map_err(|_| ProtocolEncodeError::AllocationFailed)?;
    {
        let mut sink = VecSink::new(&mut out);
        write_utf8(&mut sink, name)?;
        sink.write(args);
    }
    Ok(out)
}

/// 解码「名称 + 参数」形式的载荷。
fn decode_named(payload: &[u8]) -> Result<(String, Vec<u8>), ProtocolError> {
    if payload.len() > MAX_PAYLOAD {
        return Err(ProtocolError::PayloadTooLarge {
            len: payload.len(),
            max: MAX_PAYLOAD,
        });
    }
    let mut reader = Reader::new(payload);
    let name = reader.utf8()?;
    validate_name(&name).map_err(ProtocolError::InvalidField)?;
    let remaining = reader.remaining();
    let raw_args = reader.take(remaining)?;
    let mut args = Vec::new();
    args.try_reserve_exact(raw_args.len())
        .map_err(|_| ProtocolError::AllocationFailed)?;
    args.extend_from_slice(raw_args);
    Ok((name, args))
}

/// Task 命令载荷。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskBody {
    /// 已注册的任务名。
    pub name: String,
    /// 任务参数；由任务处理器自行解释。
    pub args: Vec<u8>,
}

impl TaskBody {
    /// 构造任务载荷并校验名称/长度。
    pub fn new(name: impl Into<String>, args: Vec<u8>) -> Result<Self, ProtocolError> {
        let name = name.into();
        validate_name(&name).map_err(ProtocolError::InvalidField)?;
        if args.len() > MAX_PAYLOAD {
            return Err(ProtocolError::PayloadTooLarge {
                len: args.len(),
                max: MAX_PAYLOAD,
            });
        }
        Ok(Self { name, args })
    }

    /// 编码为 Stdin 载荷。
    pub fn encode(&self) -> Result<Vec<u8>, ProtocolEncodeError> {
        encode_named(&self.name, &self.args)
    }

    /// 从 Stdin 载荷解码。
    pub fn decode(payload: &[u8]) -> Result<Self, ProtocolError> {
        let (name, args) = decode_named(payload)?;
        Ok(Self { name, args })
    }
}

/// Tool 命令载荷。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolBody {
    /// 已注册的工具名。
    pub name: String,
    /// 工具参数；由工具处理器自行解释。
    pub args: Vec<u8>,
}

impl ToolBody {
    /// 构造工具载荷并校验名称/长度。
    pub fn new(name: impl Into<String>, args: Vec<u8>) -> Result<Self, ProtocolError> {
        let name = name.into();
        validate_name(&name).map_err(ProtocolError::InvalidField)?;
        if args.len() > MAX_PAYLOAD {
            return Err(ProtocolError::PayloadTooLarge {
                len: args.len(),
                max: MAX_PAYLOAD,
            });
        }
        Ok(Self { name, args })
    }

    /// 编码为 Stdin 载荷。
    pub fn encode(&self) -> Result<Vec<u8>, ProtocolEncodeError> {
        encode_named(&self.name, &self.args)
    }

    /// 从 Stdin 载荷解码。
    pub fn decode(payload: &[u8]) -> Result<Self, ProtocolError> {
        let (name, args) = decode_named(payload)?;
        Ok(Self { name, args })
    }
}

/// Jump 命令载荷。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JumpBody {
    /// 目标：DeviceList 中的名称/IP，或允许的 `host:port`。
    pub target: String,
}

impl JumpBody {
    /// 构造跳转载荷并校验目标字符串。
    pub fn new(target: impl Into<String>) -> Result<Self, ProtocolError> {
        let target = target.into();
        validate_target(&target).map_err(ProtocolError::InvalidField)?;
        Ok(Self { target })
    }

    /// 编码为 Stdin 载荷。
    pub fn encode(&self) -> Result<Vec<u8>, ProtocolEncodeError> {
        validate_target(&self.target).map_err(ProtocolEncodeError::InvalidField)?;
        let total = 2usize
            .checked_add(self.target.len())
            .ok_or(ProtocolEncodeError::TooLarge {
                len: usize::MAX,
                max: MAX_PAYLOAD,
            })?;
        if total > MAX_PAYLOAD {
            return Err(ProtocolEncodeError::TooLarge {
                len: total,
                max: MAX_PAYLOAD,
            });
        }
        let mut out = Vec::new();
        out.try_reserve_exact(total)
            .map_err(|_| ProtocolEncodeError::AllocationFailed)?;
        {
            let mut sink = VecSink::new(&mut out);
            write_utf8(&mut sink, &self.target)?;
        }
        Ok(out)
    }

    /// 从 Stdin 载荷解码。
    pub fn decode(payload: &[u8]) -> Result<Self, ProtocolError> {
        let mut reader = Reader::new(payload);
        let target = reader.utf8()?;
        validate_target(&target).map_err(ProtocolError::InvalidField)?;
        if !reader.is_empty() {
            return Err(ProtocolError::TrailingBytes);
        }
        Ok(Self { target })
    }
}

#[cfg(test)]
mod tests {
    use super::{JumpBody, Kind, STDIN_HEADER_LEN, Stdin, StdinError, TaskBody, ToolBody};
    use crate::sekai_wire::{
        MAX_DDL_MS, MAX_DELAY_MS, MAX_PAYLOAD, PROTOCOL_VERSION, ProtocolError,
    };
    use alloc::string::ToString;
    use alloc::vec;
    use alloc::vec::Vec;

    fn task_frame(id: u64) -> Stdin {
        let body = TaskBody::new("echo", b"hello".to_vec()).unwrap();
        Stdin::new(
            id,
            Kind::Task,
            1_700_000_000_000,
            0,
            500,
            body.encode().unwrap(),
        )
        .unwrap()
    }

    #[test]
    fn kind_roundtrip() {
        for (value, kind) in [(1, Kind::Task), (2, Kind::ToolCall), (3, Kind::Jump)] {
            assert_eq!(kind.as_u8(), value);
            assert_eq!(Kind::from_u8(value), Ok(kind));
        }
        assert_eq!(Kind::from_u8(0), Err(ProtocolError::UnknownTag(0)));
        assert_eq!(Kind::from_u8(4), Err(ProtocolError::UnknownTag(4)));
        assert_eq!(Kind::Task.to_string(), "Task");
    }

    #[test]
    fn stdin_roundtrip_all_kinds() {
        for kind in [Kind::Task, Kind::ToolCall, Kind::Jump] {
            let stdin = Stdin::new(
                42,
                kind,
                u64::MAX,
                MAX_DELAY_MS,
                MAX_DDL_MS,
                vec![1, 2, 3, 4],
            )
            .unwrap();
            let frame = stdin.encode().unwrap();
            assert_eq!(frame.len(), STDIN_HEADER_LEN + 4);
            assert_eq!(frame[0], PROTOCOL_VERSION);
            assert_eq!(frame[1], kind.as_u8());
            assert_eq!(Stdin::decode(&frame).unwrap(), stdin);
        }
    }

    #[test]
    fn stdin_rejects_invalid_fields() {
        assert!(matches!(
            Stdin::new(0, Kind::Task, 0, 0, 0, vec![1]).unwrap_err(),
            ProtocolError::InvalidField(_)
        ));
        assert!(matches!(
            Stdin::new(1, Kind::Task, 0, MAX_DELAY_MS + 1, 0, vec![1]).unwrap_err(),
            ProtocolError::InvalidField(_)
        ));
        assert!(matches!(
            Stdin::new(1, Kind::Task, 0, 0, MAX_DDL_MS + 1, vec![1]).unwrap_err(),
            ProtocolError::InvalidField(_)
        ));
        assert!(matches!(
            Stdin::new(1, Kind::Task, 0, 0, 0, Vec::new()).unwrap_err(),
            ProtocolError::InvalidField(_)
        ));
        assert!(matches!(
            Stdin::new(1, Kind::Task, 0, 0, 0, vec![0; MAX_PAYLOAD + 1]).unwrap_err(),
            ProtocolError::PayloadTooLarge { .. }
        ));
    }

    #[test]
    fn decode_rejects_bad_version_and_tags() {
        let frame = task_frame(7).encode().unwrap();
        let mut bad_version = frame.clone();
        bad_version[0] = 9;
        let error = Stdin::decode(&bad_version).unwrap_err();
        assert_eq!(error.error, ProtocolError::UnsupportedVersion(9));
        assert_eq!(error.cmd_id, None);

        let mut bad_kind = frame.clone();
        bad_kind[1] = 9;
        let error = Stdin::decode(&bad_kind).unwrap_err();
        assert_eq!(error.error, ProtocolError::UnknownTag(9));
        // kind 解析失败前没有 cmd_id。
        assert_eq!(error.cmd_id, None);
    }

    #[test]
    fn decode_reports_partial_id() {
        let frame = task_frame(0x1122_3344_5566_7788).encode().unwrap();
        // 截断在 id 之后、payload 之前，头部可解析出 id/kind。
        let error = Stdin::decode(&frame[..12]).unwrap_err();
        assert_eq!(error.error, ProtocolError::Truncated);
        assert_eq!(error.cmd_id, Some(0x1122_3344_5566_7788));
        assert_eq!(error.kind, Some(Kind::Task));
    }

    #[test]
    fn decode_rejects_truncation_and_trailing() {
        let frame = task_frame(1).encode().unwrap();
        assert_eq!(
            Stdin::decode(&frame[..frame.len() - 1]).unwrap_err().error,
            ProtocolError::Truncated
        );
        let mut trailing = frame.clone();
        trailing.push(0);
        assert_eq!(
            Stdin::decode(&trailing).unwrap_err().error,
            ProtocolError::TrailingBytes
        );
    }

    #[test]
    fn decode_rejects_oversized_payload_length() {
        let mut frame = task_frame(1).encode().unwrap();
        let len_offset = 26;
        frame[len_offset..len_offset + 4].copy_from_slice(&(MAX_PAYLOAD as u32 + 1).to_be_bytes());
        // 长度字段超过上限时，不需要真的提供那么多字节。
        let error = Stdin::decode(&frame).unwrap_err();
        assert_eq!(
            error.error,
            ProtocolError::PayloadTooLarge {
                len: MAX_PAYLOAD + 1,
                max: MAX_PAYLOAD
            }
        );
    }

    #[test]
    fn decode_zero_id() {
        let mut frame = task_frame(1).encode().unwrap();
        frame[2..10].copy_from_slice(&0u64.to_be_bytes());
        let error = Stdin::decode(&frame).unwrap_err();
        assert!(matches!(error.error, ProtocolError::InvalidField(_)));
        assert_eq!(error.cmd_id, Some(0));
    }

    #[test]
    fn task_and_tool_bodies_roundtrip() {
        let task = TaskBody::new("do_thing", b"args".to_vec()).unwrap();
        assert_eq!(TaskBody::decode(&task.encode().unwrap()).unwrap(), task);

        let tool = ToolBody::new("gpio.read", vec![1, 2, 3]).unwrap();
        assert_eq!(ToolBody::decode(&tool.encode().unwrap()).unwrap(), tool);
    }

    #[test]
    fn bodies_reject_invalid_names() {
        assert!(TaskBody::new("", Vec::new()).is_err());
        assert!(TaskBody::new("BadName", Vec::new()).is_err());
        assert!(ToolBody::new("a b", Vec::new()).is_err());
        let long = "a".repeat(49);
        assert!(TaskBody::new(long, Vec::new()).is_err());
    }

    #[test]
    fn jump_body_roundtrip() {
        let jump = JumpBody::new("peer-1").unwrap();
        assert_eq!(JumpBody::decode(&jump.encode().unwrap()).unwrap(), jump);
        assert!(JumpBody::new("").is_err());
        assert!(JumpBody::new("a b").is_err());
    }

    #[test]
    fn jump_body_rejects_trailing_bytes() {
        let mut payload = JumpBody::new("peer-1").unwrap().encode().unwrap();
        payload.push(0);
        assert_eq!(
            JumpBody::decode(&payload),
            Err(ProtocolError::TrailingBytes)
        );
    }

    #[test]
    fn error_display_mentions_id() {
        let error = StdinError {
            cmd_id: Some(9),
            kind: Some(Kind::ToolCall),
            error: ProtocolError::Truncated,
        };
        let text = alloc::format!("{error}");
        assert!(text.contains("ToolCall"));
        assert!(text.contains('9'));
    }
}
