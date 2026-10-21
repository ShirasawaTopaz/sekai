//! MQTT 服务质量（QoS）等级。

use core::fmt;

/// 服务质量等级。
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum QoS {
    /// 至多一次（QoS 0）。
    AtMostOnce = 0,
    /// 至少一次（QoS 1）。
    AtLeastOnce = 1,
    /// 恰好一次（QoS 2）。
    ExactlyOnce = 2,
}

impl QoS {
    /// 由线上字节值构造；3..=255 返回 `None`。
    pub fn from_u8(value: u8) -> Option<Self> {
        match value {
            0 => Some(Self::AtMostOnce),
            1 => Some(Self::AtLeastOnce),
            2 => Some(Self::ExactlyOnce),
            _ => None,
        }
    }

    /// 转换为线上字节值。
    pub fn as_u8(self) -> u8 {
        self as u8
    }
}

impl fmt::Display for QoS {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "QoS {}", self.as_u8())
    }
}

#[cfg(test)]
mod tests {
    use super::QoS;
    use alloc::string::ToString;

    #[test]
    fn from_u8_roundtrip() {
        for qos in [QoS::AtMostOnce, QoS::AtLeastOnce, QoS::ExactlyOnce] {
            assert_eq!(QoS::from_u8(qos.as_u8()), Some(qos));
        }
        assert_eq!(QoS::AtMostOnce.as_u8(), 0);
        assert_eq!(QoS::AtLeastOnce.as_u8(), 1);
        assert_eq!(QoS::ExactlyOnce.as_u8(), 2);
    }

    #[test]
    fn from_u8_rejects_invalid() {
        for value in 3..=u8::MAX {
            assert_eq!(QoS::from_u8(value), None, "value = {value}");
        }
    }

    #[test]
    fn display_form() {
        assert_eq!(QoS::AtMostOnce.to_string(), "QoS 0");
        assert_eq!(QoS::AtLeastOnce.to_string(), "QoS 1");
        assert_eq!(QoS::ExactlyOnce.to_string(), "QoS 2");
    }

    #[test]
    fn ordering_and_hash() {
        assert!(QoS::AtMostOnce < QoS::AtLeastOnce);
        assert!(QoS::AtLeastOnce < QoS::ExactlyOnce);
        let set: alloc::collections::BTreeSet<QoS> =
            [QoS::ExactlyOnce, QoS::AtMostOnce].into_iter().collect();
        assert_eq!(set.len(), 2);
        assert!(set.contains(&QoS::AtMostOnce));
    }
}
