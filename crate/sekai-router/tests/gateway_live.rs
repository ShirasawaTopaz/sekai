//! MqttGateway 真实 TCP 端到端测试（task-5）。
//!
//! 进程内最小 MQTT 5 broker（监听 127.0.0.1 随机端口）+ 假Worker线程，验证
//! connect / wait_ready / list_workers / call / 稳定错误码 / 断链退避重连后
//! 通过 retained Hello 重新发现 / 命令超时事件 / Drop 不挂起。
//!
//! 沙箱注意事项：本机 DSH workspace-write 沙箱拒绝 TcpStream::try_clone（os
//! error 10022）。因此 broker 与假Worker都使用“单 TcpStream 顺序写+读”模式：
//! 连接线程先 flush mpsc 发送队列，再 read；断开由每连接的 AtomicBool 标志 +
//! TcpStream::shutdown 完成（shutdown 只需要 &self，不需要句柄复制）。

use std::collections::HashMap;
use std::io::{ErrorKind, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use sekai::mqtt::error::DecodeError;
use sekai::mqtt::packet::{Ack, Connack, Connect, Packet, Publish, Suback, Subscribe};
use sekai::mqtt::property::Properties;
use sekai::mqtt::qos::QoS;
use sekai::mqtt::reason::ReasonCode;
use sekai::mqtt::topic::{Subscription, SubscriptionOptions, TopicFilter, TopicName};
use sekai::sekai_device_list::{DeviceList, Endpoint};
use sekai::sekai_runtime::CONTENT_TYPE_STDOUT;
use sekai::sekai_stdin::Stdin;
use sekai::sekai_stdout::{ErrorPayload, Hello, Phase, Stdout};
use sekai::sekai_wire::ErrorCode;
use sekai_router::control::CommandRequest;
use sekai_router::gateway::{MqttGateway, MqttGatewayConfig, WorkerGateway};
use sekai_router::topic::TopicRoot;

const TOPIC_ROOT: &str = "sekai/v1";
const WORKER_ID: &str = "dev-1";
const SHORT_WAIT: Duration = Duration::from_millis(5);

// ============================ 最小 MQTT broker ============================

struct BrokerSub {
    filter: String,
    qos: QoS,
    no_local: bool,
}

struct BrokerClient {
    sender: Sender<Packet>,
    subs: Vec<BrokerSub>,
}

#[derive(Default)]
struct BrokerState {
    clients: HashMap<u64, BrokerClient>,
    retained: HashMap<String, Publish>,
    next_client: u64,
}

struct MiniBroker {
    port: u16,
    shutdown: Arc<AtomicBool>,
    state: Arc<Mutex<BrokerState>>,
    client_flags: Arc<Mutex<Vec<Arc<AtomicBool>>>>,
    accept_thread: Option<JoinHandle<()>>,
}

impl MiniBroker {
    fn start() -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("绑定随机端口");
        let port = listener.local_addr().expect("local addr").port();
        listener.set_nonblocking(true).expect("非阻塞监听");
        let shutdown = Arc::new(AtomicBool::new(false));
        let state = Arc::new(Mutex::new(BrokerState::default()));
        let client_flags: Arc<Mutex<Vec<Arc<AtomicBool>>>> = Arc::new(Mutex::new(Vec::new()));

        let accept_shutdown = Arc::clone(&shutdown);
        let accept_state = Arc::clone(&state);
        let accept_flags = Arc::clone(&client_flags);
        let accept_thread = thread::spawn(move || {
            while !accept_shutdown.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, _addr)) => {
                        let closed = Arc::new(AtomicBool::new(false));
                        accept_flags
                            .lock()
                            .expect("flags")
                            .push(Arc::clone(&closed));
                        let state = Arc::clone(&accept_state);
                        thread::spawn(move || serve_client(stream, state, closed));
                    }
                    Err(error) if error.kind() == ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2));
                    }
                    Err(_) => thread::sleep(Duration::from_millis(2)),
                }
            }
        });

        Self {
            port,
            shutdown,
            state,
            client_flags,
            accept_thread: Some(accept_thread),
        }
    }

    /// 关闭所有当前连接但继续监听（模拟一次网络抖动），retained 消息保留。
    fn drop_connections(&mut self) {
        let flags = {
            let flags = self.client_flags.lock().expect("flags");
            flags.clone()
        };
        for flag in flags {
            flag.store(true, Ordering::SeqCst);
        }
        self.client_flags.lock().expect("flags").clear();
        self.state.lock().expect("state").clients.clear();
    }

    fn shutdown(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        self.drop_connections();
        if let Some(handle) = self.accept_thread.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for MiniBroker {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn serve_client(mut stream: TcpStream, state: Arc<Mutex<BrokerState>>, closed: Arc<AtomicBool>) {
    if stream
        .set_read_timeout(Some(Duration::from_millis(10)))
        .is_err()
    {
        return;
    }
    stream.set_nodelay(true).ok();
    let (sender, receiver) = mpsc::channel::<Packet>();
    let client_id = {
        let mut state = state.lock().expect("state");
        let id = state.next_client;
        state.next_client += 1;
        state.clients.insert(
            id,
            BrokerClient {
                sender,
                subs: Vec::new(),
            },
        );
        id
    };

    let mut buffer: Vec<u8> = Vec::new();
    let mut read_buf = [0u8; 4096];
    loop {
        if closed.load(Ordering::SeqCst) {
            let _ = stream.shutdown(Shutdown::Both);
            state.lock().expect("state").clients.remove(&client_id);
            return;
        }
        // 先写出路由队列（同一连接只有一个 TcpStream，顺序写+读）。
        while let Ok(packet) = receiver.try_recv() {
            if write_packet(&mut stream, &packet).is_err() {
                state.lock().expect("state").clients.remove(&client_id);
                return;
            }
        }
        match stream.read(&mut read_buf) {
            Ok(0) => {
                state.lock().expect("state").clients.remove(&client_id);
                return;
            }
            Ok(read) => {
                buffer.extend_from_slice(&read_buf[..read]);
                loop {
                    match Packet::decode(&buffer) {
                        Ok((packet, used)) => {
                            buffer.drain(..used);
                            if !handle_packet(client_id, packet, &state) {
                                state.lock().expect("state").clients.remove(&client_id);
                                return;
                            }
                        }
                        Err(DecodeError::Incomplete { .. }) => break,
                        Err(_) => {
                            state.lock().expect("state").clients.remove(&client_id);
                            return;
                        }
                    }
                }
            }
            Err(error) if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                thread::sleep(Duration::from_millis(1));
            }
            Err(_) => {
                state.lock().expect("state").clients.remove(&client_id);
                return;
            }
        }
    }
}

fn write_packet(stream: &mut TcpStream, packet: &Packet) -> std::io::Result<()> {
    let bytes = packet
        .encode()
        .map_err(|error| std::io::Error::other(error.to_string()))?;
    stream.write_all(&bytes)?;
    stream.flush()
}

fn send_to(state: &Arc<Mutex<BrokerState>>, client_id: u64, packet: Packet) {
    let state = state.lock().expect("state");
    if let Some(client) = state.clients.get(&client_id) {
        let _ = client.sender.send(packet);
    }
}

fn handle_packet(client_id: u64, packet: Packet, state: &Arc<Mutex<BrokerState>>) -> bool {
    match packet {
        Packet::Connect(_connect) => {
            send_to(
                state,
                client_id,
                Packet::Connack(Connack {
                    session_present: false,
                    reason_code: ReasonCode::Success,
                    properties: Properties::new(),
                }),
            );
            true
        }
        Packet::Subscribe(subscribe) => {
            let mut reason_codes = Vec::new();
            let mut retained_to_send: Vec<(Publish, QoS)> = Vec::new();
            {
                let mut state = state.lock().expect("state");
                for subscription in &subscribe.subscriptions {
                    let filter = String::from(subscription.filter.as_str());
                    reason_codes.push(ReasonCode::GrantedQos1);
                    for (topic, publish) in &state.retained {
                        if topic_matches(&filter, topic) {
                            let qos = std::cmp::min(publish.qos, subscription.options.qos);
                            retained_to_send.push((publish.clone(), qos));
                        }
                    }
                    if let Some(client) = state.clients.get_mut(&client_id) {
                        client.subs.push(BrokerSub {
                            filter,
                            qos: subscription.options.qos,
                            no_local: subscription.options.no_local,
                        });
                    }
                }
            }
            send_to(
                state,
                client_id,
                Packet::Suback(Suback {
                    packet_id: subscribe.packet_id,
                    properties: Properties::new(),
                    reason_codes,
                }),
            );
            for (publish, qos) in retained_to_send {
                deliver(client_id, &publish, qos, state, true);
            }
            true
        }
        Packet::Publish(publish) => {
            if publish.qos == QoS::AtLeastOnce
                && let Some(packet_id) = publish.packet_id
            {
                send_to(
                    state,
                    client_id,
                    Packet::Puback(Ack {
                        packet_id,
                        reason_code: ReasonCode::Success,
                        properties: Properties::new(),
                    }),
                );
            }
            let Some(topic) = publish
                .topic
                .as_ref()
                .map(|topic| String::from(topic.as_str()))
            else {
                return true;
            };
            let retain = publish.retain;
            if retain {
                let mut state = state.lock().expect("state");
                if publish.payload.is_empty() {
                    state.retained.remove(&topic);
                } else {
                    let stored = Publish {
                        dup: false,
                        qos: publish.qos,
                        retain: true,
                        topic: Some(TopicName::new(topic.clone()).expect("主题名")),
                        packet_id: None,
                        properties: publish.properties.clone(),
                        payload: publish.payload.clone(),
                    };
                    state.retained.insert(topic.clone(), stored);
                }
            }
            route(client_id, &topic, &publish, state);
            true
        }
        Packet::Pingreq => {
            send_to(state, client_id, Packet::Pingresp);
            true
        }
        Packet::Disconnect(_) => false,
        _ => true,
    }
}

fn route(from_client: u64, topic: &str, publish: &Publish, state: &Arc<Mutex<BrokerState>>) {
    let targets: Vec<(u64, QoS)> = {
        let state = state.lock().expect("state");
        let mut targets = Vec::new();
        for (id, client) in &state.clients {
            for sub in &client.subs {
                if sub.no_local && *id == from_client {
                    continue;
                }
                if topic_matches(&sub.filter, topic) {
                    let qos = std::cmp::min(publish.qos, sub.qos);
                    targets.push((*id, qos));
                    break;
                }
            }
        }
        targets
    };
    for (target, qos) in targets {
        deliver(target, publish, qos, state, false);
    }
}

fn deliver(
    target: u64,
    publish: &Publish,
    qos: QoS,
    state: &Arc<Mutex<BrokerState>>,
    retained_delivery: bool,
) {
    let mut state = state.lock().expect("state");
    let Some(client) = state.clients.get_mut(&target) else {
        return;
    };
    let packet = Packet::Publish(Publish {
        dup: false,
        qos,
        retain: retained_delivery,
        topic: publish.topic.clone(),
        packet_id: if qos == QoS::AtMostOnce {
            None
        } else {
            Some(1)
        },
        properties: publish.properties.clone(),
        payload: publish.payload.clone(),
    });
    let _ = client.sender.send(packet);
}

fn topic_matches(filter: &str, topic: &str) -> bool {
    let mut filter_parts = filter.split('/');
    let mut topic_parts = topic.split('/');
    loop {
        match (filter_parts.next(), topic_parts.next()) {
            (Some("#"), _) => return true,
            (Some("+"), Some(_)) => {}
            (Some(expected), Some(actual)) => {
                if expected != actual {
                    return false;
                }
            }
            (Some(_), None) => return false,
            (None, Some(_)) => return false,
            (None, None) => return true,
        }
    }
}

// ============================ 假Worker ============================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WorkerMode {
    /// 正常回 Accepted + Completed。
    Respond,
    /// 只发 Hello，不回应 Stdin（用于 gateway 超时）。
    Silent,
    /// 回 Accepted + Failed(InvalidArgument)。
    Reject,
}

fn decode_available(buffer: &mut Vec<u8>, out: &mut Vec<Packet>) -> Result<(), ()> {
    loop {
        match Packet::decode(buffer) {
            Ok((packet, used)) => {
                buffer.drain(..used);
                out.push(packet);
            }
            Err(DecodeError::Incomplete { .. }) => return Ok(()),
            Err(_) => return Err(()),
        }
    }
}

fn read_packets_until(
    stream: &mut TcpStream,
    buffer: &mut Vec<u8>,
    deadline: Instant,
) -> Result<Vec<Packet>, ()> {
    let mut packets = Vec::new();
    loop {
        decode_available(buffer, &mut packets)?;
        if !packets.is_empty() || Instant::now() >= deadline {
            return Ok(packets);
        }
        let mut read_buf = [0u8; 4096];
        match stream.read(&mut read_buf) {
            Ok(0) => return Err(()),
            Ok(read) => buffer.extend_from_slice(&read_buf[..read]),
            Err(error) if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
            Err(_) => return Err(()),
        }
    }
}

fn wait_for_packet<F: Fn(&Packet) -> bool>(
    stream: &mut TcpStream,
    buffer: &mut Vec<u8>,
    timeout: Duration,
    predicate: F,
) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        let Ok(packets) = read_packets_until(stream, buffer, deadline) else {
            return false;
        };
        if packets.iter().any(&predicate) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
    }
}

fn spawn_fake_worker(port: u16, mode: WorkerMode) -> JoinHandle<()> {
    thread::spawn(move || fake_worker_main(port, mode))
}

fn fake_worker_main(port: u16, mode: WorkerMode) {
    let Ok(mut stream) = TcpStream::connect(("127.0.0.1", port)) else {
        return;
    };
    stream.set_nodelay(true).ok();
    if stream
        .set_read_timeout(Some(Duration::from_millis(10)))
        .is_err()
    {
        return;
    }
    let mut buffer: Vec<u8> = Vec::new();

    let in_topic = format!("{TOPIC_ROOT}/dev/{WORKER_ID}/in");
    let out_topic = format!("{TOPIC_ROOT}/dev/{WORKER_ID}/out");
    let state_topic = format!("{TOPIC_ROOT}/dev/{WORKER_ID}/state");

    let mut connect_properties = Properties::new();
    connect_properties.receive_maximum = Some(8);
    connect_properties.maximum_packet_size = Some(16 * 1024);
    connect_properties.session_expiry_interval = Some(0);
    let connect = Packet::Connect(Connect {
        clean_start: true,
        keep_alive: 30,
        client_id: String::from(WORKER_ID),
        will: None,
        username: None,
        password: None,
        properties: connect_properties,
    });
    if write_packet(&mut stream, &connect).is_err() {
        return;
    }
    if !wait_for_packet(
        &mut stream,
        &mut buffer,
        Duration::from_secs(5),
        |packet| matches!(packet, Packet::Connack(connack) if connack.reason_code == ReasonCode::Success),
    ) {
        return;
    }

    let subscribe = Packet::Subscribe(Subscribe {
        packet_id: 1,
        properties: Properties::new(),
        subscriptions: vec![Subscription {
            filter: TopicFilter::new(in_topic.clone()).expect("in filter"),
            options: SubscriptionOptions::new(QoS::AtLeastOnce, false, true, 0).expect("options"),
        }],
    });
    if write_packet(&mut stream, &subscribe).is_err() {
        return;
    }
    if !wait_for_packet(&mut stream, &mut buffer, Duration::from_secs(5), |packet| {
        matches!(packet, Packet::Suback(_))
    }) {
        return;
    }

    let hello = Hello {
        worker_id: String::from(WORKER_ID),
        firmware: String::from("0.1.0-test"),
        uptime_ms: 10,
        unix_ms: unix_ms(),
        tasks: vec![String::from("echo")],
        tools: vec![
            String::from("gpio.config"),
            String::from("gpio.info"),
            String::from("gpio.read"),
            String::from("gpio.write"),
            String::from("sekai.info"),
        ],
        workers: DeviceList::new(),
        active_tasks: Vec::new(),
    };
    let hello_stdout = Stdout::new(
        1,
        0,
        1,
        Phase::Hello,
        unix_ms(),
        hello.encode().expect("Hello 编码"),
    )
    .expect("Hello 帧");
    if write_stdout(&mut stream, &state_topic, &hello_stdout, true, 2).is_err() {
        return;
    }

    let mut seq = 2u32;
    let mut packet_id = 3u16;
    loop {
        let deadline = Instant::now() + Duration::from_millis(20);
        let Ok(packets) = read_packets_until(&mut stream, &mut buffer, deadline) else {
            return;
        };
        for packet in packets {
            match packet {
                Packet::Publish(publish) => {
                    let Some(topic) = publish.topic.as_ref().map(|topic| topic.as_str()) else {
                        continue;
                    };
                    if topic != in_topic.as_str() {
                        continue;
                    }
                    let Ok(stdin) = Stdin::decode(&publish.payload) else {
                        continue;
                    };
                    if mode == WorkerMode::Silent {
                        continue;
                    }
                    let accepted =
                        Stdout::new(1, stdin.id, seq, Phase::Accepted, unix_ms(), Vec::new())
                            .expect("Accepted 帧");
                    if write_stdout(&mut stream, &out_topic, &accepted, false, packet_id).is_err() {
                        return;
                    }
                    seq = seq.wrapping_add(1);
                    packet_id = next_packet_id(packet_id);
                    let frame = match mode {
                        WorkerMode::Respond => Stdout::new(
                            1,
                            stdin.id,
                            seq,
                            Phase::Completed,
                            unix_ms(),
                            b"pin=IO4 mode=unconfigured".to_vec(),
                        )
                        .expect("Completed 帧"),
                        WorkerMode::Reject => {
                            let error = ErrorPayload::new(ErrorCode::InvalidArgument, "bad pin");
                            Stdout::new(
                                1,
                                stdin.id,
                                seq,
                                Phase::Failed,
                                unix_ms(),
                                error.encode().expect("错误载荷"),
                            )
                            .expect("Failed 帧")
                        }
                        WorkerMode::Silent => return,
                    };
                    if write_stdout(&mut stream, &out_topic, &frame, false, packet_id).is_err() {
                        return;
                    }
                    seq = seq.wrapping_add(1);
                    packet_id = next_packet_id(packet_id);
                }
                Packet::Pingreq => {
                    if write_packet(&mut stream, &Packet::Pingresp).is_err() {
                        return;
                    }
                }
                Packet::Disconnect(_) => return,
                _ => {}
            }
        }
    }
}

fn write_stdout(
    stream: &mut TcpStream,
    topic: &str,
    stdout: &Stdout,
    retain: bool,
    packet_id: u16,
) -> std::io::Result<()> {
    let mut properties = Properties::new();
    properties.content_type = Some(String::from(CONTENT_TYPE_STDOUT));
    let packet = Packet::Publish(Publish {
        dup: false,
        qos: QoS::AtLeastOnce,
        retain,
        topic: Some(TopicName::new(topic).expect("主题名")),
        packet_id: Some(packet_id),
        properties,
        payload: stdout.encode().expect("Stdout 编码"),
    });
    write_packet(stream, &packet)
}

fn next_packet_id(current: u16) -> u16 {
    if current == u16::MAX { 1 } else { current + 1 }
}

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

// ============================ 测试 ============================

fn gateway_config(port: u16) -> MqttGatewayConfig {
    let topic_root = TopicRoot::new(TOPIC_ROOT).expect("主题根");
    let endpoint = Endpoint::new("127.0.0.1", port).expect("端点");
    let mut config = MqttGatewayConfig::new(endpoint, topic_root);
    config.client_id = Some(String::from("sekai-router-gateway-live-test"));
    config
}

fn wait_until(timeout: Duration, mut predicate: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if predicate() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(SHORT_WAIT);
    }
}

fn wait_for_worker(gateway: &mut MqttGateway, timeout: Duration) -> bool {
    wait_until(timeout, || {
        gateway
            .list_workers()
            .iter()
            .any(|info| info.worker_id == WORKER_ID && info.online)
    })
}

#[test]
fn gateway_connects_lists_and_calls_worker() {
    let mut broker = MiniBroker::start();
    let _worker = spawn_fake_worker(broker.port, WorkerMode::Respond);
    let mut gateway = MqttGateway::connect(gateway_config(broker.port)).expect("gateway connect");

    gateway.wait_ready(5_000).expect("应连接并订阅成功");
    assert!(gateway.is_ready());
    assert!(wait_for_worker(&mut gateway, Duration::from_secs(5)));

    let workers = gateway.list_workers();
    let info = workers
        .iter()
        .find(|info| info.worker_id == WORKER_ID)
        .expect("应发现 dev-1");
    assert!(info.online);
    assert!(info.has_tool("gpio.info"));
    assert!(info.has_task("echo"));

    let result = gateway
        .call(CommandRequest::tool(WORKER_ID, "gpio.info", "IO4"))
        .expect("call 应成功返回结果");
    assert!(result.ok());
    assert_eq!(result.worker_id, WORKER_ID);
    assert_eq!(result.phase_names(), vec!["Accepted", "Completed"]);
    assert_eq!(result.payload, b"pin=IO4 mode=unconfigured");

    // 稳定错误码：未通告工具、未知Worker、未通告任务。
    let error = gateway
        .call(CommandRequest::tool(WORKER_ID, "no.such", ""))
        .expect_err("未通告工具应失败");
    assert_eq!(error.code, "not_advertised");
    let error = gateway
        .call(CommandRequest::tool("missing", "gpio.info", ""))
        .expect_err("未知Worker应失败");
    assert_eq!(error.code, "worker_not_found");
    let error = gateway
        .call(CommandRequest::task(
            WORKER_ID,
            "no-such-task",
            b"x".to_vec(),
        ))
        .expect_err("未通告任务应失败");
    assert_eq!(error.code, "not_advertised");

    let started = Instant::now();
    gateway.shutdown();
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "shutdown 不应挂起"
    );
    broker.shutdown();
}

#[test]
fn gateway_worker_failure_is_returned_as_result() {
    let mut broker = MiniBroker::start();
    let _worker = spawn_fake_worker(broker.port, WorkerMode::Reject);
    let mut gateway = MqttGateway::connect(gateway_config(broker.port)).expect("gateway connect");
    gateway.wait_ready(5_000).expect("应就绪");
    assert!(wait_for_worker(&mut gateway, Duration::from_secs(5)));

    let result = gateway
        .call(CommandRequest::tool(WORKER_ID, "gpio.info", "IO4"))
        .expect("call 应返回最终结果");
    assert!(!result.ok());
    let failure = result.failure.expect("应携带失败信息");
    assert_eq!(failure.code, "worker_failed");
    assert_eq!(failure.worker_code, Some(ErrorCode::InvalidArgument));
    assert!(failure.message.contains("bad pin"));

    drop(gateway);
    broker.shutdown();
}

#[test]
fn gateway_call_timeout_returns_failure_result() {
    let mut broker = MiniBroker::start();
    let _worker = spawn_fake_worker(broker.port, WorkerMode::Silent);
    let mut gateway = MqttGateway::connect(gateway_config(broker.port)).expect("gateway connect");
    gateway.wait_ready(5_000).expect("应就绪");
    assert!(wait_for_worker(&mut gateway, Duration::from_secs(5)));

    let started = Instant::now();
    let result = gateway
        .call(CommandRequest::tool(WORKER_ID, "gpio.info", "IO4").with_timeout(150))
        .expect("call 应返回最终结果而不是挂起");
    assert!(started.elapsed() < Duration::from_secs(3));
    assert!(!result.ok());
    assert_eq!(result.failure.expect("失败信息").code, "timeout");

    drop(gateway);
    broker.shutdown();
}

#[test]
fn gateway_reconnects_and_rediscovers_retained_hello() {
    let mut broker = MiniBroker::start();
    let _worker = spawn_fake_worker(broker.port, WorkerMode::Respond);
    let mut gateway = MqttGateway::connect(gateway_config(broker.port)).expect("gateway connect");
    gateway.wait_ready(5_000).expect("应就绪");
    assert!(wait_for_worker(&mut gateway, Duration::from_secs(5)));

    // 断开所有 TCP 连接（模拟网络抖动），broker 继续监听且保留 retained 消息。
    broker.drop_connections();
    assert!(
        wait_until(Duration::from_secs(3), || !gateway.is_ready()),
        "断链后 ready 应变为 false"
    );

    gateway
        .wait_ready(8_000)
        .expect("worker 应退避重连并重新就绪");
    assert!(gateway.is_ready());
    assert!(
        wait_for_worker(&mut gateway, Duration::from_secs(5)),
        "重连后应通过 retained Hello 重新发现Worker"
    );

    drop(gateway);
    broker.shutdown();
}

#[test]
fn gateway_waits_ready_for_unreachable_broker_then_fails_fast() {
    // 只 bind 不 accept：TCP 可连接但永远不会返回 CONNACK。
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("绑定端口");
    let port = listener.local_addr().expect("local addr").port();
    let mut gateway = MqttGateway::connect(gateway_config(port)).expect("gateway connect");

    let error = gateway.wait_ready(400).expect_err("不可达 broker 不应就绪");
    assert!(
        matches!(error.code.as_str(), "link_lost" | "timeout"),
        "code = {}",
        error.code
    );
    assert!(!gateway.is_ready());

    let started = Instant::now();
    let error = gateway
        .call(CommandRequest::tool(WORKER_ID, "gpio.info", ""))
        .expect_err("未就绪时 call 应快速失败");
    assert_eq!(error.code, "link_lost");
    assert!(started.elapsed() < Duration::from_secs(2));

    let started = Instant::now();
    drop(gateway);
    assert!(started.elapsed() < Duration::from_secs(3), "Drop 不应挂起");
    drop(listener);
}
