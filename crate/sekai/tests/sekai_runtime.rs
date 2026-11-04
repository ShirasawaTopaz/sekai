//! 运行时端到端测试：用合成 MQTT 报文和手动时钟驱动 Task/Tool/Jump。
//!
//! 本文件使用 `Rc<Cell>` 测试时钟，仅适用于默认（非 `send`）构建；
//! `send` 构建的编译断言与烟测见 `tests/send.rs`。

#![cfg(not(feature = "send"))]

use std::cell::Cell;
use std::rc::Rc;

use sekai::mqtt::packet::{Ack, Connack, Disconnect, Packet, Publish, Suback};
use sekai::mqtt::property::Properties;
use sekai::mqtt::qos::QoS;
use sekai::mqtt::reason::ReasonCode;
use sekai::mqtt::topic::TopicName;
use sekai::sekai_clock::Clock;
use sekai::sekai_device_list::{Device, DeviceList, Endpoint};
use sekai::sekai_runtime::{JumpPhase, LinkAction, Runtime, RuntimeConfig, SwitchReason};
use sekai::sekai_stdin::{JumpBody, Kind, Stdin, TaskBody, ToolBody};
use sekai::sekai_stdout::{ErrorPayload, Hello, JumpOutcome, Phase, Stdout};
use sekai::sekai_task::{TaskContext, TaskHandler, TaskPoll, TaskRegistry};
use sekai::sekai_tool::{ToolContext, ToolHandler, ToolRegistry};
use sekai::sekai_wire::{ErrorCode, MAX_PAYLOAD};

#[derive(Clone)]
struct TestClock {
    state: Rc<ClockState>,
}

struct ClockState {
    mono: Cell<u64>,
    unix: Cell<Option<u64>>,
    calibrations: Cell<u32>,
}

impl TestClock {
    fn new() -> Self {
        Self {
            state: Rc::new(ClockState {
                mono: Cell::new(0),
                unix: Cell::new(Some(1_700_000_000_000)),
                calibrations: Cell::new(0),
            }),
        }
    }

    fn advance(&self, ms: u64) {
        self.state
            .mono
            .set(self.state.mono.get().saturating_add(ms));
    }
}

impl Clock for TestClock {
    fn monotonic_ms(&self) -> u64 {
        self.state.mono.get()
    }

    fn unix_ms(&self) -> Option<u64> {
        self.state.unix.get()
    }

    fn calibrate_unix_ms(&self, unix_ms: u64) {
        self.state.unix.set(Some(unix_ms));
        self.state
            .calibrations
            .set(self.state.calibrations.get() + 1);
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

struct HangTask;

impl TaskHandler for HangTask {
    fn poll(&mut self, _ctx: &mut TaskContext<'_>) -> TaskPoll {
        TaskPoll::Pending {
            wake_after_ms: 1_000,
        }
    }
}

struct CountTask {
    counter: Rc<Cell<u32>>,
}

impl TaskHandler for CountTask {
    fn poll(&mut self, _ctx: &mut TaskContext<'_>) -> TaskPoll {
        self.counter.set(self.counter.get() + 1);
        TaskPoll::Done(Vec::new())
    }
}

struct HugeTask;

impl TaskHandler for HugeTask {
    fn poll(&mut self, _ctx: &mut TaskContext<'_>) -> TaskPoll {
        TaskPoll::Done(vec![0; MAX_PAYLOAD + 1])
    }
}

struct EchoTool;

impl ToolHandler for EchoTool {
    fn call(
        &mut self,
        ctx: &mut ToolContext<'_>,
    ) -> Result<Vec<u8>, sekai::sekai_wire::CommandError> {
        Ok(ctx.args.to_vec())
    }
}

struct SlowTool {
    clock: TestClock,
}

impl ToolHandler for SlowTool {
    fn call(
        &mut self,
        ctx: &mut ToolContext<'_>,
    ) -> Result<Vec<u8>, sekai::sekai_wire::CommandError> {
        self.clock.advance(1_000);
        Ok(ctx.args.to_vec())
    }
}

struct Harness {
    runtime: Runtime<TestClock>,
    clock: TestClock,
    endpoint: Endpoint,
    peer: Endpoint,
    in_topic: TopicName,
    out_topic: TopicName,
    state_topic: TopicName,
    next_cmd_id: u64,
    next_packet_id: u16,
    counter: Rc<Cell<u32>>,
}

impl Harness {
    fn new() -> Self {
        Self::build(32)
    }

    fn build(dedup_window: usize) -> Self {
        let clock = TestClock::new();
        let endpoint = Endpoint {
            host: String::from("127.0.0.1"),
            port: 1883,
        };
        let peer = Endpoint {
            host: String::from("127.0.0.1"),
            port: 1884,
        };
        let mut workers = DeviceList::new();
        workers
            .insert(Device::new("peer", "127.0.0.1", 1884).unwrap())
            .unwrap();

        let counter = Rc::new(Cell::new(0));
        let mut tasks = TaskRegistry::new();
        tasks
            .register("echo", |args| {
                Ok(Box::new(EchoTask {
                    args: args.to_vec(),
                }))
            })
            .unwrap();
        tasks.register("hang", |_| Ok(Box::new(HangTask))).unwrap();
        let counter_for_task = Rc::clone(&counter);
        tasks
            .register("count", move |_| {
                Ok(Box::new(CountTask {
                    counter: Rc::clone(&counter_for_task),
                }))
            })
            .unwrap();
        tasks.register("huge", |_| Ok(Box::new(HugeTask))).unwrap();

        let mut tools = ToolRegistry::new();
        tools.register("echo", EchoTool).unwrap();
        tools
            .register(
                "slow",
                SlowTool {
                    clock: clock.clone(),
                },
            )
            .unwrap();

        let mut config = RuntimeConfig::new("demo", 1);
        config.mqtt.maximum_packet_size = Some(16 * 1024);
        config.dedup_window = dedup_window;
        let runtime = Runtime::new(config, clock.clone(), workers, tasks, tools).unwrap();

        Self {
            runtime,
            clock,
            endpoint,
            peer,
            in_topic: TopicName::new("sekai/v1/dev/demo/in").unwrap(),
            out_topic: TopicName::new("sekai/v1/dev/demo/out").unwrap(),
            state_topic: TopicName::new("sekai/v1/dev/demo/state").unwrap(),
            next_cmd_id: 1,
            next_packet_id: 1,
            counter,
        }
    }

    fn connected() -> Self {
        let mut harness = Self::new();
        harness.handshake();
        harness
    }

    fn connected_with_dedup_window(window: usize) -> Self {
        let mut harness = Self::build(window);
        harness.handshake();
        harness
    }

    fn handshake(&mut self) {
        self.runtime.link_up(self.endpoint.clone()).unwrap();
        let packets = self.drain_packets();
        assert!(
            packets
                .iter()
                .any(|packet| matches!(packet, Packet::Connect(_))),
            "missing CONNECT"
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
            .expect("missing SUBSCRIBE");
        let hello_publish = packets
            .iter()
            .find_map(|packet| match packet {
                Packet::Publish(publish)
                    if publish.topic.as_ref().map(TopicName::as_str)
                        == Some(self.state_topic.as_str()) =>
                {
                    Some(publish.clone())
                }
                _ => None,
            })
            .expect("missing Hello");
        let hello_stdout = Stdout::decode(&hello_publish.payload).unwrap();
        assert_eq!(hello_stdout.phase, Phase::Hello);
        let hello = Hello::decode(&hello_stdout.payload).unwrap();
        assert_eq!(hello.worker_id, "demo");
        assert!(hello.tasks.iter().any(|name| name == "echo"));

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
                let expected_topic = match stdout.phase {
                    Phase::Hello | Phase::Offline => self.state_topic.as_str(),
                    _ => self.out_topic.as_str(),
                };
                assert_eq!(
                    publish.topic.as_ref().map(TopicName::as_str),
                    Some(expected_topic)
                );
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

    fn send(&mut self, stdin: Stdin) {
        let payload = stdin.encode().unwrap();
        let packet_id = self.next_packet_id;
        self.next_packet_id = if packet_id == u16::MAX {
            1
        } else {
            packet_id + 1
        };
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
        self.runtime.poll().unwrap();
    }

    fn task_stdin(&mut self, name: &str, args: &[u8], delay_ms: u32, ddl_ms: u32) -> Stdin {
        let body = TaskBody::new(name, args.to_vec()).unwrap();
        let stdin = Stdin::new(
            self.next_cmd_id,
            Kind::Task,
            self.clock.unix_ms().unwrap(),
            delay_ms,
            ddl_ms,
            body.encode().unwrap(),
        )
        .unwrap();
        self.next_cmd_id += 1;
        stdin
    }

    fn tool_stdin(&mut self, name: &str, args: &[u8], delay_ms: u32, ddl_ms: u32) -> Stdin {
        let body = ToolBody::new(name, args.to_vec()).unwrap();
        let stdin = Stdin::new(
            self.next_cmd_id,
            Kind::ToolCall,
            self.clock.unix_ms().unwrap(),
            delay_ms,
            ddl_ms,
            body.encode().unwrap(),
        )
        .unwrap();
        self.next_cmd_id += 1;
        stdin
    }

    fn jump_stdin(&mut self, target: &str) -> Stdin {
        let body = JumpBody::new(target).unwrap();
        let stdin = Stdin::new(
            self.next_cmd_id,
            Kind::Jump,
            self.clock.unix_ms().unwrap(),
            0,
            500,
            body.encode().unwrap(),
        )
        .unwrap();
        self.next_cmd_id += 1;
        stdin
    }
}

fn phases(stdouts: &[Stdout]) -> Vec<Phase> {
    stdouts.iter().map(|stdout| stdout.phase).collect()
}

fn connack(session_present: bool) -> Packet {
    Packet::Connack(Connack {
        session_present,
        reason_code: ReasonCode::Success,
        properties: Properties::new(),
    })
}

#[test]
fn task_completes_and_reports_result() {
    let mut harness = Harness::connected();
    let stdin = harness.task_stdin("echo", b"hi", 0, 500);
    harness.send(stdin);
    let events = harness.pump();
    assert_eq!(
        phases(&events),
        vec![Phase::Accepted, Phase::Started, Phase::Completed]
    );
    assert_eq!(events[2].payload, b"hi");
    assert_eq!(events[2].cmd_id, 1);
    assert_eq!(harness.runtime.status().running, 0);
}

#[test]
fn task_delay_waits_before_start() {
    let mut harness = Harness::connected();
    let stdin = harness.task_stdin("echo", b"hi", 1_000, 500);
    harness.send(stdin);
    assert_eq!(phases(&harness.pump()), vec![Phase::Accepted]);

    harness.clock.advance(999);
    assert!(harness.pump().is_empty());

    harness.clock.advance(1);
    assert_eq!(
        phases(&harness.pump()),
        vec![Phase::Started, Phase::Completed]
    );
}

#[test]
fn task_ddl_times_out() {
    let mut harness = Harness::connected();
    let stdin = harness.task_stdin("hang", b"", 0, 100);
    harness.send(stdin);
    assert_eq!(
        phases(&harness.pump()),
        vec![Phase::Accepted, Phase::Started]
    );

    harness.clock.advance(100);
    let events = harness.pump();
    assert_eq!(phases(&events), vec![Phase::Failed]);
    let error = ErrorPayload::decode(&events[0].payload).unwrap();
    assert_eq!(error.code, ErrorCode::Timeout);
}

#[test]
fn unknown_task_is_rejected_without_accept() {
    let mut harness = Harness::connected();
    let stdin = harness.task_stdin("missing", b"", 0, 500);
    harness.send(stdin);
    let events = harness.pump();
    assert_eq!(phases(&events), vec![Phase::Failed]);
    let error = ErrorPayload::decode(&events[0].payload).unwrap();
    assert_eq!(error.code, ErrorCode::UnknownTask);
}

#[test]
fn duplicate_command_replays_final_without_reexecution() {
    let mut harness = Harness::connected();
    let stdin = harness.task_stdin("count", b"", 0, 500);
    harness.send(stdin.clone());
    let first = harness.pump();
    assert_eq!(
        phases(&first),
        vec![Phase::Accepted, Phase::Started, Phase::Completed]
    );
    assert_eq!(harness.counter.get(), 1);

    harness.send(stdin);
    let second = harness.pump();
    assert_eq!(phases(&second), vec![Phase::Completed]);
    assert_eq!(second[0].seq, first[2].seq);
    assert_eq!(harness.counter.get(), 1);
}

#[test]
fn oversized_task_result_is_failed() {
    let mut harness = Harness::connected();
    let stdin = harness.task_stdin("huge", b"", 0, 500);
    harness.send(stdin);
    let events = harness.pump();
    assert_eq!(
        phases(&events),
        vec![Phase::Accepted, Phase::Started, Phase::Failed]
    );
    let error = ErrorPayload::decode(&events[2].payload).unwrap();
    assert_eq!(error.code, ErrorCode::ResultTooLarge);
}

#[test]
fn builtin_time_tool_returns_sixteen_bytes() {
    let mut harness = Harness::connected();
    let stdin = harness.tool_stdin("sekai.time", b"", 0, 500);
    harness.send(stdin);
    let events = harness.pump();
    assert_eq!(phases(&events), vec![Phase::Accepted, Phase::Completed]);
    assert_eq!(events[1].payload.len(), 16);
}

#[test]
fn slow_tool_times_out() {
    let mut harness = Harness::connected();
    let stdin = harness.tool_stdin("slow", b"", 0, 50);
    harness.send(stdin);
    let events = harness.pump();
    assert_eq!(phases(&events), vec![Phase::Accepted, Phase::Failed]);
    let error = ErrorPayload::decode(&events[1].payload).unwrap();
    assert_eq!(error.code, ErrorCode::Timeout);
}

#[test]
fn jump_success_moves_link_and_reports_outcome() {
    let mut harness = Harness::connected();
    let stdin = harness.jump_stdin("peer");
    harness.send(stdin);
    let events = harness.pump();
    assert_eq!(phases(&events), vec![Phase::Accepted, Phase::Offline]);

    harness.runtime.poll().unwrap();
    let packets = harness.drain_packets();
    assert!(
        packets
            .iter()
            .any(|packet| matches!(packet, Packet::Disconnect(_))),
        "missing DISCONNECT"
    );
    let action = harness.runtime.take_link_action().expect("missing switch");
    match action {
        LinkAction::Switch { endpoint, reason } => {
            assert_eq!(endpoint.port, 1884);
            assert_eq!(reason, SwitchReason::JumpCommand);
        }
    }

    harness.runtime.link_closed();
    harness.runtime.link_up(harness.peer.clone()).unwrap();
    assert!(
        harness
            .drain_packets()
            .iter()
            .any(|packet| matches!(packet, Packet::Connect(_)))
    );
    harness.runtime.feed(connack(false)).unwrap();
    harness.runtime.poll().unwrap();

    let packets = harness.drain_packets();
    let mut hello_seen = false;
    let mut completed = None;
    let mut subscribe_id = None;
    for packet in packets {
        match packet {
            Packet::Subscribe(subscribe) => subscribe_id = Some(subscribe.packet_id),
            Packet::Publish(publish) => {
                let stdout = Stdout::decode(&publish.payload).unwrap();
                if let Some(packet_id) = publish.packet_id {
                    harness.ack(packet_id);
                }
                match stdout.phase {
                    Phase::Hello => hello_seen = true,
                    Phase::Completed => completed = Some(stdout),
                    _ => {}
                }
            }
            _ => {}
        }
    }
    assert!(hello_seen, "missing Hello after jump");
    if let Some(packet_id) = subscribe_id {
        harness
            .runtime
            .feed(Packet::Suback(Suback {
                packet_id,
                properties: Properties::new(),
                reason_codes: vec![ReasonCode::GrantedQos1],
            }))
            .unwrap();
    }
    let completed = completed.expect("missing jump completion");
    let outcome = JumpOutcome::decode(&completed.payload).unwrap();
    assert_eq!(outcome.endpoint.port, 1884);
    assert!(harness.runtime.status().jump.is_none());
}

#[test]
fn jump_failure_falls_back_and_reports_error() {
    let mut harness = Harness::connected();
    let stdin = harness.jump_stdin("peer");
    harness.send(stdin);
    let events = harness.pump();
    assert_eq!(phases(&events), vec![Phase::Accepted, Phase::Offline]);

    harness.runtime.poll().unwrap();
    harness.drain_packets();
    let action = harness.runtime.take_link_action().expect("missing switch");
    match action {
        LinkAction::Switch { endpoint, .. } => assert_eq!(endpoint.port, 1884),
    }

    harness.runtime.link_closed();
    harness.runtime.link_failed(harness.peer.clone());
    let action = harness
        .runtime
        .take_link_action()
        .expect("missing fallback");
    match action {
        LinkAction::Switch { endpoint, reason } => {
            assert_eq!(endpoint.port, 1883);
            assert_eq!(reason, SwitchReason::Fallback);
        }
    }

    harness.runtime.link_up(harness.endpoint.clone()).unwrap();
    harness.drain_packets();
    harness.runtime.feed(connack(false)).unwrap();
    harness.runtime.poll().unwrap();
    let packets = harness.drain_packets();
    let mut failed = None;
    for packet in packets {
        if let Packet::Publish(publish) = packet {
            let stdout = Stdout::decode(&publish.payload).unwrap();
            if let Some(packet_id) = publish.packet_id {
                harness.ack(packet_id);
            }
            if stdout.phase == Phase::Failed {
                failed = Some(stdout);
            }
        }
    }
    let failed = failed.expect("missing jump failure");
    let error = ErrorPayload::decode(&failed.payload).unwrap();
    assert_eq!(error.code, ErrorCode::JumpUnreachable);
    assert!(harness.runtime.status().jump.is_none());
}

#[test]
fn server_move_triggers_link_switch() {
    let mut harness = Harness::connected();
    let mut properties = Properties::new();
    properties.server_reference = Some(String::from("127.0.0.1:1884"));
    harness
        .runtime
        .feed(Packet::Disconnect(Disconnect {
            reason_code: ReasonCode::ServerMoved,
            properties,
        }))
        .unwrap();
    harness.runtime.link_lost();

    let action = harness.runtime.take_link_action().expect("missing move");
    match action {
        LinkAction::Switch { endpoint, reason } => {
            assert_eq!(endpoint.port, 1884);
            assert_eq!(reason, SwitchReason::ServerMoved);
        }
    }

    harness.runtime.link_up(harness.peer.clone()).unwrap();
    harness.drain_packets();
    harness.runtime.feed(connack(false)).unwrap();
    harness.runtime.poll().unwrap();
    let packets = harness.drain_packets();
    let mut hello_seen = false;
    let mut completed_seen = false;
    for packet in packets {
        if let Packet::Publish(publish) = packet {
            let stdout = Stdout::decode(&publish.payload).unwrap();
            if let Some(packet_id) = publish.packet_id {
                harness.ack(packet_id);
            }
            match stdout.phase {
                Phase::Hello => hello_seen = true,
                Phase::Completed => completed_seen = true,
                _ => {}
            }
        }
    }
    assert!(hello_seen);
    assert!(
        !completed_seen,
        "server move must not emit a command result"
    );
    assert_eq!(harness.runtime.status().jump, None);
}

#[test]
fn jump_invalid_target_is_rejected() {
    let mut harness = Harness::connected();
    let stdin = harness.jump_stdin("missing");
    harness.send(stdin);
    let events = harness.pump();
    assert_eq!(phases(&events), vec![Phase::Failed]);
    let error = ErrorPayload::decode(&events[0].payload).unwrap();
    assert_eq!(error.code, ErrorCode::JumpInvalidTarget);
    assert!(harness.runtime.take_link_action().is_none());
}

#[test]
fn jump_phase_is_visible_in_status() {
    let mut harness = Harness::connected();
    let stdin = harness.jump_stdin("peer");
    harness.send(stdin);
    assert_eq!(harness.runtime.status().jump, Some(JumpPhase::Draining));
    let _ = harness.pump();
    harness.runtime.poll().unwrap();
    assert_eq!(harness.runtime.status().jump, Some(JumpPhase::Switching));
}

#[test]
fn builtin_workers_and_set_reflect_state() {
    let mut harness = Harness::connected();

    let list = harness.tool_stdin("sekai.workers", b"", 0, 500);
    harness.send(list);
    let events = harness.pump();
    assert_eq!(phases(&events), vec![Phase::Accepted, Phase::Completed]);
    let workers = DeviceList::decode(&events[1].payload).unwrap();
    assert_eq!(workers.get("peer").unwrap().port, 1884);

    let empty = DeviceList::new().encode().unwrap();
    let set = harness.tool_stdin("sekai.workers.set", &empty, 0, 500);
    harness.send(set);
    let events = harness.pump();
    assert_eq!(
        phases(&events),
        vec![Phase::Accepted, Phase::Hello, Phase::Completed]
    );

    let list = harness.tool_stdin("sekai.workers", b"", 0, 500);
    harness.send(list);
    let events = harness.pump();
    let workers = DeviceList::decode(&events[1].payload).unwrap();
    assert!(workers.is_empty());
}

#[test]
fn builtin_set_time_updates_clock() {
    let mut harness = Harness::connected();
    let target = 1_700_000_123_000u64;
    let set = harness.tool_stdin("sekai.set_time", &target.to_be_bytes(), 0, 500);
    harness.send(set);
    let events = harness.pump();
    assert_eq!(phases(&events), vec![Phase::Accepted, Phase::Completed]);
    let previous = u64::from_be_bytes(events[1].payload.as_slice().try_into().unwrap());
    assert_eq!(previous, 1_700_000_000_000);
    assert_eq!(harness.clock.unix_ms(), Some(target));
}

#[test]
fn builtin_info_reports_active_tasks() {
    let mut harness = Harness::connected();
    let stdin = harness.task_stdin("hang", b"", 0, 5_000);
    let task_id = stdin.id;
    harness.send(stdin);
    let events = harness.pump();
    assert_eq!(phases(&events), vec![Phase::Accepted, Phase::Started]);

    let info = harness.tool_stdin("sekai.info", b"", 0, 500);
    harness.send(info);
    let events = harness.pump();
    assert_eq!(phases(&events), vec![Phase::Accepted, Phase::Completed]);
    let hello = Hello::decode(&events[1].payload).unwrap();
    assert!(
        hello
            .active_tasks
            .iter()
            .any(|active| active.cmd_id == task_id)
    );
}

#[test]
fn builtin_cancel_cancels_delayed_task() {
    let mut harness = Harness::connected();
    let stdin = harness.task_stdin("echo", b"hi", 5_000, 500);
    let task_id = stdin.id;
    harness.send(stdin);
    assert_eq!(phases(&harness.pump()), vec![Phase::Accepted]);

    let cancel = harness.tool_stdin("sekai.cancel", &task_id.to_be_bytes(), 0, 500);
    harness.send(cancel);
    let events = harness.pump();
    assert_eq!(
        phases(&events),
        vec![Phase::Accepted, Phase::Failed, Phase::Completed]
    );
    assert_eq!(events[1].cmd_id, task_id);
    let error = ErrorPayload::decode(&events[1].payload).unwrap();
    assert_eq!(error.code, ErrorCode::Cancelled);
}

#[test]
fn dedup_window_still_protects_active_command() {
    let mut harness = Harness::connected_with_dedup_window(1);
    let long = harness.task_stdin("hang", b"", 0, 5_000);
    harness.send(long.clone());
    assert_eq!(
        phases(&harness.pump()),
        vec![Phase::Accepted, Phase::Started]
    );

    // 新命令把 id=1 挤出 1 个 id 的去重窗口；但 id=1 仍在运行。
    let other = harness.task_stdin("echo", b"", 0, 500);
    harness.send(other);
    assert!(
        harness
            .pump()
            .iter()
            .any(|event| event.phase == Phase::Completed)
    );

    harness.send(long);
    assert!(harness.pump().is_empty());
    assert_eq!(harness.runtime.status().running, 1);
}

#[test]
fn parse_error_replays_same_final() {
    let mut harness = Harness::connected();
    let stdin = Stdin::new(
        77,
        Kind::Task,
        harness.clock.unix_ms().unwrap(),
        0,
        500,
        vec![0xFF, 0xFF],
    )
    .unwrap();
    harness.send(stdin.clone());
    let first = harness.pump();
    assert_eq!(phases(&first), vec![Phase::Failed]);
    let seq = first[0].seq;

    harness.send(stdin);
    let second = harness.pump();
    assert_eq!(phases(&second), vec![Phase::Failed]);
    assert_eq!(second[0].seq, seq);
    let error = ErrorPayload::decode(&second[0].payload).unwrap();
    assert_eq!(error.code, ErrorCode::InvalidArgument);
}

#[test]
fn outbox_requeues_inflight_after_session_reset() {
    let mut harness = Harness::connected();
    let stdin = harness.task_stdin("echo", b"hi", 0, 500);
    harness.send(stdin);
    let packets = harness.drain_packets();
    let completed = packets
        .iter()
        .find_map(|packet| match packet {
            Packet::Publish(publish) => {
                let stdout = Stdout::decode(&publish.payload).unwrap();
                (stdout.phase == Phase::Completed).then_some(stdout)
            }
            _ => None,
        })
        .expect("missing completed");
    // 不喂 PUBACK：事件停留在 MQTT inflight 与发件箱中。
    harness.runtime.link_lost();
    harness.runtime.link_up(harness.endpoint.clone()).unwrap();
    harness.drain_packets();
    harness.runtime.feed(connack(false)).unwrap();
    harness.runtime.poll().unwrap();

    let packets = harness.drain_packets();
    let republished = packets
        .iter()
        .find_map(|packet| match packet {
            Packet::Publish(publish) => {
                let stdout = Stdout::decode(&publish.payload).unwrap();
                (stdout.phase == Phase::Completed).then_some(stdout)
            }
            _ => None,
        })
        .expect("missing republished completed");
    assert_eq!(republished.seq, completed.seq);
    assert_eq!(republished.cmd_id, completed.cmd_id);
}

#[test]
fn jump_target_refused_falls_back() {
    let mut harness = Harness::connected();
    let stdin = harness.jump_stdin("peer");
    harness.send(stdin);
    assert_eq!(
        phases(&harness.pump()),
        vec![Phase::Accepted, Phase::Offline]
    );

    harness.runtime.poll().unwrap();
    harness.drain_packets();
    match harness.runtime.take_link_action().expect("missing switch") {
        LinkAction::Switch { endpoint, .. } => assert_eq!(endpoint.port, 1884),
    }

    harness.runtime.link_closed();
    harness.runtime.link_up(harness.peer.clone()).unwrap();
    harness.drain_packets();
    harness
        .runtime
        .feed(Packet::Connack(Connack {
            session_present: false,
            reason_code: ReasonCode::ServerUnavailable,
            properties: Properties::new(),
        }))
        .unwrap();
    match harness
        .runtime
        .take_link_action()
        .expect("missing fallback")
    {
        LinkAction::Switch { endpoint, reason } => {
            assert_eq!(endpoint.port, 1883);
            assert_eq!(reason, SwitchReason::Fallback);
        }
    }

    harness.runtime.link_up(harness.endpoint.clone()).unwrap();
    harness.drain_packets();
    harness.runtime.feed(connack(false)).unwrap();
    harness.runtime.poll().unwrap();
    let packets = harness.drain_packets();
    let mut failed = None;
    for packet in packets {
        if let Packet::Publish(publish) = packet {
            let stdout = Stdout::decode(&publish.payload).unwrap();
            if let Some(packet_id) = publish.packet_id {
                harness.ack(packet_id);
            }
            if stdout.phase == Phase::Failed {
                failed = Some(stdout);
            }
        }
    }
    let failed = failed.expect("missing jump failure");
    assert_eq!(
        ErrorPayload::decode(&failed.payload).unwrap().code,
        ErrorCode::JumpUnreachable
    );
}

#[test]
fn retained_stdin_is_rejected() {
    let mut harness = Harness::connected();
    let body = TaskBody::new("echo", b"hi".to_vec()).unwrap();
    let stdin = Stdin::new(
        5,
        Kind::Task,
        harness.clock.unix_ms().unwrap(),
        0,
        500,
        body.encode().unwrap(),
    )
    .unwrap();
    harness
        .runtime
        .feed(Packet::Publish(Publish {
            dup: false,
            qos: QoS::AtLeastOnce,
            retain: true,
            topic: Some(harness.in_topic.clone()),
            packet_id: Some(9),
            properties: Properties::new(),
            payload: stdin.encode().unwrap(),
        }))
        .unwrap();
    harness.runtime.poll().unwrap();
    assert!(harness.pump().is_empty());
    assert_eq!(harness.runtime.status().rejected_commands, 1);
}

#[test]
fn jump_connect_timeout_falls_back() {
    let mut harness = Harness::connected();
    let stdin = harness.jump_stdin("peer");
    harness.send(stdin);
    assert_eq!(
        phases(&harness.pump()),
        vec![Phase::Accepted, Phase::Offline]
    );
    harness.runtime.poll().unwrap();
    harness.drain_packets();
    match harness.runtime.take_link_action().expect("missing switch") {
        LinkAction::Switch { endpoint, .. } => assert_eq!(endpoint.port, 1884),
    }

    harness.runtime.link_closed();
    harness.runtime.link_up(harness.peer.clone()).unwrap();
    harness.drain_packets();
    // MQTT CONNECT 超时由 client.poll 报告，运行时应转成回退。
    harness.clock.advance(5_000);
    harness.runtime.poll().unwrap();
    match harness
        .runtime
        .take_link_action()
        .expect("missing fallback")
    {
        LinkAction::Switch { endpoint, reason } => {
            assert_eq!(endpoint.port, 1883);
            assert_eq!(reason, SwitchReason::Fallback);
        }
    }
    assert_eq!(harness.runtime.status().jump, Some(JumpPhase::FallingBack));
}
