use alloc::vec::Vec;

use crate::sekai_stdin::STDIN_HEADER_LEN;
use crate::sekai_stdout::STDOUT_HEADER_LEN;
use crate::sekai_wire::MAX_PAYLOAD;

/// 长度前缀的字节数（u32 大端）。
pub const FRAME_PREFIX_LEN: usize = 4;

/// 单帧载荷（V1 帧本体）的最大长度：`max(Stdin, Stdout) 头 + 载荷上限`。
pub const MAX_FRAME_LEN: usize = STDOUT_HEADER_LEN + MAX_PAYLOAD;

/// Stdin 帧的最大长度（诊断/测试用）。
pub const MAX_STDIN_FRAME_LEN: usize = STDIN_HEADER_LEN + MAX_PAYLOAD;

/// 直连承载下两端各有一个 MQTT 客户端（Router 的 ControlPlane 与 Worker 的
/// Runtime）；保活必须真正跨链路，否则对端静默死亡无法被发现（P10）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeepAlive {
    /// 保活请求（发送方等待对端回应）。
    Pingreq,
    /// 保活应答。
    Pingresp,
}

/// Recognize the two fixed MQTT keepalive packets carried on a direct link.
pub fn decode_keepalive(frame: &[u8]) -> Option<KeepAlive> {
    match frame {
        [0xc0, 0] => Some(KeepAlive::Pingreq),
        [0xd0, 0] => Some(KeepAlive::Pingresp),
        _ => None,
    }
}

/// Encode a direct-link keepalive packet.
pub fn encode_keepalive(kind: KeepAlive) -> Vec<u8> {
    match kind {
        KeepAlive::Pingreq => alloc::vec![0xc0, 0],
        KeepAlive::Pingresp => alloc::vec![0xd0, 0],
    }
}
