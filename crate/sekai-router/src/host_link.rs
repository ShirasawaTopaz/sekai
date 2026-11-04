//! std TCP 链路、MQTT 字节流分帧与重连退避。
//!
//! Worker启动层的 link/framing 是 crate 私有模块；控制端只依赖 sekai 公共 API，
//! 因此这里实现一份等价的最小链路层（计划 §3.2 允许的少量重复）：
//!
//! - TcpLink：带读超时的 TcpStream 包装；读超时视为非致命 Timeout；
//! - StreamDecoder：有界缓冲，把 TCP 字节流切成 Packet；
//! - Backoff：500ms 起步、翻倍到 30s 封顶的指数退避。

use std::fmt;
use std::io::{ErrorKind, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

use sekai::mqtt::error::DecodeError;
use sekai::mqtt::packet::Packet;
use sekai::sekai_device_list::Endpoint;

/// 链路错误。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkError {
    /// 目标地址无法解析。
    Resolve(String),
    /// TCP 连接失败。
    Connect(String),
    /// 在已关闭的链路上读写。
    NotConnected,
    /// 读超时（非致命）。
    Timeout,
    /// 其它 I/O 错误。
    Io(String),
}

impl fmt::Display for LinkError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Resolve(message) => write!(f, "地址解析失败：{message}"),
            Self::Connect(message) => write!(f, "连接失败：{message}"),
            Self::NotConnected => write!(f, "链路未连接"),
            Self::Timeout => write!(f, "读取超时"),
            Self::Io(message) => write!(f, "I/O 错误：{message}"),
        }
    }
}

impl std::error::Error for LinkError {}

/// 传输层抽象。
pub trait Link {
    /// 建立到端点的 TCP 连接；先关闭旧连接。
    fn connect(&mut self, endpoint: &Endpoint) -> Result<(), LinkError>;
    /// 读取一段字节；读超时返回 LinkError::Timeout。
    fn read(&mut self, buffer: &mut [u8]) -> Result<usize, LinkError>;
    /// 写出全部字节。
    fn write_all(&mut self, bytes: &[u8]) -> Result<(), LinkError>;
    /// 关闭连接（幂等）。
    fn close(&mut self);
}

/// 基于 std::net::TcpStream 的链路。
pub struct TcpLink {
    stream: Option<TcpStream>,
    read_timeout: Duration,
    connect_timeout: Duration,
}

impl Default for TcpLink {
    fn default() -> Self {
        Self::new()
    }
}

impl TcpLink {
    /// 创建未连接链路；读超时 20ms，连接超时 5s。
    pub fn new() -> Self {
        Self {
            stream: None,
            read_timeout: Duration::from_millis(20),
            connect_timeout: Duration::from_secs(5),
        }
    }

    /// 是否已连接（测试用）。
    pub fn is_connected(&self) -> bool {
        self.stream.is_some()
    }
}

impl Link for TcpLink {
    fn connect(&mut self, endpoint: &Endpoint) -> Result<(), LinkError> {
        self.close();
        let address = format!("{}:{}", endpoint.host, endpoint.port);
        let mut addresses = address
            .to_socket_addrs()
            .map_err(|error| LinkError::Resolve(error.to_string()))?;
        let address = addresses
            .next()
            .ok_or_else(|| LinkError::Resolve(String::from("没有可用地址")))?;
        let stream = TcpStream::connect_timeout(&address, self.connect_timeout)
            .map_err(|error| LinkError::Connect(error.to_string()))?;
        let _ = stream.set_nodelay(true);
        let _ = stream.set_read_timeout(Some(self.read_timeout));
        let _ = stream.set_write_timeout(Some(self.connect_timeout));
        self.stream = Some(stream);
        Ok(())
    }

    fn read(&mut self, buffer: &mut [u8]) -> Result<usize, LinkError> {
        let Some(stream) = self.stream.as_mut() else {
            return Err(LinkError::NotConnected);
        };
        match stream.read(buffer) {
            Ok(read) => Ok(read),
            Err(error)
                if matches!(
                    error.kind(),
                    ErrorKind::WouldBlock | ErrorKind::TimedOut | ErrorKind::Interrupted
                ) =>
            {
                Err(LinkError::Timeout)
            }
            Err(error) => Err(LinkError::Io(error.to_string())),
        }
    }

    fn write_all(&mut self, bytes: &[u8]) -> Result<(), LinkError> {
        let Some(stream) = self.stream.as_mut() else {
            return Err(LinkError::NotConnected);
        };
        stream
            .write_all(bytes)
            .map_err(|error| LinkError::Io(error.to_string()))
    }

    fn close(&mut self) {
        self.stream = None;
    }
}

/// 有界 MQTT 字节流分帧器。
pub struct StreamDecoder {
    buffer: Vec<u8>,
    max_packet: u32,
    max_buffer: usize,
}

impl StreamDecoder {
    /// 用入站最大报文长度创建分帧器。
    pub fn new(max_packet: u32) -> Self {
        let max_packet = max_packet.max(2);
        let max_buffer = (max_packet as usize)
            .saturating_add(5)
            .saturating_add(4 * 1024);
        Self {
            buffer: Vec::new(),
            max_packet,
            max_buffer,
        }
    }

    /// 追加一批字节；缓冲超过安全上限时返回 PacketTooLarge。
    pub fn push(&mut self, bytes: &[u8]) -> Result<(), DecodeError> {
        if self.buffer.len().saturating_add(bytes.len()) > self.max_buffer {
            return Err(DecodeError::PacketTooLarge {
                size: self.buffer.len().saturating_add(bytes.len()),
                max: self.max_packet,
            });
        }
        self.buffer.extend_from_slice(bytes);
        Ok(())
    }

    /// 取出下一个完整报文；半包返回 Ok(None)。
    pub fn next_packet(&mut self) -> Result<Option<Packet>, DecodeError> {
        match Packet::decode_with_limit(&self.buffer, self.max_packet) {
            Ok((packet, used)) => {
                self.buffer.drain(..used);
                Ok(Some(packet))
            }
            Err(DecodeError::Incomplete { needed }) => {
                if needed as u64 > u64::from(self.max_packet) {
                    Err(DecodeError::PacketTooLarge {
                        size: needed,
                        max: self.max_packet,
                    })
                } else {
                    Ok(None)
                }
            }
            Err(error) => Err(error),
        }
    }

    /// 当前缓冲字节数（诊断用）。
    pub fn buffered(&self) -> usize {
        self.buffer.len()
    }
}

/// 指数退避：500ms 起步，翻倍到 30s 封顶。
#[derive(Debug, Clone, Copy)]
pub struct Backoff {
    next_ms: u64,
    max_ms: u64,
}

impl Default for Backoff {
    fn default() -> Self {
        Self::new()
    }
}

impl Backoff {
    /// 默认 500ms 到 30s。
    pub fn new() -> Self {
        Self {
            next_ms: 500,
            max_ms: 30_000,
        }
    }

    /// 取当前等待时长并推进到下一次。
    pub fn next_delay_ms(&mut self) -> u64 {
        let value = self.next_ms;
        self.next_ms = self.next_ms.saturating_mul(2).min(self.max_ms);
        value
    }

    /// 连接成功后重置。
    pub fn reset(&mut self) {
        self.next_ms = 500;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sekai::mqtt::packet::Publish;
    use sekai::mqtt::property::Properties;
    use sekai::mqtt::qos::QoS;
    use sekai::mqtt::topic::TopicName;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::thread;

    fn publish_frame(payload: &[u8]) -> Vec<u8> {
        Packet::Publish(Publish {
            dup: false,
            qos: QoS::AtMostOnce,
            retain: false,
            topic: Some(TopicName::new("a/b").unwrap()),
            packet_id: None,
            properties: Properties::new(),
            payload: payload.to_vec(),
        })
        .encode()
        .unwrap()
    }

    #[test]
    fn decoder_handles_every_split_boundary() {
        let frame = publish_frame(b"hello");
        for split in 0..=frame.len() {
            let mut decoder = StreamDecoder::new(1024);
            decoder.push(&frame[..split]).unwrap();
            let first = decoder.next_packet().unwrap();
            if split == frame.len() {
                assert!(first.is_some(), "split = {split}");
            } else {
                assert!(first.is_none(), "split = {split}");
                decoder.push(&frame[split..]).unwrap();
                assert!(decoder.next_packet().unwrap().is_some(), "split = {split}");
            }
        }
    }

    #[test]
    fn decoder_handles_multiple_packets() {
        let mut bytes = publish_frame(b"one");
        bytes.extend_from_slice(&publish_frame(b"two"));
        let mut decoder = StreamDecoder::new(1024);
        decoder.push(&bytes).unwrap();
        assert!(decoder.next_packet().unwrap().is_some());
        assert!(decoder.next_packet().unwrap().is_some());
        assert!(decoder.next_packet().unwrap().is_none());
        assert_eq!(decoder.buffered(), 0);
    }

    #[test]
    fn decoder_rejects_oversized_packet() {
        let frame = publish_frame(b"hello");
        let mut decoder = StreamDecoder::new(5);
        decoder.push(&frame).unwrap();
        assert!(decoder.next_packet().is_err());
    }

    #[test]
    fn backoff_doubles_and_caps() {
        let mut backoff = Backoff::new();
        assert_eq!(backoff.next_delay_ms(), 500);
        assert_eq!(backoff.next_delay_ms(), 1_000);
        assert_eq!(backoff.next_delay_ms(), 2_000);
        assert_eq!(backoff.next_delay_ms(), 4_000);
        assert_eq!(backoff.next_delay_ms(), 8_000);
        assert_eq!(backoff.next_delay_ms(), 16_000);
        assert_eq!(backoff.next_delay_ms(), 30_000);
        assert_eq!(backoff.next_delay_ms(), 30_000);
        backoff.reset();
        assert_eq!(backoff.next_delay_ms(), 500);
    }

    #[test]
    fn tcp_link_roundtrip() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream.write_all(b"hi").unwrap();
            let mut buffer = [0u8; 2];
            stream.read_exact(&mut buffer).unwrap();
            assert_eq!(&buffer, b"ok");
        });
        let endpoint = Endpoint {
            host: String::from("127.0.0.1"),
            port,
        };
        let mut link = TcpLink::new();
        link.connect(&endpoint).unwrap();
        let mut buffer = [0u8; 2];
        let read = loop {
            match link.read(&mut buffer) {
                Ok(read) => break read,
                Err(LinkError::Timeout) => continue,
                Err(error) => panic!("read failed: {error}"),
            }
        };
        assert_eq!(&buffer[..read], b"hi");
        link.write_all(b"ok").unwrap();
        link.close();
        assert!(!link.is_connected());
        server.join().unwrap();
    }
}
