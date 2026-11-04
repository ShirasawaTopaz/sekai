//! 服务端主题构造与解析。
//!
//! Worker控制平面的主题形状固定为（架构文档 §链路矩阵：MQTT 主题链路保留
//! `dev` 段，设备→Worker 改名不改变线上协议）：
//!
//! - {root}/dev/{worker_id}/in
//! - {root}/dev/{worker_id}/out
//! - {root}/dev/{worker_id}/state
//!
//! 入口（in）由控制端发布，out/state 由Worker发布。主题根允许带命名空间前缀
//! （例如 factory-a/sekai/v1），因此解析入站主题时按**末尾三段**识别
//! dev/{worker_id}/{out|state}，而不是假设根从第一个字符开始。

use sekai::sekai_wire::validate_worker_id;

/// 默认主题根。
pub const DEFAULT_TOPIC_ROOT: &str = "sekai/v1";
/// 主题根最大字节数。
pub const MAX_TOPIC_ROOT_LEN: usize = 128;

/// 主题根错误。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TopicError {
    /// 空字符串。
    Empty,
    /// 超过长度上限。
    TooLong(usize),
    /// 以 / 开头或结尾。
    SlashEdge,
    /// 含通配符 + 或 #。
    Wildcard,
    /// 含 NUL 或控制字符/空白。
    InvalidChar,
    /// 包含空段（连续 //）。
    EmptySegment,
}

impl std::fmt::Display for TopicError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => write!(f, "主题根不能为空"),
            Self::TooLong(len) => write!(f, "主题根超过 {MAX_TOPIC_ROOT_LEN} 字节（实际 {len}）"),
            Self::SlashEdge => write!(f, "主题根不能以 / 开头或结尾"),
            Self::Wildcard => write!(f, "主题根不能包含通配符 + 或 #"),
            Self::InvalidChar => write!(f, "主题根不能包含控制字符、空白或 NUL"),
            Self::EmptySegment => write!(f, "主题根不能包含空段（连续 //）"),
        }
    }
}

impl std::error::Error for TopicError {}

/// 校验过的主题根。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TopicRoot(String);

impl TopicRoot {
    /// 校验并构造主题根。
    pub fn new(value: impl Into<String>) -> Result<Self, TopicError> {
        let value = value.into();
        if value.is_empty() {
            return Err(TopicError::Empty);
        }
        if value.len() > MAX_TOPIC_ROOT_LEN {
            return Err(TopicError::TooLong(value.len()));
        }
        if value.starts_with('/') || value.ends_with('/') {
            return Err(TopicError::SlashEdge);
        }
        if value.contains('+') || value.contains('#') {
            return Err(TopicError::Wildcard);
        }
        if value
            .bytes()
            .any(|byte| byte == 0 || byte.is_ascii_control() || byte.is_ascii_whitespace())
        {
            return Err(TopicError::InvalidChar);
        }
        if value.split('/').any(str::is_empty) {
            return Err(TopicError::EmptySegment);
        }
        Ok(Self(value))
    }

    /// 主题根字符串。
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Worker in 主题。
    pub fn worker_in(&self, worker_id: &str) -> String {
        format!("{}/dev/{worker_id}/in", self.0)
    }

    /// Worker out 主题。
    pub fn worker_out(&self, worker_id: &str) -> String {
        format!("{}/dev/{worker_id}/out", self.0)
    }

    /// Worker state 主题。
    pub fn worker_state(&self, worker_id: &str) -> String {
        format!("{}/dev/{worker_id}/state", self.0)
    }

    /// 所有Worker命令事件的订阅过滤器。
    pub fn out_filter(&self) -> String {
        format!("{}/dev/+/out", self.0)
    }

    /// 所有Worker状态（retained Hello/Offline）的订阅过滤器。
    pub fn state_filter(&self) -> String {
        format!("{}/dev/+/state", self.0)
    }
}

/// 入站主题所属通道。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Channel {
    /// 命令事件（out）。
    Out,
    /// 状态通告（state，retained）。
    State,
}

impl Channel {
    /// 用于日志/显示的稳定名称。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Out => "out",
            Self::State => "state",
        }
    }
}

/// 解析后的入站主题。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IncomingTopic {
    /// 主题中的Worker id（已通过 sekai 的 validate_worker_id）。
    pub worker_id: String,
    /// 通道。
    pub channel: Channel,
}

/// 解析入站主题末尾的 dev/{worker_id}/{out|state} 三段。
///
/// 主题根带命名空间时（factory-a/sekai/v1）依然可解析；Worker id 与通道名
/// 必须合法，否则返回 None。
pub fn parse_incoming(topic: &str) -> Option<IncomingTopic> {
    let segments: Vec<&str> = topic.split('/').collect();
    // 至少要有 root/dev/{worker_id}/{out|state}，避免把无根主题当成Worker主题。
    if segments.len() < 4 {
        return None;
    }
    let channel = match segments[segments.len() - 1] {
        "out" => Channel::Out,
        "state" => Channel::State,
        _ => return None,
    };
    let worker_id = segments[segments.len() - 2];
    if segments[segments.len() - 3] != "dev" {
        return None;
    }
    if validate_worker_id(worker_id).is_err() {
        return None;
    }
    Some(IncomingTopic {
        worker_id: String::from(worker_id),
        channel,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_default_topics() {
        let root = TopicRoot::new(DEFAULT_TOPIC_ROOT).unwrap();
        assert_eq!(root.worker_in("dev-1"), "sekai/v1/dev/dev-1/in");
        assert_eq!(root.worker_out("dev-1"), "sekai/v1/dev/dev-1/out");
        assert_eq!(root.worker_state("dev-1"), "sekai/v1/dev/dev-1/state");
        assert_eq!(root.out_filter(), "sekai/v1/dev/+/out");
        assert_eq!(root.state_filter(), "sekai/v1/dev/+/state");
    }

    #[test]
    fn supports_namespaced_root() {
        let root = TopicRoot::new("factory-a/sekai/v1").unwrap();
        assert_eq!(root.out_filter(), "factory-a/sekai/v1/dev/+/out");
        let incoming = parse_incoming("factory-a/sekai/v1/dev/host-gpio/state").unwrap();
        assert_eq!(incoming.worker_id, "host-gpio");
        assert_eq!(incoming.channel, Channel::State);
        let incoming = parse_incoming("factory-a/sekai/v1/dev/host-gpio/out").unwrap();
        assert_eq!(incoming.channel, Channel::Out);
    }

    #[test]
    fn rejects_invalid_roots() {
        for value in [
            "",
            "/sekai/v1",
            "sekai/v1/",
            "sekai/+/v1",
            "sekai/#/v1",
            "sekai//v1",
            "sekai v1",
            "sekai/v1\u{0}",
        ] {
            assert!(TopicRoot::new(value).is_err(), "should reject {value:?}");
        }
        let long = "a".repeat(MAX_TOPIC_ROOT_LEN + 1);
        assert!(matches!(TopicRoot::new(long), Err(TopicError::TooLong(_))));
    }

    #[test]
    fn parses_only_worker_topics() {
        assert!(parse_incoming("sekai/v1/dev/a/out").is_some());
        assert!(parse_incoming("sekai/v1/dev/a/state").is_some());
        for topic in [
            "sekai/v1/dev/a/in",
            "sekai/v1/dev/a/",
            "sekai/v1/dev//out",
            "sekai/v1/dev/a b/out",
            "sekai/v1/a/out",
            "sekai/v1/dev/a/Out",
            "sekai/v1/worker/a/out",
            "dev/a/out",
            "",
        ] {
            assert!(parse_incoming(topic).is_none(), "should reject {topic:?}");
        }
    }
}
