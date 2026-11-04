//! Jump 目标Worker列表。
//!
//! 每个 [`Device`] 描述一个可连接的 MQTT 端点（通常是另一台Worker/网关上
//! 的 broker），供 [`crate::sekai_runtime::Runtime`] 执行 Jump 时解析。
//! 列表是有界的：最多 [`MAX_DEVICES`] 项，名称唯一。

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;

use crate::mqtt::codec::{Reader, Sink, VecSink, write_u16, write_utf8};
use crate::mqtt::error::{DecodeError, EncodeError};
use crate::sekai_wire::{
    MAX_DEVICE_ID_LEN, MAX_DEVICES, MAX_HOST_LEN, ProtocolEncodeError, ProtocolError,
    validate_host, validate_worker_id,
};

/// 一个可连接的 MQTT 端点。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    /// 主机名或 IPv4 地址（v1 不支持 IPv6 字面量）。
    pub host: String,
    /// TCP 端口，1..=65535。
    pub port: u16,
}

impl Endpoint {
    /// 构造端点并校验主机名/端口。
    pub fn new(host: impl Into<String>, port: u16) -> Result<Self, ProtocolError> {
        let host = host.into();
        validate_host(&host).map_err(ProtocolError::InvalidField)?;
        if port == 0 {
            return Err(ProtocolError::InvalidField("端口不能为 0"));
        }
        Ok(Self { host, port })
    }

    /// 解析 `host:port`，可接受 `mqtt://` / `tcp://` 前缀。
    ///
    /// v1 不支持 IPv6 字面量，也不接受缺少端口的写法。
    pub fn parse(text: &str) -> Result<Self, ProtocolError> {
        let text = text
            .strip_prefix("mqtt://")
            .or_else(|| text.strip_prefix("tcp://"))
            .unwrap_or(text);
        if text.is_empty() {
            return Err(ProtocolError::InvalidField("端点不能为空"));
        }
        if text.contains('[') || text.contains(']') {
            return Err(ProtocolError::InvalidField("v1 不支持 IPv6 字面量"));
        }
        let (host, port) = text
            .rsplit_once(':')
            .ok_or(ProtocolError::InvalidField("端点应为主机:端口"))?;
        let port = port
            .parse::<u16>()
            .map_err(|_| ProtocolError::InvalidField("端口非法"))?;
        Self::new(host, port)
    }
}

impl fmt::Display for Endpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.host, self.port)
    }
}

/// Worker列表中的一项。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Device {
    /// 逻辑名称，在列表内唯一。
    pub name: String,
    /// IPv4 地址或主机名。
    pub ip: String,
    /// TCP 端口。
    pub port: u16,
}

impl Device {
    /// 构造Worker条目并校验字段。
    pub fn new(
        name: impl Into<String>,
        ip: impl Into<String>,
        port: u16,
    ) -> Result<Self, ProtocolError> {
        let name = name.into();
        let ip = ip.into();
        validate_worker_id(&name).map_err(ProtocolError::InvalidField)?;
        validate_host(&ip).map_err(ProtocolError::InvalidField)?;
        if port == 0 {
            return Err(ProtocolError::InvalidField("端口不能为 0"));
        }
        Ok(Self { name, ip, port })
    }

    /// 该Worker对应的连接端点。
    pub fn endpoint(&self) -> Endpoint {
        Endpoint {
            host: self.ip.clone(),
            port: self.port,
        }
    }
}

/// 有界跳转目标列表。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DeviceList {
    workers: Vec<Device>,
}

impl DeviceList {
    /// 创建空列表。
    pub fn new() -> Self {
        Self {
            workers: Vec::new(),
        }
    }

    /// 当前条目数。
    pub fn len(&self) -> usize {
        self.workers.len()
    }

    /// 是否为空。
    pub fn is_empty(&self) -> bool {
        self.workers.is_empty()
    }

    /// 按名称查找。
    pub fn get(&self, name: &str) -> Option<&Device> {
        self.workers.iter().find(|worker| worker.name == name)
    }

    /// 遍历所有Worker。
    pub fn iter(&self) -> core::slice::Iter<'_, Device> {
        self.workers.iter()
    }

    /// 插入Worker；同名Worker会被替换，数量超限时返回错误。
    pub fn insert(&mut self, worker: Device) -> Result<(), ProtocolError> {
        if let Some(existing) = self
            .workers
            .iter_mut()
            .find(|existing| existing.name == worker.name)
        {
            *existing = worker;
            return Ok(());
        }
        if self.workers.len() >= MAX_DEVICES {
            return Err(ProtocolError::InvalidField("Worker列表已满"));
        }
        self.workers.push(worker);
        Ok(())
    }

    /// 删除指定名称，返回是否删除成功。
    pub fn remove(&mut self, name: &str) -> bool {
        let Some(index) = self.workers.iter().position(|worker| worker.name == name) else {
            return false;
        };
        self.workers.remove(index);
        true
    }

    /// 原子替换整个列表；名称重复或数量超限时保持原列表不变。
    pub fn replace(&mut self, workers: Vec<Device>) -> Result<(), ProtocolError> {
        if workers.len() > MAX_DEVICES {
            return Err(ProtocolError::InvalidField("Worker列表已满"));
        }
        let mut seen: Vec<&str> = Vec::with_capacity(workers.len());
        for worker in &workers {
            if seen.contains(&worker.name.as_str()) {
                return Err(ProtocolError::InvalidField("Worker名称重复"));
            }
            seen.push(worker.name.as_str());
        }
        self.workers = workers;
        Ok(())
    }

    /// 解析 Jump 目标。
    ///
    /// 优先匹配Worker名称，其次匹配 IP/主机名；只有在
    /// `allow_arbitrary` 为真时才接受显式的 `host:port`。
    pub fn resolve(&self, target: &str, allow_arbitrary: bool) -> Result<Endpoint, ProtocolError> {
        if target.is_empty() {
            return Err(ProtocolError::InvalidField("跳转目标为空"));
        }
        if let Some(worker) = self.workers.iter().find(|worker| worker.name == target) {
            return Ok(worker.endpoint());
        }
        if let Some(worker) = self.workers.iter().find(|worker| worker.ip == target) {
            return Ok(worker.endpoint());
        }
        if allow_arbitrary {
            return Endpoint::parse(target);
        }
        Err(ProtocolError::InvalidField("跳转目标不在Worker列表中"))
    }

    /// 编码为独立载荷：`u16 count | (name, ip, port)*`。
    pub fn encode(&self) -> Result<Vec<u8>, ProtocolEncodeError> {
        let mut out = Vec::new();
        {
            let mut sink = VecSink::new(&mut out);
            self.encode_into(&mut sink)?;
        }
        Ok(out)
    }

    /// 从独立载荷解码（最多 [`MAX_DEVICES`] 项，名称必须唯一）。
    pub fn decode(payload: &[u8]) -> Result<Self, ProtocolError> {
        let mut reader = Reader::new(payload);
        let list = Self::decode_from(&mut reader)?;
        if !reader.is_empty() {
            return Err(ProtocolError::TrailingBytes);
        }
        Ok(list)
    }

    pub(crate) fn encode_into<S: Sink + ?Sized>(&self, sink: &mut S) -> Result<(), EncodeError> {
        let count =
            u16::try_from(self.workers.len()).map_err(|_| EncodeError::ValueOutOfRange {
                field: "Worker数量",
                value: self.workers.len() as u32,
                max: u16::MAX as u32,
            })?;
        write_u16(sink, count);
        for worker in &self.workers {
            write_utf8(sink, &worker.name)?;
            write_utf8(sink, &worker.ip)?;
            write_u16(sink, worker.port);
        }
        Ok(())
    }

    pub(crate) fn decode_from(reader: &mut Reader<'_>) -> Result<Self, DecodeError> {
        let count = reader.u16()? as usize;
        if count > MAX_DEVICES {
            return Err(DecodeError::ProtocolError("Worker列表超过上限"));
        }
        let mut list = Self::new();
        for _ in 0..count {
            let name = reader.utf8()?;
            let ip = reader.utf8()?;
            let port = reader.u16()?;
            let worker = Device::new(name, ip, port).map_err(|error| match error {
                ProtocolError::InvalidField(message) => DecodeError::ProtocolError(message),
                _ => DecodeError::ProtocolError("Worker条目非法"),
            })?;
            if list.get(&worker.name).is_some() {
                return Err(DecodeError::ProtocolError("Worker名称重复"));
            }
            list.insert(worker)
                .map_err(|_| DecodeError::ProtocolError("Worker列表已满"))?;
        }
        Ok(list)
    }
}

/// 让常量上限在编译期保持可见，避免文档与实现分叉。
const _: () = {
    assert!(MAX_DEVICES > 0);
    assert!(MAX_DEVICE_ID_LEN >= 1);
    assert!(MAX_HOST_LEN >= 1);
};

#[cfg(test)]
mod tests {
    use super::{Device, DeviceList, Endpoint};
    use crate::sekai_wire::{MAX_DEVICES, ProtocolError};
    use alloc::string::String;
    use alloc::vec;
    use alloc::vec::Vec;

    fn worker(name: &str, ip: &str, port: u16) -> Device {
        Device::new(name, ip, port).unwrap()
    }

    #[test]
    fn endpoint_parse_forms() {
        assert_eq!(
            Endpoint::parse("127.0.0.1:1883").unwrap(),
            Endpoint {
                host: String::from("127.0.0.1"),
                port: 1883
            }
        );
        assert_eq!(
            Endpoint::parse("mqtt://broker.local:1884").unwrap(),
            Endpoint {
                host: String::from("broker.local"),
                port: 1884
            }
        );
        assert!(Endpoint::parse("host").is_err());
        assert!(Endpoint::parse("host:0").is_err());
        assert!(Endpoint::parse("host:70000").is_err());
        assert!(Endpoint::parse("[::1]:1883").is_err());
    }

    #[test]
    fn worker_validation() {
        assert!(Device::new("peer-1", "127.0.0.1", 1883).is_ok());
        assert!(Device::new("", "127.0.0.1", 1883).is_err());
        assert!(Device::new("peer/1", "127.0.0.1", 1883).is_err());
        assert!(Device::new("peer-1", "", 1883).is_err());
        assert!(Device::new("peer-1", "127.0.0.1", 0).is_err());
    }

    #[test]
    fn insert_replaces_and_enforces_limit() {
        let mut list = DeviceList::new();
        list.insert(worker("a", "127.0.0.1", 1883)).unwrap();
        list.insert(worker("a", "127.0.0.2", 1884)).unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list.get("a").unwrap().ip, "127.0.0.2");
        for index in 1..MAX_DEVICES {
            list.insert(worker(
                &alloc::format!("d{index}"),
                "127.0.0.1",
                1883 + index as u16,
            ))
            .unwrap();
        }
        assert_eq!(list.len(), MAX_DEVICES);
        assert!(matches!(
            list.insert(worker("overflow", "127.0.0.1", 1883)),
            Err(ProtocolError::InvalidField(_))
        ));
    }

    #[test]
    fn resolve_by_name_ip_and_explicit() {
        let mut list = DeviceList::new();
        list.insert(worker("peer-1", "192.168.1.5", 1883)).unwrap();

        assert_eq!(list.resolve("peer-1", false).unwrap().host, "192.168.1.5");
        assert_eq!(list.resolve("192.168.1.5", false).unwrap().port, 1883);
        assert!(list.resolve("10.0.0.1:1884", false).is_err());
        assert_eq!(
            list.resolve("10.0.0.1:1884", true).unwrap(),
            Endpoint {
                host: String::from("10.0.0.1"),
                port: 1884
            }
        );
        assert!(list.resolve("", false).is_err());
    }

    #[test]
    fn replace_is_atomic_on_duplicate() {
        let mut list = DeviceList::new();
        list.insert(worker("a", "127.0.0.1", 1883)).unwrap();
        let replacement = vec![
            worker("x", "127.0.0.1", 1883),
            worker("x", "127.0.0.2", 1883),
        ];
        assert!(list.replace(replacement).is_err());
        assert_eq!(list.len(), 1);
        assert_eq!(list.get("a").unwrap().ip, "127.0.0.1");

        list.replace(vec![
            worker("x", "127.0.0.1", 1883),
            worker("y", "127.0.0.2", 1884),
        ])
        .unwrap();
        assert_eq!(list.len(), 2);
        assert!(list.get("a").is_none());
    }

    #[test]
    fn encode_decode_roundtrip() {
        let mut list = DeviceList::new();
        list.insert(worker("peer-1", "192.168.1.5", 1883)).unwrap();
        list.insert(worker("peer-2", "broker.local", 1884)).unwrap();
        let payload = list.encode().unwrap();
        assert_eq!(DeviceList::decode(&payload).unwrap(), list);
    }

    #[test]
    fn decode_rejects_duplicates_and_trailing() {
        let mut list = DeviceList::new();
        list.insert(worker("peer-1", "192.168.1.5", 1883)).unwrap();
        let payload = list.encode().unwrap();

        let mut trailing = payload.clone();
        trailing.push(0);
        assert_eq!(
            DeviceList::decode(&trailing),
            Err(ProtocolError::TrailingBytes)
        );

        // 手工构造两项同名列表。
        let mut duplicate = Vec::new();
        duplicate.extend_from_slice(&2u16.to_be_bytes());
        for _ in 0..2 {
            duplicate.extend_from_slice(&6u16.to_be_bytes());
            duplicate.extend_from_slice(b"peer-1");
            duplicate.extend_from_slice(&9u16.to_be_bytes());
            duplicate.extend_from_slice(b"127.0.0.1");
            duplicate.extend_from_slice(&1883u16.to_be_bytes());
        }
        assert!(DeviceList::decode(&duplicate).is_err());
    }

    #[test]
    fn remove_works() {
        let mut list = DeviceList::new();
        list.insert(worker("a", "127.0.0.1", 1883)).unwrap();
        assert!(list.remove("a"));
        assert!(!list.remove("a"));
        assert!(list.is_empty());
    }
}
