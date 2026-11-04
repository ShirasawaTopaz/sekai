//! Worker控制平面的输出流（Stdout）帧。
//!
//! `Stdout` 是 Worker 上报给 Router 的信封；载荷按 [`Phase`] 解释：
//! 命令事件（Accepted/Started/Completed/Failed）、能力通告（Hello）
//! 与离线状态（Offline）。帧格式（大端）：
//!
//! ```text
//! 偏移  字段         类型   说明
//! 0     version      u8     协议版本，必须为 1
//! 1     phase        u8     1=Accepted ... 7=Offline
//! 2     boot_id      u64    本启动周期 id，非 0
//! 10    cmd_id       u64    对应 Stdin.id；Hello/Offline 为 0
//! 18    seq          u32    本 boot 内单调递增；LWT Offline 为 0
//! 22    timestamp    u64    Worker Unix 毫秒；未校准为 0
//! 30    payload_len  u32    载荷长度，≤ 4096
//! 34    payload      [u8]   类型相关载荷
//! ```
//!
//! 去重键是 `(worker_id, boot_id, seq)`；Router 必须忽略重复投递。

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;

use crate::mqtt::codec::{
    Reader, Sink, VecSink, write_u8, write_u16, write_u32, write_u64, write_utf8,
};
use crate::sekai_device_list::{DeviceList, Endpoint};
use crate::sekai_wire::{
    ErrorCode, MAX_ACTIVE_TASKS, MAX_ERROR_MESSAGE_LEN, MAX_FIRMWARE_LEN, MAX_PAYLOAD, MAX_TASKS,
    MAX_TOOLS, PROTOCOL_VERSION, ProtocolEncodeError, ProtocolError, truncate_utf8, validate_host,
    validate_name, validate_worker_id,
};

/// Stdout 帧固定头长度（字节）。
pub const STDOUT_HEADER_LEN: usize = 34;

/// Stdout 事件阶段。
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// 命令已校验并排队/开始。
    Accepted = 1,
    /// Task 首次开始推进。
    Started = 2,
    /// 预留：进度输出，v1 不发送。
    Progress = 3,
    /// 最终成功。
    Completed = 4,
    /// 最终失败。
    Failed = 5,
    /// 能力/状态通告。
    Hello = 6,
    /// 离线状态。
    Offline = 7,
}

impl Phase {
    /// 线上 u8 值。
    pub const fn as_u8(self) -> u8 {
        self as u8
    }

    /// 由线上值构造；未知值返回 [`ProtocolError::UnknownTag`]。
    pub const fn from_u8(value: u8) -> Result<Self, ProtocolError> {
        Ok(match value {
            1 => Self::Accepted,
            2 => Self::Started,
            3 => Self::Progress,
            4 => Self::Completed,
            5 => Self::Failed,
            6 => Self::Hello,
            7 => Self::Offline,
            other => return Err(ProtocolError::UnknownTag(other)),
        })
    }

    /// 是否是命令的最终事件。
    pub const fn is_final(self) -> bool {
        matches!(self, Self::Completed | Self::Failed)
    }
}

impl fmt::Display for Phase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            Self::Accepted => "Accepted",
            Self::Started => "Started",
            Self::Progress => "Progress",
            Self::Completed => "Completed",
            Self::Failed => "Failed",
            Self::Hello => "Hello",
            Self::Offline => "Offline",
        };
        f.write_str(name)
    }
}

/// Stdout 帧。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stdout {
    /// 本启动周期 id，非 0。
    pub boot_id: u64,
    /// 对应命令 id；Hello/Offline 为 0。
    pub cmd_id: u64,
    /// 本 boot 内的事件序号；LWT Offline 为 0。
    pub seq: u32,
    /// 事件阶段。
    pub phase: Phase,
    /// Worker Unix 毫秒；未校准为 0。
    pub timestamp: u64,
    /// 类型相关载荷。
    pub payload: Vec<u8>,
}

impl Stdout {
    /// 构造 Stdout 帧并校验阶段规则。
    pub fn new(
        boot_id: u64,
        cmd_id: u64,
        seq: u32,
        phase: Phase,
        timestamp: u64,
        payload: Vec<u8>,
    ) -> Result<Self, ProtocolError> {
        if payload.len() > MAX_PAYLOAD {
            return Err(ProtocolError::PayloadTooLarge {
                len: payload.len(),
                max: MAX_PAYLOAD,
            });
        }
        let frame = Self {
            boot_id,
            cmd_id,
            seq,
            phase,
            timestamp,
            payload,
        };
        frame.validate().map_err(ProtocolError::InvalidField)?;
        Ok(frame)
    }

    /// 是否是最终事件（Completed/Failed）。
    pub fn is_final(&self) -> bool {
        self.phase.is_final()
    }

    /// 编码为独立字节向量。
    pub fn encode(&self) -> Result<Vec<u8>, ProtocolEncodeError> {
        if self.payload.len() > MAX_PAYLOAD {
            return Err(ProtocolEncodeError::TooLarge {
                len: self.payload.len(),
                max: MAX_PAYLOAD,
            });
        }
        self.validate().map_err(ProtocolEncodeError::InvalidField)?;
        let total = STDOUT_HEADER_LEN.checked_add(self.payload.len()).ok_or(
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
            write_u8(&mut sink, self.phase.as_u8());
            write_u64(&mut sink, self.boot_id);
            write_u64(&mut sink, self.cmd_id);
            write_u32(&mut sink, self.seq);
            write_u64(&mut sink, self.timestamp);
            write_u32(&mut sink, self.payload.len() as u32);
            sink.write(&self.payload);
        }
        Ok(out)
    }

    /// 从完整帧解码。
    pub fn decode(frame: &[u8]) -> Result<Self, ProtocolError> {
        let mut reader = Reader::new(frame);
        let version = reader.u8()?;
        if version != PROTOCOL_VERSION {
            return Err(ProtocolError::UnsupportedVersion(version));
        }
        let phase = Phase::from_u8(reader.u8()?)?;
        let boot_id = reader.u64()?;
        let cmd_id = reader.u64()?;
        let seq = reader.u32()?;
        let timestamp = reader.u64()?;
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
        Self::new(boot_id, cmd_id, seq, phase, timestamp, payload)
    }

    fn validate(&self) -> Result<(), &'static str> {
        if self.boot_id == 0 {
            return Err("boot_id 不能为 0");
        }
        match self.phase {
            Phase::Hello | Phase::Offline => {
                if self.cmd_id != 0 {
                    return Err("Hello/Offline 的 cmd_id 必须为 0");
                }
            }
            _ => {
                if self.cmd_id == 0 {
                    return Err("命令事件的 cmd_id 不能为 0");
                }
            }
        }
        if self.phase == Phase::Offline {
            if !self.payload.is_empty() {
                return Err("Offline 载荷必须为空");
            }
        } else if self.seq == 0 {
            return Err("除 LWT Offline 外 seq 不能为 0");
        }
        Ok(())
    }
}

/// `Failed` 事件的载荷。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ErrorPayload {
    /// 错误码。
    pub code: ErrorCode,
    /// 可读描述，最多 256 字节。
    pub message: String,
}

impl ErrorPayload {
    /// 构造错误载荷；消息超过 256 字节时按字符边界截断。
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        let message = message.into().replace('\0', " ");
        Self {
            code,
            message: String::from(truncate_utf8(&message, MAX_ERROR_MESSAGE_LEN)),
        }
    }

    /// 编码为 `Failed` 载荷。
    pub fn encode(&self) -> Result<Vec<u8>, ProtocolEncodeError> {
        if self.message.len() > MAX_ERROR_MESSAGE_LEN {
            return Err(ProtocolEncodeError::InvalidField("错误消息超过 256 字节"));
        }
        let total = 2usize
            .checked_add(2)
            .and_then(|value| value.checked_add(self.message.len()))
            .ok_or(ProtocolEncodeError::TooLarge {
                len: usize::MAX,
                max: MAX_PAYLOAD,
            })?;
        let mut out = Vec::new();
        out.try_reserve_exact(total)
            .map_err(|_| ProtocolEncodeError::AllocationFailed)?;
        {
            let mut sink = VecSink::new(&mut out);
            write_u16(&mut sink, self.code.as_u16());
            write_utf8(&mut sink, &self.message)?;
        }
        Ok(out)
    }

    /// 从 `Failed` 载荷解码。
    pub fn decode(payload: &[u8]) -> Result<Self, ProtocolError> {
        let mut reader = Reader::new(payload);
        let raw_code = reader.u16()?;
        let code =
            ErrorCode::from_u16(raw_code).ok_or(ProtocolError::InvalidField("未知错误码"))?;
        let message = reader.utf8()?;
        if message.len() > MAX_ERROR_MESSAGE_LEN {
            return Err(ProtocolError::InvalidField("错误消息超过 256 字节"));
        }
        if !reader.is_empty() {
            return Err(ProtocolError::TrailingBytes);
        }
        Ok(Self { code, message })
    }
}

/// `Completed` 事件在 Jump 成功时的载荷。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JumpOutcome {
    /// 迁移后的目标端点。
    pub endpoint: Endpoint,
}

impl JumpOutcome {
    /// 用校验过的端点构造。
    pub fn new(endpoint: Endpoint) -> Self {
        Self { endpoint }
    }

    /// 编码为 `Completed` 载荷：`u16 host_len | host | u16 port`。
    pub fn encode(&self) -> Result<Vec<u8>, ProtocolEncodeError> {
        validate_host(&self.endpoint.host).map_err(ProtocolEncodeError::InvalidField)?;
        if self.endpoint.port == 0 {
            return Err(ProtocolEncodeError::InvalidField("端口不能为 0"));
        }
        let total = 2usize
            .checked_add(self.endpoint.host.len())
            .and_then(|value| value.checked_add(2))
            .ok_or(ProtocolEncodeError::TooLarge {
                len: usize::MAX,
                max: MAX_PAYLOAD,
            })?;
        let mut out = Vec::new();
        out.try_reserve_exact(total)
            .map_err(|_| ProtocolEncodeError::AllocationFailed)?;
        {
            let mut sink = VecSink::new(&mut out);
            write_utf8(&mut sink, &self.endpoint.host)?;
            write_u16(&mut sink, self.endpoint.port);
        }
        Ok(out)
    }

    /// 从 `Completed` 载荷解码。
    pub fn decode(payload: &[u8]) -> Result<Self, ProtocolError> {
        let mut reader = Reader::new(payload);
        let host = reader.utf8()?;
        let port = reader.u16()?;
        let endpoint = Endpoint::new(host, port)?;
        if !reader.is_empty() {
            return Err(ProtocolError::TrailingBytes);
        }
        Ok(Self { endpoint })
    }
}

/// Hello 中的一个活跃任务。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveTask {
    /// 原命令 id。
    pub cmd_id: u64,
    /// 任务名。
    pub name: String,
    /// 已运行时长（毫秒）。
    pub elapsed_ms: u64,
}

/// 能力/状态通告载荷。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hello {
    /// Worker id。
    pub worker_id: String,
    /// 固件版本字符串。
    pub firmware: String,
    /// 自启动以来的毫秒数。
    pub uptime_ms: u64,
    /// Worker Unix 毫秒；0 表示未校准。
    pub unix_ms: u64,
    /// 已注册任务名（最多 16，建议排序）。
    pub tasks: Vec<String>,
    /// 已注册工具名（最多 16，含内置工具，建议排序）。
    pub tools: Vec<String>,
    /// 当前跳转目标列表。
    pub workers: DeviceList,
    /// 正在运行的任务（最多 4）。
    pub active_tasks: Vec<ActiveTask>,
}

impl Hello {
    /// 编码为 Hello 载荷。
    pub fn encode(&self) -> Result<Vec<u8>, ProtocolEncodeError> {
        self.validate().map_err(ProtocolEncodeError::InvalidField)?;
        let mut out = Vec::new();
        {
            let mut sink = VecSink::new(&mut out);
            write_utf8(&mut sink, &self.worker_id)?;
            write_utf8(&mut sink, &self.firmware)?;
            write_u64(&mut sink, self.uptime_ms);
            write_u64(&mut sink, self.unix_ms);
            write_u16(&mut sink, self.tasks.len() as u16);
            for name in &self.tasks {
                write_utf8(&mut sink, name)?;
            }
            write_u16(&mut sink, self.tools.len() as u16);
            for name in &self.tools {
                write_utf8(&mut sink, name)?;
            }
            self.workers.encode_into(&mut sink)?;
            write_u16(&mut sink, self.active_tasks.len() as u16);
            for task in &self.active_tasks {
                write_u64(&mut sink, task.cmd_id);
                write_utf8(&mut sink, &task.name)?;
                write_u64(&mut sink, task.elapsed_ms);
            }
        }
        if out.len() > MAX_PAYLOAD {
            return Err(ProtocolEncodeError::TooLarge {
                len: out.len(),
                max: MAX_PAYLOAD,
            });
        }
        Ok(out)
    }

    /// 从 Hello 载荷解码。
    pub fn decode(payload: &[u8]) -> Result<Self, ProtocolError> {
        if payload.len() > MAX_PAYLOAD {
            return Err(ProtocolError::PayloadTooLarge {
                len: payload.len(),
                max: MAX_PAYLOAD,
            });
        }
        let mut reader = Reader::new(payload);
        let worker_id = reader.utf8()?;
        let firmware = reader.utf8()?;
        let uptime_ms = reader.u64()?;
        let unix_ms = reader.u64()?;

        let task_count = reader.u16()? as usize;
        if task_count > MAX_TASKS {
            return Err(ProtocolError::InvalidField("任务清单超过 16 项"));
        }
        let mut tasks = Vec::new();
        tasks
            .try_reserve_exact(task_count)
            .map_err(|_| ProtocolError::AllocationFailed)?;
        for _ in 0..task_count {
            tasks.push(reader.utf8()?);
        }

        let tool_count = reader.u16()? as usize;
        if tool_count > MAX_TOOLS {
            return Err(ProtocolError::InvalidField("工具清单超过 16 项"));
        }
        let mut tools = Vec::new();
        tools
            .try_reserve_exact(tool_count)
            .map_err(|_| ProtocolError::AllocationFailed)?;
        for _ in 0..tool_count {
            tools.push(reader.utf8()?);
        }

        let workers = DeviceList::decode_from(&mut reader)?;

        let active_count = reader.u16()? as usize;
        if active_count > MAX_ACTIVE_TASKS {
            return Err(ProtocolError::InvalidField("活跃任务超过 4 项"));
        }
        let mut active_tasks = Vec::new();
        active_tasks
            .try_reserve_exact(active_count)
            .map_err(|_| ProtocolError::AllocationFailed)?;
        for _ in 0..active_count {
            let cmd_id = reader.u64()?;
            let name = reader.utf8()?;
            let elapsed_ms = reader.u64()?;
            active_tasks.push(ActiveTask {
                cmd_id,
                name,
                elapsed_ms,
            });
        }

        if !reader.is_empty() {
            return Err(ProtocolError::TrailingBytes);
        }

        let hello = Self {
            worker_id,
            firmware,
            uptime_ms,
            unix_ms,
            tasks,
            tools,
            workers,
            active_tasks,
        };
        hello.validate().map_err(ProtocolError::InvalidField)?;
        Ok(hello)
    }

    fn validate(&self) -> Result<(), &'static str> {
        validate_worker_id(&self.worker_id)?;
        if self.firmware.len() > MAX_FIRMWARE_LEN {
            return Err("固件版本超过 64 字节");
        }
        if self.tasks.len() > MAX_TASKS {
            return Err("任务清单超过 16 项");
        }
        for name in &self.tasks {
            validate_name(name)?;
        }
        if self.tools.len() > MAX_TOOLS {
            return Err("工具清单超过 16 项");
        }
        for name in &self.tools {
            validate_name(name)?;
        }
        if self.active_tasks.len() > MAX_ACTIVE_TASKS {
            return Err("活跃任务超过 4 项");
        }
        for task in &self.active_tasks {
            if task.cmd_id == 0 {
                return Err("活跃任务的 cmd_id 不能为 0");
            }
            validate_name(&task.name)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{ActiveTask, ErrorPayload, Hello, JumpOutcome, Phase, STDOUT_HEADER_LEN, Stdout};
    use crate::sekai_device_list::{Device, DeviceList, Endpoint};
    use crate::sekai_wire::{
        ErrorCode, MAX_ACTIVE_TASKS, MAX_ERROR_MESSAGE_LEN, MAX_PAYLOAD, ProtocolError,
    };
    use alloc::string::String;
    use alloc::vec;
    use alloc::vec::Vec;

    fn sample_hello() -> Hello {
        let mut workers = DeviceList::new();
        workers
            .insert(Device::new("peer-1", "192.168.1.5", 1883).unwrap())
            .unwrap();
        Hello {
            worker_id: String::from("demo"),
            firmware: String::from("0.1.0"),
            uptime_ms: 1234,
            unix_ms: 1_700_000_000_000,
            tasks: vec![String::from("echo"), String::from("sleep")],
            tools: vec![String::from("sekai.info"), String::from("sekai.time")],
            workers,
            active_tasks: vec![ActiveTask {
                cmd_id: 7,
                name: String::from("echo"),
                elapsed_ms: 42,
            }],
        }
    }

    fn event() -> Stdout {
        Stdout::new(
            1,
            42,
            1,
            Phase::Completed,
            1_700_000_000_000,
            b"hello".to_vec(),
        )
        .unwrap()
    }

    #[test]
    fn phase_roundtrip_and_finality() {
        for (value, phase) in [
            (1, Phase::Accepted),
            (2, Phase::Started),
            (3, Phase::Progress),
            (4, Phase::Completed),
            (5, Phase::Failed),
            (6, Phase::Hello),
            (7, Phase::Offline),
        ] {
            assert_eq!(phase.as_u8(), value);
            assert_eq!(Phase::from_u8(value), Ok(phase));
            assert_eq!(
                phase.is_final(),
                matches!(phase, Phase::Completed | Phase::Failed)
            );
        }
        assert_eq!(Phase::from_u8(0), Err(ProtocolError::UnknownTag(0)));
        assert_eq!(Phase::from_u8(8), Err(ProtocolError::UnknownTag(8)));
    }

    #[test]
    fn stdout_roundtrip() {
        let frame = event();
        let encoded = frame.encode().unwrap();
        assert_eq!(encoded.len(), STDOUT_HEADER_LEN + 5);
        assert_eq!(Stdout::decode(&encoded).unwrap(), frame);
    }

    #[test]
    fn stdout_validation_rules() {
        // boot_id 不能为 0。
        assert!(Stdout::new(0, 1, 1, Phase::Completed, 0, vec![]).is_err());
        // Hello/Offline 的 cmd_id 必须为 0。
        assert!(Stdout::new(1, 1, 1, Phase::Hello, 0, vec![]).is_err());
        assert!(Stdout::new(1, 1, 1, Phase::Offline, 0, vec![]).is_err());
        // 命令事件的 cmd_id 不能为 0。
        assert!(Stdout::new(1, 0, 1, Phase::Completed, 0, vec![]).is_err());
        // 非 Offline 的 seq 不能为 0。
        assert!(Stdout::new(1, 1, 0, Phase::Completed, 0, vec![]).is_err());
        // LWT Offline 允许 seq=0。
        assert!(Stdout::new(1, 0, 0, Phase::Offline, 0, vec![]).is_ok());
        // Offline 载荷必须为空。
        assert!(Stdout::new(1, 0, 1, Phase::Offline, 0, vec![1]).is_err());
    }

    #[test]
    fn stdout_decode_rejects_malformed() {
        let encoded = event().encode().unwrap();

        let mut bad_version = encoded.clone();
        bad_version[0] = 9;
        assert_eq!(
            Stdout::decode(&bad_version),
            Err(ProtocolError::UnsupportedVersion(9))
        );

        let mut bad_phase = encoded.clone();
        bad_phase[1] = 9;
        assert_eq!(
            Stdout::decode(&bad_phase),
            Err(ProtocolError::UnknownTag(9))
        );

        assert_eq!(
            Stdout::decode(&encoded[..STDOUT_HEADER_LEN - 1]),
            Err(ProtocolError::Truncated)
        );

        let mut trailing = encoded.clone();
        trailing.push(0);
        assert_eq!(Stdout::decode(&trailing), Err(ProtocolError::TrailingBytes));

        let mut huge = encoded;
        let len_offset = 30;
        huge[len_offset..len_offset + 4].copy_from_slice(&(MAX_PAYLOAD as u32 + 1).to_be_bytes());
        assert_eq!(
            Stdout::decode(&huge),
            Err(ProtocolError::PayloadTooLarge {
                len: MAX_PAYLOAD + 1,
                max: MAX_PAYLOAD
            })
        );
    }

    #[test]
    fn error_payload_roundtrip_and_truncate() {
        let error = ErrorPayload::new(ErrorCode::Timeout, "超时");
        assert_eq!(
            ErrorPayload::decode(&error.encode().unwrap()).unwrap(),
            error
        );

        let long = "错".repeat(MAX_ERROR_MESSAGE_LEN);
        let truncated = ErrorPayload::new(ErrorCode::Internal, long);
        assert!(truncated.message.len() <= MAX_ERROR_MESSAGE_LEN);
        assert!(truncated.message.is_char_boundary(truncated.message.len()));
        assert!(ErrorPayload::decode(&truncated.encode().unwrap()).is_ok());
    }

    #[test]
    fn error_payload_rejects_unknown_code_and_trailing() {
        let mut payload = Vec::new();
        payload.extend_from_slice(&0xFFFFu16.to_be_bytes());
        payload.extend_from_slice(&0u16.to_be_bytes());
        assert!(ErrorPayload::decode(&payload).is_err());

        let mut payload = ErrorPayload::new(ErrorCode::Busy, "忙").encode().unwrap();
        payload.push(0);
        assert_eq!(
            ErrorPayload::decode(&payload),
            Err(ProtocolError::TrailingBytes)
        );
    }

    #[test]
    fn jump_outcome_roundtrip() {
        let outcome = JumpOutcome::new(Endpoint {
            host: String::from("peer-1"),
            port: 1883,
        });
        assert_eq!(
            JumpOutcome::decode(&outcome.encode().unwrap()).unwrap(),
            outcome
        );
        assert!(JumpOutcome::decode(&[0, 1, b'x', 0, 0]).is_err());
    }

    #[test]
    fn hello_roundtrip() {
        let hello = sample_hello();
        let payload = hello.encode().unwrap();
        assert!(payload.len() <= MAX_PAYLOAD);
        assert_eq!(Hello::decode(&payload).unwrap(), hello);
    }

    #[test]
    fn hello_rejects_invalid_fields() {
        let mut hello = sample_hello();
        hello.worker_id = String::from("bad/id");
        assert!(hello.encode().is_err());

        let mut hello = sample_hello();
        hello.tasks = vec![String::from("BadName")];
        assert!(hello.encode().is_err());

        let mut hello = sample_hello();
        hello.active_tasks = (0..=MAX_ACTIVE_TASKS)
            .map(|index| ActiveTask {
                cmd_id: index as u64 + 1,
                name: String::from("echo"),
                elapsed_ms: 0,
            })
            .collect();
        assert!(hello.encode().is_err());

        let mut hello = sample_hello();
        hello.active_tasks[0].cmd_id = 0;
        assert!(hello.encode().is_err());
    }

    #[test]
    fn hello_decode_rejects_trailing() {
        let mut payload = sample_hello().encode().unwrap();
        payload.push(0);
        assert_eq!(Hello::decode(&payload), Err(ProtocolError::TrailingBytes));
    }

    #[test]
    fn hello_stdout_integration() {
        let payload = sample_hello().encode().unwrap();
        let frame = Stdout::new(1, 0, 3, Phase::Hello, 0, payload.clone()).unwrap();
        let decoded = Stdout::decode(&frame.encode().unwrap()).unwrap();
        assert_eq!(decoded.phase, Phase::Hello);
        assert_eq!(Hello::decode(&decoded.payload).unwrap(), sample_hello());
    }
}
