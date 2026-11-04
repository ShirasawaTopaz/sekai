//! 运行时端到端集成测试：合成 MQTT 报文驱动 Runtime + 虚拟 GPIO 工具。
//!
//! 复用 crate/sekai/tests/sekai_runtime.rs 的最小合成 harness 思路（测试时钟 +
//! CONNECT/CONNACK + SUBSCRIBE/Hello/SUBACK + Stdin ToolCall），只使用公开 API，
//! 不修改 sekai crate。ESP32、STM32F1、STM32F4 三种虚拟后端各跑一遍 gpio.* 契约，
//! 并验证工具数量上限（6 个内置 + echo + 4 个 GPIO = 11，运行时上限 16）。

use std::cell::Cell;
use std::rc::Rc;

use sekai::mqtt::packet::{Ack, Connack, Packet, Publish, Suback};
use sekai::mqtt::property::Properties;
use sekai::mqtt::qos::QoS;
use sekai::mqtt::reason::ReasonCode;
use sekai::mqtt::topic::TopicName;
use sekai::sekai_clock::Clock;
use sekai::sekai_device_list::{DeviceList, Endpoint};
use sekai::sekai_runtime::{Runtime, RuntimeConfig, RuntimeError};
use sekai::sekai_stdin::{Kind, Stdin, ToolBody};
use sekai::sekai_stdout::{ErrorPayload, Hello, Phase, Stdout};
use sekai::sekai_task::{TaskContext, TaskHandler, TaskPoll, TaskRegistry};
use sekai::sekai_tool::{ToolContext, ToolHandler, ToolRegistry};
use sekai::sekai_wire::{CommandError, ErrorCode};
use sekai_gpio::{ChipFamily, GPIO_TOOL_NAMES, VirtualGpio};

/// 手动时钟：测试里所有时间都由测试推进，不依赖真实等待。
#[derive(Clone)]
struct TestClock {
    mono: Rc<Cell<u64>>,
    unix: Rc<Cell<Option<u64>>>,
}

impl TestClock {
    fn new() -> Self {
        Self {
            mono: Rc::new(Cell::new(0)),
            unix: Rc::new(Cell::new(Some(1_700_000_000_000))),
        }
    }
}

impl Clock for TestClock {
    fn monotonic_ms(&self) -> u64 {
        self.mono.get()
    }

    fn unix_ms(&self) -> Option<u64> {
        self.unix.get()
    }

    fn calibrate_unix_ms(&self, unix_ms: u64) {
        self.unix.set(Some(unix_ms));
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

struct EchoTool;

impl ToolHandler for EchoTool {
    fn call(&mut self, ctx: &mut ToolContext<'_>) -> Result<Vec<u8>, CommandError> {
        Ok(ctx.args.to_vec())
    }
}

/// 与 PC Worker 等价的 demo 任务表：只注册 echo。
fn demo_tasks() -> TaskRegistry {
    let mut tasks = TaskRegistry::new();
    tasks
        .register("echo", |args| {
            Ok(Box::new(EchoTask {
                args: args.to_vec(),
            }))
        })
        .expect("注册 echo 任务");
    tasks
}

/// 与 PC Worker 等价的用户工具表：echo + 4 个 gpio.* + 指定数量的 dummy 工具。
///
/// 工具总数 = 6 个内置工具 + 1 个 echo + 4 个 GPIO = 11；Runtime 要求
/// 用户工具数 + 6 个内置工具不超过 16，即最多再加 5 个 dummy（用户 10 个）。
fn demo_tools(family: ChipFamily, dummy: usize) -> ToolRegistry {
    let mut tools = ToolRegistry::new();
    tools.register("echo", EchoTool).expect("注册 echo 工具");
    sekai_gpio::register_tools(&mut tools, VirtualGpio::new(family)).expect("注册 GPIO 工具");
    for index in 0..dummy {
        tools
            .register(&format!("dummy{index}"), EchoTool)
            .expect("注册 dummy 工具");
    }
    tools
}

fn runtime_with_clock(
    clock: TestClock,
    family: ChipFamily,
    dummy: usize,
) -> Result<Runtime<TestClock>, RuntimeError> {
    Runtime::new(
        RuntimeConfig::new("demo", 1),
        clock,
        DeviceList::new(),
        demo_tasks(),
        demo_tools(family, dummy),
    )
}

fn build_runtime(family: ChipFamily, dummy: usize) -> Result<Runtime<TestClock>, RuntimeError> {
    runtime_with_clock(TestClock::new(), family, dummy)
}

/// 合成 MQTT 报文驱动的运行时 harness。
struct Harness {
    runtime: Runtime<TestClock>,
    clock: TestClock,
    endpoint: Endpoint,
    in_topic: TopicName,
    out_topic: TopicName,
    state_topic: TopicName,
    next_cmd_id: u64,
    next_packet_id: u16,
    hello: Option<Hello>,
}

impl Harness {
    fn new(family: ChipFamily) -> Self {
        let clock = TestClock::new();
        let runtime = runtime_with_clock(clock.clone(), family, 0).expect("Runtime::new 应成功");
        Self {
            runtime,
            clock,
            endpoint: Endpoint {
                host: String::from("127.0.0.1"),
                port: 1883,
            },
            in_topic: TopicName::new("sekai/v1/dev/demo/in").unwrap(),
            out_topic: TopicName::new("sekai/v1/dev/demo/out").unwrap(),
            state_topic: TopicName::new("sekai/v1/dev/demo/state").unwrap(),
            next_cmd_id: 1,
            next_packet_id: 1,
            hello: None,
        }
    }

    fn connected(family: ChipFamily) -> Self {
        let mut harness = Self::new(family);
        harness.handshake();
        harness
    }

    fn hello(&self) -> &Hello {
        self.hello.as_ref().expect("握手后应收到 Hello")
    }

    /// CONNECT -> CONNACK -> SUBSCRIBE/Hello -> SUBACK，保存 Hello 供断言。
    fn handshake(&mut self) {
        self.runtime.link_up(self.endpoint.clone()).unwrap();
        let packets = self.drain_packets();
        assert!(
            packets
                .iter()
                .any(|packet| matches!(packet, Packet::Connect(_))),
            "missing CONNECT"
        );

        self.runtime.feed(connack()).unwrap();
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
        self.hello = Some(hello);

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

    /// 发送一次 ToolCall 并返回该命令的全部 Stdout 事件。
    fn tool_call(&mut self, name: &str, args: &[u8]) -> Vec<Stdout> {
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
        let cmd_id = stdin.id;
        self.send(stdin);
        self.pump()
            .into_iter()
            .filter(|event| event.cmd_id == cmd_id)
            .collect()
    }
}

fn connack() -> Packet {
    Packet::Connack(Connack {
        session_present: false,
        reason_code: ReasonCode::Success,
        properties: Properties::new(),
    })
}

fn phases(events: &[Stdout]) -> Vec<Phase> {
    events.iter().map(|event| event.phase).collect()
}

/// 断言命令成功并返回 Completed 载荷。
fn completed_payload(events: &[Stdout]) -> Vec<u8> {
    assert_eq!(
        phases(events),
        vec![Phase::Accepted, Phase::Completed],
        "unexpected events: {events:?}"
    );
    events.last().expect("至少有一条事件").payload.clone()
}

/// 断言命令成功并返回文本结果。
fn completed_text(events: &[Stdout]) -> String {
    let payload = completed_payload(events);
    String::from_utf8(payload).expect("GPIO 工具结果是 UTF-8 文本")
}

/// 断言命令失败并返回错误载荷。
fn failed_error(events: &[Stdout]) -> ErrorPayload {
    assert_eq!(
        phases(events),
        vec![Phase::Accepted, Phase::Failed],
        "unexpected events: {events:?}"
    );
    ErrorPayload::decode(&events.last().expect("至少有一条事件").payload).unwrap()
}

/// 断言命令失败并返回错误码。
fn failed_code(events: &[Stdout]) -> ErrorCode {
    failed_error(events).code
}

/// 断言 Hello 暴露 4 个 gpio.* 工具与 echo 任务：总工具数 11。
fn assert_gpio_hello(harness: &Harness) {
    let hello = harness.hello();
    assert_eq!(hello.worker_id, "demo");
    assert!(
        hello.tasks.iter().any(|name| name == "echo"),
        "Hello.tasks 应保留 echo 任务：{:?}",
        hello.tasks
    );
    // 6 个内置工具 + echo + 4 个 GPIO = 11。
    assert_eq!(
        hello.tools.len(),
        6 + 1 + GPIO_TOOL_NAMES.len(),
        "Hello.tools = {:?}",
        hello.tools
    );
    assert!(
        hello.tools.iter().any(|name| name == "echo"),
        "Hello.tools 缺少 echo：{:?}",
        hello.tools
    );
    for name in GPIO_TOOL_NAMES {
        assert!(
            hello.tools.iter().any(|tool| tool == name),
            "Hello.tools 缺少 {name}：{:?}",
            hello.tools
        );
    }
}

/// ESP32 虚拟后端：IO 引脚名 + 保留/仅输入/不存在引脚。
#[test]
fn esp32_virtual_gpio_tool_contract() {
    let mut harness = Harness::connected(ChipFamily::Esp32);
    assert_gpio_hello(&harness);

    // gpio.info help：固定自描述文本覆盖四个工具语法。
    let help = completed_text(&harness.tool_call("gpio.info", b"help"));
    for token in ["gpio tools v1", "info", "config", "read", "write"] {
        assert!(help.contains(token), "gpio.info help 缺少 {token}：{help}");
    }

    // 无参数摘要：虚拟后端 + ESP32 引脚表。
    let summary = completed_text(&harness.tool_call("gpio.info", b""));
    for token in [
        "gpio=1",
        "family=esp32",
        "chip=esp32",
        "backend=virtual",
        "pins=34",
        "configured=0",
    ] {
        assert!(
            summary.contains(token),
            "gpio.info 摘要缺少 {token}：{summary}"
        );
    }

    // 单引脚详情（未配置）。
    let detail = completed_text(&harness.tool_call("gpio.info", b"IO4"));
    assert!(detail.contains("pin=IO4"), "{detail}");
    assert!(detail.contains("mode=unconfigured"), "{detail}");

    // 配置为输出并写入。
    let config = completed_text(&harness.tool_call("gpio.config", b"IO4 out level=0"));
    assert!(config.contains("mode=output"), "gpio.config 结果：{config}");
    assert!(config.contains("level=0"), "gpio.config 结果：{config}");
    assert_eq!(
        completed_text(&harness.tool_call("gpio.write", b"IO4 1")),
        "pin=IO4 level=1"
    );
    let detail = completed_text(&harness.tool_call("gpio.info", b"IO4"));
    assert!(detail.contains("mode=output"), "{detail}");
    assert!(detail.contains("level=1"), "{detail}");
    assert!(detail.contains("configured=1"), "{detail}");

    // 输出模式下读取 → InvalidArgument。
    let error = failed_error(&harness.tool_call("gpio.read", b"IO4"));
    assert_eq!(error.code, ErrorCode::InvalidArgument);
    assert!(error.message.contains("IO4"), "{}", error.message);

    // 切回输入 + 上拉；虚拟外部电平默认 0。
    let config = completed_text(&harness.tool_call("gpio.config", b"IO4 in pull=up"));
    assert!(config.contains("mode=input"), "gpio.config 结果：{config}");
    assert!(config.contains("pull=up"), "gpio.config 结果：{config}");
    assert_eq!(
        completed_text(&harness.tool_call("gpio.read", b"IO4")),
        "pin=IO4 level=0"
    );
    // 输入模式下写入 → InvalidArgument。
    assert_eq!(
        failed_code(&harness.tool_call("gpio.write", b"IO4 1")),
        ErrorCode::InvalidArgument
    );

    // 保留引脚（SPI flash）与仅输入引脚（IO34）拒绝输出配置。
    assert_eq!(
        failed_code(&harness.tool_call("gpio.config", b"IO6 out")),
        ErrorCode::InvalidArgument
    );
    assert_eq!(
        failed_code(&harness.tool_call("gpio.config", b"IO34 out")),
        ErrorCode::InvalidArgument
    );
    // 不存在引脚（IO40）。
    assert_eq!(
        failed_code(&harness.tool_call("gpio.info", b"IO40")),
        ErrorCode::InvalidArgument
    );
}

/// STM32F1 虚拟后端：PA 引脚名 + 端口列表 + 表外引脚。
#[test]
fn stm32f1_virtual_gpio_tool_contract() {
    let mut harness = Harness::connected(ChipFamily::Stm32F1);
    assert_gpio_hello(&harness);

    let summary = completed_text(&harness.tool_call("gpio.info", b""));
    for token in [
        "family=stm32",
        "chip=stm32f1",
        "backend=virtual",
        "ports=A,B,C",
        "pins=35",
    ] {
        assert!(
            summary.contains(token),
            "gpio.info 摘要缺少 {token}：{summary}"
        );
    }

    // PA5 可配置、写入、读回。
    let config = completed_text(&harness.tool_call("gpio.config", b"PA5 out level=0"));
    assert!(config.contains("mode=output"), "gpio.config 结果：{config}");
    assert_eq!(
        completed_text(&harness.tool_call("gpio.write", b"PA5 1")),
        "pin=PA5 level=1"
    );
    assert_eq!(
        failed_code(&harness.tool_call("gpio.read", b"PA5")),
        ErrorCode::InvalidArgument
    );
    let config = completed_text(&harness.tool_call("gpio.config", b"PA5 in pull=up"));
    assert!(config.contains("mode=input"), "gpio.config 结果：{config}");
    assert_eq!(
        completed_text(&harness.tool_call("gpio.read", b"PA5")),
        "pin=PA5 level=0"
    );

    // STM32 端口列表：首行 port=A，之后每个表内引脚一行。
    let listing = completed_text(&harness.tool_call("gpio.info", b"port=A"));
    assert!(listing.starts_with("port=A\n"), "{listing}");
    assert!(listing.contains("PA5 caps="), "{listing}");

    // F1 表外引脚 PC0 与保留引脚 PA13（SWD）。
    assert_eq!(
        failed_code(&harness.tool_call("gpio.config", b"PC0 out")),
        ErrorCode::InvalidArgument
    );
    assert_eq!(
        failed_code(&harness.tool_call("gpio.info", b"PC0")),
        ErrorCode::InvalidArgument
    );
    assert_eq!(
        failed_code(&harness.tool_call("gpio.config", b"PA13 out")),
        ErrorCode::InvalidArgument
    );
}

/// STM32F4 虚拟后端：PA/PC/PE 引脚名（与 F1 的差异）。
#[test]
fn stm32f4_virtual_gpio_tool_contract() {
    let mut harness = Harness::connected(ChipFamily::Stm32F4);
    assert_gpio_hello(&harness);

    let summary = completed_text(&harness.tool_call("gpio.info", b""));
    for token in [
        "chip=stm32f4",
        "backend=virtual",
        "ports=A,B,C,D,E",
        "pins=80",
    ] {
        assert!(
            summary.contains(token),
            "gpio.info 摘要缺少 {token}：{summary}"
        );
    }

    // PA5 在 F4 下同样可配置/写入。
    let config = completed_text(&harness.tool_call("gpio.config", b"PA5 out level=0"));
    assert!(config.contains("mode=output"), "gpio.config 结果：{config}");
    assert_eq!(
        completed_text(&harness.tool_call("gpio.write", b"PA5 1")),
        "pin=PA5 level=1"
    );

    // PC0/PE0 在 F4 表内（F1 表外），可正常配置。
    for args in [b"PC0 out".as_slice(), b"PE0 in pull=down".as_slice()] {
        let config = completed_text(&harness.tool_call("gpio.config", args));
        assert!(
            config.contains("mode="),
            "gpio.config {args:?} 结果：{config}"
        );
    }
}

/// 工具数量：Hello 总数 11；Runtime 上限为用户工具 + 6 个内置 <= 16。
#[test]
fn tool_count_fits_runtime_limit() {
    for family in [ChipFamily::Esp32, ChipFamily::Stm32F1, ChipFamily::Stm32F4] {
        let harness = Harness::connected(family);
        assert_eq!(
            harness.hello().tools.len(),
            6 + 1 + GPIO_TOOL_NAMES.len(),
            "{} 家族 Hello.tools 数量不对",
            family.chip()
        );
    }

    // 4 个 GPIO + echo = 5 个用户工具；再加 5 个 dummy 时用户 10 个、总数 16，
    // 仍然合法；加第 6 个 dummy（用户 11 个）才超过上限。
    for dummy in 0..=5 {
        assert!(
            build_runtime(ChipFamily::Esp32, dummy).is_ok(),
            "{dummy} 个 dummy 工具时 Runtime::new 应成功"
        );
    }
    for dummy in 6..=8 {
        match build_runtime(ChipFamily::Esp32, dummy) {
            Err(RuntimeError::InvalidConfig(message)) => {
                assert!(message.contains("16"), "错误消息应提到上限 16：{message}");
            }
            Err(other) => panic!("{dummy} 个 dummy 工具时错误类型不对：{other}"),
            Ok(_) => panic!("{dummy} 个 dummy 工具时 Runtime::new 应失败"),
        }
    }
}
