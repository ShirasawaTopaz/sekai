//! 共享的Worker/端点配置解析。
//!
//! 宿主（x86）与固件（esp32）共用这些函数：它们只依赖 `sekai` 的类型与
//! `alloc`，不依赖 `std`，因此同一个实现可以在两边编译。

use alloc::format;
use alloc::string::String;

use sekai::sekai_device_list::{Device, DeviceList, Endpoint};

/// 解析Worker列表字符串：`name=host:port,...`。
///
/// 空字符串/纯空白返回空列表；任何一项非法都返回带位置的可读错误。
/// x86 驱动用于 `SEKAI_DEVICES`，ESP32 固件用于编译期 `SEKAI_DEVICES`。
pub fn parse_workers_str(value: &str) -> Result<DeviceList, String> {
    let mut workers = DeviceList::new();
    let value = value.trim();
    if value.is_empty() {
        return Ok(workers);
    }
    for entry in value.split(',') {
        let entry = entry.trim();
        if entry.is_empty() {
            continue;
        }
        let (name, address) = entry
            .split_once('=')
            .ok_or_else(|| format!("Worker项 `{entry}` 应为 name=host:port"))?;
        let endpoint = parse_endpoint(address.trim())
            .map_err(|error| format!("Worker项 `{entry}` 端点非法：{error}"))?;
        let worker = Device::new(name.trim(), endpoint.host, endpoint.port)
            .map_err(|error| format!("Worker项 `{entry}` 非法：{error}"))?;
        workers
            .insert(worker)
            .map_err(|error| format!("Worker项 `{entry}` 非法：{error}"))?;
    }
    Ok(workers)
}

/// 解析 `host:port` 端点；host 可以是主机名或 IPv4 字面量。
pub fn parse_endpoint(value: &str) -> Result<Endpoint, String> {
    Endpoint::parse(value).map_err(|error| alloc::string::ToString::to_string(&error))
}

/// 解析 **IPv4 字面量**:port 端点。
///
/// ESP32 本轮不做 DNS，因此 broker 地址必须是 IPv4 字面量；host 不是 IPv4
/// 字面量时返回错误。x86 驱动仍使用允许主机名的 [`parse_endpoint`]。
pub fn parse_ipv4_endpoint(value: &str) -> Result<Endpoint, String> {
    let endpoint = parse_endpoint(value)?;
    if !is_ipv4_literal(&endpoint.host) {
        return Err(format!(
            "端点 `{value}` 的主机必须是 IPv4 字面量（本轮不做 DNS）"
        ));
    }
    Ok(endpoint)
}

/// 判断字符串是否为点分十进制 IPv4 字面量。
pub fn is_ipv4_literal(host: &str) -> bool {
    let parts: alloc::vec::Vec<&str> = host.split('.').collect();
    if parts.len() != 4 {
        return false;
    }
    parts.iter().all(|part| {
        !part.is_empty()
            && part.len() <= 3
            && part.bytes().all(|byte| byte.is_ascii_digit())
            && part
                .parse::<u16>()
                .map(|value| value <= 255)
                .unwrap_or(false)
    })
}

#[cfg(test)]
mod tests {
    use super::{is_ipv4_literal, parse_endpoint, parse_ipv4_endpoint, parse_workers_str};

    #[test]
    fn workers_parse() {
        let workers = parse_workers_str("peer=127.0.0.1:1884, backup=10.0.0.2:1883").unwrap();
        assert_eq!(workers.len(), 2);
        assert_eq!(workers.get("peer").unwrap().port, 1884);
        assert_eq!(workers.get("backup").unwrap().ip, "10.0.0.2");
        assert!(parse_workers_str("").unwrap().is_empty());
        assert!(parse_workers_str("   ").unwrap().is_empty());
    }

    #[test]
    fn workers_parse_rejects_bad_entry() {
        assert!(parse_workers_str("peer").is_err());
        assert!(parse_workers_str("peer=not-an-endpoint").is_err());
        assert!(parse_workers_str("peer=127.0.0.1:0").is_err());
        assert!(parse_workers_str("=127.0.0.1:1883").is_err());
    }

    #[test]
    fn endpoints_parse() {
        let endpoint = parse_endpoint("192.168.1.10:1883").unwrap();
        assert_eq!(endpoint.host, "192.168.1.10");
        assert_eq!(endpoint.port, 1883);
        assert!(parse_endpoint("broker.local:1883").is_ok());
        assert!(parse_endpoint("192.168.1.10").is_err());
        assert!(parse_endpoint("192.168.1.10:0").is_err());
        assert!(parse_endpoint("").is_err());
    }

    #[test]
    fn ipv4_endpoints_are_strict() {
        assert!(parse_ipv4_endpoint("192.168.1.10:1883").is_ok());
        assert!(parse_ipv4_endpoint("127.0.0.1:1883").is_ok());
        assert!(parse_ipv4_endpoint("broker.local:1883").is_err());
        assert!(parse_ipv4_endpoint("256.1.1.1:1883").is_err());
        assert!(parse_ipv4_endpoint("1.2.3:1883").is_err());
        assert!(parse_ipv4_endpoint("::1:1883").is_err());
    }

    #[test]
    fn ipv4_literal_detection() {
        assert!(is_ipv4_literal("0.0.0.0"));
        assert!(is_ipv4_literal("255.255.255.255"));
        assert!(!is_ipv4_literal(""));
        assert!(!is_ipv4_literal("1.2.3"));
        assert!(!is_ipv4_literal("1.2.3.4.5"));
        assert!(!is_ipv4_literal("1.2.3.a"));
        assert!(!is_ipv4_literal("1.2.3.256"));
        assert!(!is_ipv4_literal("1.2.3.-1"));
    }
}
