//! ESP32 网络主循环：TCP 连接、MQTT 会话与退避重连。
//!
//! 本模块只负责字节流与链路生命周期；MQTT 状态机全部在
//! [sekai::sekai_runtime::Runtime] 里。断线（EOF/读写错误/超时）统一走
//! link_lost，正常关闭（Jump）走 link_closed，两者都可能产生一个
//! [LinkAction] 指向下一个端点。

use alloc::boxed::Box;

use embassy_net::tcp::TcpSocket;
use embassy_net::{IpAddress, IpEndpoint, Runner, Stack};
use embassy_time::{Duration, Timer, with_timeout};
use esp_println::println;
use esp_radio::wifi::{Interface, WifiController};
use sekai::sekai_clock::Clock;
use sekai::sekai_device_list::Endpoint;
use sekai::sekai_runtime::{LinkAction, Runtime};

use crate::clock::SystemClock;
use crate::framing::StreamDecoder;

/// WiFi 重连超时。
const WIFI_TIMEOUT: Duration = Duration::from_secs(20);
/// DHCP 等待超时。
const DHCP_TIMEOUT: Duration = Duration::from_secs(15);
/// TCP 连接超时。
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// 单次 socket 写入超时。
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);
/// MQTT 会话读轮询间隔（超时后先 poll 再让出）。
const READ_POLL: Duration = Duration::from_millis(20);
/// MQTT 会话最大空闲睡眠（毫秒），保证 20ms 级 poll。
const MAX_IDLE_MS: u64 = 20;
/// TCP 收发缓冲大小。
const SOCKET_BUFFER_BYTES: usize = 4096;
/// 单次读缓冲大小（解码器负责拼接半包）。
const READ_CHUNK_BYTES: usize = 512;

/// 网络协议栈 runner 任务；必须常驻执行器。
#[embassy_executor::task]
pub(super) async fn net_task(mut runner: Runner<'static, Interface>) -> ! {
    runner.run().await
}

/// MQTT 主循环：连接、会话、退避重连，永不返回。
pub(super) async fn run_mqtt(
    mut runtime: Runtime<SystemClock>,
    stack: Stack<'static>,
    mut wifi: WifiController<'static>,
    mut endpoint: Endpoint,
) -> ! {
    let mut backoff = Backoff::new();
    loop {
        if !wifi.is_connected() {
            println!("[sekai] WiFi 未连接，尝试重连");
            match with_timeout(WIFI_TIMEOUT, wifi.connect_async()).await {
                Ok(Ok(_info)) => println!("[sekai] WiFi 已连接"),
                Ok(Err(error)) => println!("[sekai] WiFi 连接失败：{error}"),
                Err(_) => println!("[sekai] WiFi 连接超时"),
            }
        }
        if with_timeout(DHCP_TIMEOUT, stack.wait_config_up())
            .await
            .is_err()
        {
            println!("[sekai] DHCP 地址仍未就绪");
        }

        let mut rx_buffer = Box::new([0u8; SOCKET_BUFFER_BYTES]);
        let mut tx_buffer = Box::new([0u8; SOCKET_BUFFER_BYTES]);
        let mut socket = TcpSocket::new(stack, &mut rx_buffer[..], &mut tx_buffer[..]);

        let outcome =
            match with_timeout(CONNECT_TIMEOUT, socket.connect(ip_endpoint(&endpoint))).await {
                Ok(Ok(())) => {
                    println!("[sekai] MQTT TCP 已连接 {endpoint}");
                    if let Err(error) = runtime.link_up(endpoint.clone()) {
                        println!("[sekai] link_up 失败：{error}");
                        SessionEnd::Lost
                    } else {
                        backoff.reset();
                        let outcome = mqtt_session(&mut runtime, &mut socket).await;
                        socket.close();
                        outcome
                    }
                }
                Ok(Err(error)) => {
                    runtime.link_failed(endpoint.clone());
                    println!("[sekai] TCP 连接 {endpoint} 失败：{error}");
                    SessionEnd::Lost
                }
                Err(_) => {
                    runtime.link_failed(endpoint.clone());
                    println!("[sekai] TCP 连接 {endpoint} 超时");
                    SessionEnd::Lost
                }
            };

        match outcome {
            SessionEnd::Lost => runtime.link_lost(),
            SessionEnd::Switch(action) => {
                runtime.link_closed();
                endpoint = switch_endpoint(action);
                backoff.reset();
                println!("[sekai] 切换链路到 {endpoint}");
            }
        }
        // link_lost / link_closed 之后可能产生回退动作。
        if let Some(action) = runtime.take_link_action() {
            endpoint = switch_endpoint(action);
            backoff.reset();
            println!("[sekai] 切换链路到 {endpoint}");
        }

        Timer::after(Duration::from_millis(backoff.next())).await;
    }
}

/// 一次 MQTT 会话的结束方式。
enum SessionEnd {
    /// 链路异常断开（EOF/读写错误/运行时错误）。
    Lost,
    /// 需要切换到新端点（正常关闭当前 socket 后调用 `link_closed`）。
    Switch(LinkAction),
}

/// 在已连接的 socket 上跑 MQTT 收发与运行时推进，直到会话结束。
async fn mqtt_session(
    runtime: &mut Runtime<SystemClock>,
    socket: &mut TcpSocket<'_>,
) -> SessionEnd {
    let mut decoder = StreamDecoder::new(runtime.max_incoming_packet_size().unwrap_or(16 * 1024));
    let mut read_buffer = [0u8; READ_CHUNK_BYTES];

    loop {
        // 1. 先把待发送报文全部写出。
        if !flush_outgoing(runtime, socket).await {
            return SessionEnd::Lost;
        }

        // 2. 非阻塞（20ms 超时）读取一段字节，拼包后喂给运行时。
        match with_timeout(READ_POLL, socket.read(&mut read_buffer)).await {
            Ok(Ok(0)) => return SessionEnd::Lost,
            Ok(Ok(read)) => {
                if decoder.push(&read_buffer[..read]).is_err() {
                    return SessionEnd::Lost;
                }
                loop {
                    match decoder.next() {
                        Ok(Some(packet)) => {
                            if runtime.feed(packet).is_err() {
                                return SessionEnd::Lost;
                            }
                        }
                        Ok(None) => break,
                        Err(_) => return SessionEnd::Lost,
                    }
                }
            }
            Ok(Err(_)) => return SessionEnd::Lost,
            // 读超时是正常情况：继续推进运行时定时器。
            Err(_) => {}
        }

        // 3. 推进 MQTT 定时器、收件箱、任务与发件箱。
        if let Err(error) = runtime.poll() {
            println!("[sekai] 运行时推进失败：{error}");
            return SessionEnd::Lost;
        }

        // 4. 出现链路切换动作（Jump/服务器迁移）：先 flush 剩余报文再关闭。
        if let Some(action) = runtime.take_link_action() {
            let _ = flush_outgoing(runtime, socket).await;
            return SessionEnd::Switch(action);
        }

        // 5. 按运行时的下一次 deadline 睡眠，最多 20ms，最少 1ms。
        let sleep_ms = runtime
            .next_deadline()
            .map(|deadline| {
                deadline
                    .saturating_sub(SystemClock.monotonic_ms())
                    .min(MAX_IDLE_MS)
            })
            .unwrap_or(MAX_IDLE_MS)
            .max(1);
        Timer::after(Duration::from_millis(sleep_ms)).await;
    }
}

/// 把运行时的待发报文全部写出；任一步失败返回 false。
async fn flush_outgoing(runtime: &mut Runtime<SystemClock>, socket: &mut TcpSocket<'_>) -> bool {
    while let Some(packet) = runtime.next_outgoing() {
        let bytes = match packet.encode() {
            Ok(bytes) => bytes,
            Err(error) => {
                println!("[sekai] MQTT 编码失败：{error}");
                return false;
            }
        };
        if !write_all(socket, &bytes).await {
            return false;
        }
    }
    true
}

/// 循环写入直到所有字节发出；超时/错误返回 false。
async fn write_all(socket: &mut TcpSocket<'_>, mut bytes: &[u8]) -> bool {
    while !bytes.is_empty() {
        match with_timeout(WRITE_TIMEOUT, socket.write(bytes)).await {
            Ok(Ok(0)) => return false,
            Ok(Ok(written)) => bytes = &bytes[written..],
            Ok(Err(_)) => return false,
            Err(_) => return false,
        }
    }
    true
}

/// 取链路动作里的目标端点。
fn switch_endpoint(action: LinkAction) -> Endpoint {
    match action {
        LinkAction::Switch { endpoint, .. } => endpoint,
    }
}

/// 把控制平面端点转换成 embassy-net 的 IPv4 端点。
///
/// SEKAI_MQTT_ADDR 与 Jump 目标在配置阶段已要求 IPv4 字面量；这里解析
/// 失败时回退到 0.0.0.0:0，连接会立刻失败并进入退避重连。
fn ip_endpoint(endpoint: &Endpoint) -> IpEndpoint {
    match parse_ipv4_octets(&endpoint.host) {
        Some([a, b, c, d]) => IpEndpoint::new(IpAddress::v4(a, b, c, d), endpoint.port),
        None => {
            println!("[sekai] 端点 {} 不是 IPv4 字面量", endpoint.host);
            IpEndpoint::new(IpAddress::v4(0, 0, 0, 0), 0)
        }
    }
}

/// 解析点分十进制 IPv4 字面量。
fn parse_ipv4_octets(host: &str) -> Option<[u8; 4]> {
    let mut octets = [0u8; 4];
    let mut parts = host.split('.');
    for octet in &mut octets {
        let part = parts.next()?;
        if part.is_empty() || part.len() > 3 {
            return None;
        }
        *octet = part.parse().ok()?;
    }
    if parts.next().is_some() {
        return None;
    }
    Some(octets)
}

/// 指数退避：500ms 起步，翻倍到 30s 封顶（与宿主驱动一致）。
#[derive(Debug, Clone, Copy)]
struct Backoff {
    next_ms: u64,
    max_ms: u64,
}

impl Backoff {
    /// 默认 500ms 到 30s。
    const fn new() -> Self {
        Self {
            next_ms: 500,
            max_ms: 30_000,
        }
    }

    /// 取当前等待时长并推进到下一次。
    fn next(&mut self) -> u64 {
        let value = self.next_ms;
        self.next_ms = self.next_ms.saturating_mul(2).min(self.max_ms);
        value
    }

    /// 连接成功后重置。
    fn reset(&mut self) {
        self.next_ms = 500;
    }
}

#[cfg(test)]
mod tests {
    use super::{Backoff, parse_ipv4_octets};

    #[test]
    fn backoff_doubles_and_caps() {
        let mut backoff = Backoff::new();
        assert_eq!(backoff.next(), 500);
        assert_eq!(backoff.next(), 1_000);
        assert_eq!(backoff.next(), 2_000);
        assert_eq!(backoff.next(), 4_000);
        assert_eq!(backoff.next(), 8_000);
        assert_eq!(backoff.next(), 16_000);
        assert_eq!(backoff.next(), 30_000);
        assert_eq!(backoff.next(), 30_000);
        backoff.reset();
        assert_eq!(backoff.next(), 500);
    }

    #[test]
    fn ipv4_literals_parse() {
        assert_eq!(parse_ipv4_octets("192.168.1.10"), Some([192, 168, 1, 10]));
        assert_eq!(parse_ipv4_octets("0.0.0.0"), Some([0, 0, 0, 0]));
        assert_eq!(parse_ipv4_octets("256.1.1.1"), None);
        assert_eq!(parse_ipv4_octets("1.2.3"), None);
        assert_eq!(parse_ipv4_octets("1.2.3.4.5"), None);
        assert_eq!(parse_ipv4_octets("broker.local"), None);
    }
}
