//! `send` 特性测试：注册表/运行时的 `Send` 编译断言与最小运行时烟测。
//!
//! 该特性是 ESP32 Embassy 异步运行时的前置条件：`Runtime` 必须能跨
//! `await` 边界持有。默认构建（不带 `send`）不编译本文件。

#![cfg(feature = "send")]

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use sekai::mqtt::packet::{Ack, Connack, Packet, Publish, Suback};
use sekai::mqtt::property::Properties;
use sekai::mqtt::qos::QoS;
use sekai::mqtt::reason::ReasonCode;
use sekai::mqtt::topic::TopicName;
use sekai::sekai_clock::Clock;
use sekai::sekai_device_list::{DeviceList, Endpoint};
use sekai::sekai_runtime::{Runtime, RuntimeConfig};
use sekai::sekai_stdin::{Kind, Stdin, ToolBody};
use sekai::sekai_stdout::{Phase, Stdout};
use sekai::sekai_task::{BoxedTask, TaskContext, TaskHandler, TaskPoll, TaskRegistry};
use sekai::sekai_tool::{BoxedTool, ToolContext, ToolHandler, ToolRegistry};
use sekai::sekai_wire::CommandError;

/// 编译期断言：`T: Send`。
fn assert_send<T: Send>() {}

#[derive(Clone)]
struct SendClock {
    state: Arc<ClockState>,
}

struct ClockState {
    mono: AtomicU64,
    unix: AtomicU64,
}

impl SendClock {
    fn new() -> Self {
        Self {
            state: Arc::new(ClockState {
                mono: AtomicU64::new(0),
                unix: AtomicU64::new(1_700_000_000_000),
            }),
        }
    }

    fn advance(&self, ms: u64) {
        self.state.mono.fetch_add(ms, Ordering::Relaxed);
    }
}

impl Clock for SendClock {
    fn monotonic_ms(&self) -> u64 {
        self.state.mono.load(Ordering::Relaxed)
    }

    fn unix_ms(&self) -> Option<u64> {
        Some(
            self.state
                .mono
                .load(Ordering::Relaxed)
                .saturating_add(self.state.unix.load(Ordering::Relaxed)),
        )
    }

    fn calibrate_unix_ms(&self, unix_ms: u64) {
        self.state.unix.store(unix_ms, Ordering::Relaxed);
    }
}

struct EchoTool;

impl ToolHandler for EchoTool {
    fn call(&mut self, ctx: &mut ToolContext<'_>) -> Result<Vec<u8>, CommandError> {
        Ok(ctx.args.to_vec())
    }
}

struct EchoTask {
    args: Vec<u8>,
}

impl TaskHandler for EchoTask {
    fn poll(&mut self, _ctx: &mut TaskContext<'_>) -> TaskPoll {
        TaskPoll::Done(self.args.clone())
    }
}

fn endpoint(port: u16) -> Endpoint {
    Endpoint {
        host: String::from("127.0.0.1"),
        port,
    }
}

struct Harness {
    runtime: Runtime<SendClock>,
    clock: SendClock,
    in_topic: TopicName,
    next_cmd_id: u64,
    next_packet_id: u16,
}

impl Harness {
    fn connected() -> Self {
        let clock = SendClock::new();
        let mut tasks = TaskRegistry::new();
        tasks
            .register("echo", |args| {
                Ok(Box::new(EchoTask {
                    args: args.to_vec(),
                }))
            })
            .unwrap();
        let mut tools = ToolRegistry::new();
        tools.register("echo", EchoTool).unwrap();

        let config = RuntimeConfig::new("send-demo", 7);
        let runtime = Runtime::new(config, clock.clone(), DeviceList::new(), tasks, tools).unwrap();

        let mut harness = Self {
            runtime,
            clock,
            in_topic: TopicName::new("sekai/v1/dev/send-demo/in").unwrap(),
            next_cmd_id: 1,
            next_packet_id: 1,
        };
        harness.handshake();
        harness
    }

    fn handshake(&mut self) {
        let endpoint = endpoint(1883);
        self.runtime.link_up(endpoint).unwrap();
        let packets = self.drain_packets();
        assert!(
            packets
                .iter()
                .any(|packet| matches!(packet, Packet::Connect(_))),
            "缺少 CONNECT"
        );

        self.runtime
            .feed(Packet::Connack(Connack {
                session_present: false,
                reason_code: ReasonCode::Success,
                properties: Properties::new(),
            }))
            .unwrap();
        self.runtime.poll().unwrap();

        let packets = self.drain_packets();
        let subscribe_id = packets
            .iter()
            .find_map(|packet| match packet {
                Packet::Subscribe(subscribe) => Some(subscribe.packet_id),
                _ => None,
            })
            .expect("缺少 SUBSCRIBE");
        let hello_publish = packets
            .iter()
            .find_map(|packet| match packet {
                Packet::Publish(publish) => Some(publish.clone()),
                _ => None,
            })
            .expect("缺少 Hello PUBLISH");
        let stdout = Stdout::decode(&hello_publish.payload).unwrap();
        assert_eq!(stdout.phase, Phase::Hello);
        if let Some(packet_id) = hello_publish.packet_id {
            self.ack(packet_id);
        }
        self.runtime
            .feed(Packet::Suback(Suback {
                packet_id: subscribe_id,
                properties: Properties::new(),
                reason_codes: vec![ReasonCode::GrantedQos1],
            }))
            .unwrap();
        assert!(self.runtime.status().subscribed);
    }

    fn drain_packets(&mut self) -> Vec<Packet> {
        let mut packets = Vec::new();
        while let Some(packet) = self.runtime.next_outgoing() {
            packets.push(packet);
        }
        packets
    }

    fn ack(&mut self, packet_id: u16) {
        self.runtime
            .feed(Packet::Puback(Ack {
                packet_id,
                reason_code: ReasonCode::Success,
                properties: Properties::new(),
            }))
            .unwrap();
    }

    fn drain_stdouts(&mut self) -> Vec<Stdout> {
        let packets = self.drain_packets();
        let mut acks = Vec::new();
        let mut stdouts = Vec::new();
        for packet in packets {
            if let Packet::Publish(publish) = packet {
                let stdout = Stdout::decode(&publish.payload).unwrap();
                if let Some(packet_id) = publish.packet_id {
                    acks.push(packet_id);
                }
                stdouts.push(stdout);
            }
        }
        for packet_id in acks {
            self.ack(packet_id);
        }
        stdouts
    }

    fn pump(&mut self) -> Vec<Stdout> {
        self.runtime.poll().unwrap();
        self.drain_stdouts()
    }

    fn call_tool(&mut self, name: &str, args: &[u8]) -> Vec<Stdout> {
        let body = ToolBody::new(name, args.to_vec()).unwrap();
        let stdin = Stdin::new(
            self.next_cmd_id,
            Kind::ToolCall,
            self.clock.unix_ms().unwrap(),
            0,
            500,
            body.encode().unwrap(),
        )
        .unwrap();
        self.next_cmd_id += 1;
        let payload = stdin.encode().unwrap();
        let packet_id = self.next_packet_id;
        self.next_packet_id += 1;
        self.runtime
            .feed(Packet::Publish(Publish {
                dup: false,
                qos: QoS::AtLeastOnce,
                retain: false,
                topic: Some(self.in_topic.clone()),
                packet_id: Some(packet_id),
                properties: Properties::new(),
                payload,
            }))
            .unwrap();
        self.pump()
    }
}

#[test]
fn runtime_and_registries_are_send() {
    assert_send::<Runtime<SendClock>>();
    assert_send::<ToolRegistry>();
    assert_send::<TaskRegistry>();
    assert_send::<BoxedTool>();
    assert_send::<BoxedTask>();

    // 处理器 trait object 在 send 构建下可跨线程移动：用真实线程验证。
    let mut tools = ToolRegistry::new();
    tools.register("echo", EchoTool).unwrap();
    let mut runtime = Runtime::new(
        RuntimeConfig::new("send-thread", 1),
        SendClock::new(),
        DeviceList::new(),
        TaskRegistry::new(),
        tools,
    )
    .unwrap();
    let handle = std::thread::spawn(move || {
        runtime.link_up(endpoint(1883)).unwrap();
        runtime.next_outgoing().is_some()
    });
    assert!(handle.join().unwrap(), "Runtime 应能在其他线程中使用");
}

#[test]
fn send_harness_runs_tool_call() {
    let mut harness = Harness::connected();
    let events = harness.call_tool("echo", b"hello");
    let phases: Vec<Phase> = events.iter().map(|stdout| stdout.phase).collect();
    assert_eq!(phases, vec![Phase::Accepted, Phase::Completed]);
    assert_eq!(events[1].payload, b"hello");
    assert_eq!(events[1].cmd_id, 1);

    // 时钟推进后 keep alive 等定时器不 panic。
    harness.clock.advance(1_000);
    let _ = harness.pump();
}

#[test]
fn send_harness_runs_task() {
    let mut harness = Harness::connected();
    let body = sekai::sekai_stdin::TaskBody::new("echo", b"task".to_vec()).unwrap();
    let stdin = Stdin::new(
        harness.next_cmd_id,
        Kind::Task,
        harness.clock.unix_ms().unwrap(),
        0,
        500,
        body.encode().unwrap(),
    )
    .unwrap();
    harness.next_cmd_id += 1;
    let payload = stdin.encode().unwrap();
    harness
        .runtime
        .feed(Packet::Publish(Publish {
            dup: false,
            qos: QoS::AtLeastOnce,
            retain: false,
            topic: Some(harness.in_topic.clone()),
            packet_id: Some(harness.next_packet_id),
            properties: Properties::new(),
            payload,
        }))
        .unwrap();
    harness.next_packet_id += 1;
    let events = harness.pump();
    let phases: Vec<Phase> = events.iter().map(|stdout| stdout.phase).collect();
    assert_eq!(
        phases,
        vec![Phase::Accepted, Phase::Started, Phase::Completed]
    );
    assert_eq!(events[2].payload, b"task");
}
