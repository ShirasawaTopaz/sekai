//! 进程内环回 harness：ControlPlane（服务端）+ MemoryBroker + 一个或多个 WorkerNode。
//!
//! 所有状态机都是真实的（服务端 `ControlPlane`、Worker端 `Runtime`、MQTT `Client`、
//! 虚拟 GPIO），只有传输层被替换成内存报文桥。时间由 `ManualClock` 精确控制，
//! 因此测试完全确定、不访问网络、不依赖 broker。

use sekai::mqtt::packet::{Packet, Publish};
use sekai::sekai_device_list::Endpoint;
use sekai::sekai_runtime::CONTENT_TYPE_STDOUT;
use sekai::sekai_stdout::{Hello, Phase, Stdout};
use sekai_gpio::{ChipFamily, Level, PinId};
use sekai_router::control::{
    CommandRequest, CommandResult, ControlError, ControlEvent, ControlPlane, ControlPlaneConfig,
};
use sekai_router::topic::TopicRoot;

use super::broker::{ClientId, MemoryBroker, RecordedPublish, SERVER_CLIENT};
use super::clock::ManualClock;
use super::worker::{DEFAULT_BOOT_ID, WorkerNode, WorkerSpec};

/// 默认测试基准 Unix 毫秒。
pub const DEFAULT_BASE_UNIX_MS: u64 = 1_790_000_000_000;
/// 默认主题根。
pub const DEFAULT_TOPIC_ROOT: &str = "sekai/v1";
/// 服务端 ControlPlane 使用的 MQTT Client ID（测试固定值）。
pub const SERVER_CLIENT_ID: &str = "sekai-router-test";

/// 环回 harness。
pub struct Harness {
    clock: ManualClock,
    broker: MemoryBroker,
    server: ControlPlane,
    workers: Vec<WorkerNode>,
    events: Vec<ControlEvent>,
    topic_root: TopicRoot,
    endpoint: Endpoint,
    unix_base: u64,
    control_config: ControlPlaneConfig,
    errors: Vec<String>,
}

impl Harness {
    /// 用主题根与 Unix 基准创建空 harness（尚无Worker）。
    pub fn new(topic_root: &str, unix_base: u64) -> Self {
        Self::with_control_config(topic_root, unix_base, |_config| {})
    }

    /// 创建 harness 并允许先修改 ControlPlane 配置。
    pub fn with_control_config(
        topic_root: &str,
        unix_base: u64,
        configure: impl FnOnce(&mut ControlPlaneConfig),
    ) -> Self {
        let clock = ManualClock::new(unix_base);
        let mut broker = MemoryBroker::new();
        let server_client = broker.add_client();
        debug_assert_eq!(server_client, SERVER_CLIENT);
        let topic_root = TopicRoot::new(topic_root).expect("测试主题根必须合法");
        let mut config = ControlPlaneConfig::new(SERVER_CLIENT_ID, topic_root.clone());
        configure(&mut config);
        let server = ControlPlane::new(config.clone()).expect("ControlPlane 构造失败");
        Self {
            clock,
            broker,
            server,
            workers: Vec::new(),
            events: Vec::new(),
            topic_root,
            endpoint: Endpoint::new("127.0.0.1", 1883).expect("端点合法"),
            unix_base,
            control_config: config,
            errors: Vec::new(),
        }
    }

    // ---- 构造 ----

    /// 添加一台Worker（boot_id=1，固件为测试版本），返回Worker下标。
    pub fn add_worker(
        &mut self,
        worker_id: &str,
        family: ChipFamily,
        initial_inputs: &[(PinId, Level)],
    ) -> usize {
        let mut spec = WorkerSpec::new(worker_id, family);
        spec.inputs = initial_inputs.to_vec();
        self.add_worker_spec(spec)
    }

    /// 用完整描述添加Worker。
    pub fn add_worker_spec(&mut self, mut spec: WorkerSpec) -> usize {
        if spec.boot_id == 0 {
            spec.boot_id = DEFAULT_BOOT_ID;
        }
        spec.topic_root = String::from(self.topic_root.as_str());
        let broker_client = self.broker.add_client();
        let node = WorkerNode::new(spec, self.clock.clone(), broker_client)
            .unwrap_or_else(|error| panic!("构建Worker失败：{error}"));
        self.workers.push(node);
        self.workers.len() - 1
    }

    // ---- 时间与访问 ----

    /// 手动时钟。
    pub fn clock(&self) -> &ManualClock {
        &self.clock
    }

    /// 当前 Unix 毫秒。
    pub fn now_unix_ms(&self) -> u64 {
        self.clock.unix().unwrap_or(self.unix_base)
    }

    /// 当前单调毫秒。
    pub fn now_mono_ms(&self) -> u64 {
        self.clock.monotonic()
    }

    /// 主题根。
    pub fn topic_root(&self) -> &TopicRoot {
        &self.topic_root
    }

    /// 测试用 broker 端点。
    pub fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }

    /// ControlPlane 只读访问。
    pub fn server(&self) -> &ControlPlane {
        &self.server
    }

    /// ControlPlane 可变访问。
    pub fn server_mut(&mut self) -> &mut ControlPlane {
        &mut self.server
    }

    /// 创建 harness 时使用的 ControlPlane 配置。
    pub fn control_config(&self) -> &ControlPlaneConfig {
        &self.control_config
    }

    /// 所有Worker。
    pub fn workers(&self) -> &[WorkerNode] {
        &self.workers
    }

    /// 按 id 查找Worker。
    pub fn worker(&self, worker_id: &str) -> Option<&WorkerNode> {
        self.workers
            .iter()
            .find(|worker| worker.worker_id() == worker_id)
    }

    /// 按 id 查找Worker（可变）。
    pub fn worker_mut(&mut self, worker_id: &str) -> Option<&mut WorkerNode> {
        self.workers
            .iter_mut()
            .find(|worker| worker.worker_id() == worker_id)
    }

    /// Worker id 列表。
    pub fn worker_ids(&self) -> Vec<&str> {
        self.workers
            .iter()
            .map(|worker| worker.worker_id())
            .collect()
    }

    /// broker 只读访问。
    pub fn broker(&self) -> &MemoryBroker {
        &self.broker
    }

    /// broker 可变访问。
    pub fn broker_mut(&mut self) -> &mut MemoryBroker {
        &mut self.broker
    }

    /// pump 过程中记录到的错误（正常测试应为空）。
    pub fn errors(&self) -> &[String] {
        &self.errors
    }

    /// 断言 pump 过程中没有出现错误。
    pub fn assert_no_errors(&self) {
        assert!(
            self.errors.is_empty(),
            "harness 记录了错误：{:?}",
            self.errors
        );
    }

    // ---- 事件 ----

    /// 至今累积的 ControlEvent 快照。
    pub fn events(&self) -> Vec<ControlEvent> {
        self.events.clone()
    }

    /// 取走累积的 ControlEvent。
    pub fn take_events(&mut self) -> Vec<ControlEvent> {
        std::mem::take(&mut self.events)
    }

    /// 某个命令的最终结果（如果已结束）。
    pub fn finished(&self, cmd_id: u64) -> Option<CommandResult> {
        self.events.iter().rev().find_map(|event| match event {
            ControlEvent::CommandFinished(result) if result.cmd_id == cmd_id => {
                Some(result.clone())
            }
            _ => None,
        })
    }

    /// 所有已结束命令。
    pub fn command_results(&self) -> Vec<CommandResult> {
        self.events
            .iter()
            .filter_map(|event| match event {
                ControlEvent::CommandFinished(result) => Some(result.clone()),
                _ => None,
            })
            .collect()
    }

    /// 某个命令的 CommandFinished 出现次数（用于断言重复事件）。
    pub fn finished_count(&self, cmd_id: u64) -> usize {
        self.events
            .iter()
            .filter(|event| matches!(event, ControlEvent::CommandFinished(result) if result.cmd_id == cmd_id))
            .count()
    }

    // ---- 发布记录 ----

    /// 所有经过 broker 的 PUBLISH 记录。
    pub fn recorded_publishes(&self) -> &[RecordedPublish] {
        self.broker.publishes()
    }

    /// 兼容别名。
    pub fn publish_packet_log(&self) -> &[RecordedPublish] {
        self.broker.publishes()
    }

    /// 服务端发给Worker的最后一条 `in` 发布。
    pub fn last_stdin_publish(&self, worker_id: &str) -> Option<&RecordedPublish> {
        let topic = self.topic_root.worker_in(worker_id);
        self.broker
            .publishes()
            .iter()
            .rev()
            .find(|record| record.topic == topic)
    }

    /// 某Worker发布到 out/state 的最后一条记录。
    pub fn last_worker_publish(&self, worker_id: &str) -> Option<&RecordedPublish> {
        let client = self.worker(worker_id)?.broker_client();
        self.broker
            .publishes()
            .iter()
            .rev()
            .find(|record| record.from == client)
    }

    // ---- 泵 ----

    /// 执行一轮完整的内存报文交换（出站 → broker → 入站 → poll → 收集事件）。
    pub fn pump(&mut self) {
        self.pump_once();
    }

    fn pump_once(&mut self) -> bool {
        let mut moved = false;
        let now_unix = self.now_unix_ms();
        let now_mono = self.now_mono_ms();

        while let Some(packet) = self.server.next_outgoing(now_unix) {
            moved = true;
            if let Err(error) = self.broker.submit(SERVER_CLIENT, packet) {
                self.errors.push(format!("server -> broker: {error}"));
            }
        }

        let mut worker_packets: Vec<(ClientId, Packet)> = Vec::new();
        for worker in &mut self.workers {
            while let Some(packet) = worker.next_outgoing() {
                worker_packets.push((worker.broker_client(), packet));
            }
        }
        if !worker_packets.is_empty() {
            moved = true;
        }
        for (client, packet) in worker_packets {
            if let Err(error) = self.broker.submit(client, packet) {
                self.errors
                    .push(format!("worker {client} -> broker: {error}"));
            }
        }

        while let Some(packet) = self.broker.take_delivery(SERVER_CLIENT) {
            moved = true;
            if let Err(error) = self.server.feed(packet, now_unix) {
                self.errors.push(format!("broker -> server: {error}"));
            }
        }

        for index in 0..self.workers.len() {
            let client = self.workers[index].broker_client();
            while let Some(packet) = self.broker.take_delivery(client) {
                moved = true;
                if let Err(error) = self.workers[index].feed(packet) {
                    self.errors
                        .push(format!("broker -> worker {client}: {error}"));
                }
            }
        }

        if let Err(error) = self.server.poll(now_unix, now_mono) {
            self.errors.push(format!("server poll: {error}"));
        }
        for index in 0..self.workers.len() {
            if let Err(error) = self.workers[index].poll() {
                self.errors.push(format!(
                    "worker {} poll: {error}",
                    self.workers[index].worker_id()
                ));
            }
        }
        self.drain_events();
        moved
    }

    fn drain_events(&mut self) {
        for event in self.server.take_events() {
            self.events.push(event);
        }
    }

    /// 泵到双方都没有待发报文为止（最多 64 轮，避免测试挂死）。
    pub fn settle(&mut self) {
        for _ in 0..64 {
            let moved = self.pump_once();
            if moved {
                continue;
            }
            if !self.pump_once() {
                return;
            }
        }
    }

    /// 推进手动时钟并泵到静止。
    pub fn advance(&mut self, ms: u64) {
        self.clock.advance(ms);
        self.pump();
        self.settle();
    }

    /// `advance` 的兼容别名。
    pub fn advance_ms(&mut self, ms: u64) {
        self.advance(ms);
    }

    // ---- 连接 ----

    /// 服务端与所有Worker link_up，泵到服务端 ready 且所有Worker在线（含 retained Hello 回放）。
    pub fn connect(&mut self) {
        let now_unix = self.now_unix_ms();
        self.server
            .link_up(self.endpoint.clone(), now_unix)
            .expect("服务端 link_up 失败");
        let endpoint = self.endpoint.clone();
        for worker in &mut self.workers {
            worker.link_up(&endpoint).expect("Worker link_up 失败");
        }
        self.pump_until_ready();
    }

    fn pump_until_ready(&mut self) {
        for _ in 0..2048 {
            self.pump();
            if self.is_ready_and_all_online() {
                self.settle();
                if self.is_ready_and_all_online() {
                    return;
                }
            }
        }
        panic!(
            "harness: 服务器/Worker在限制内未就绪；ready={}, workers={:?}, errors={:?}",
            self.server.is_ready(),
            self.workers
                .iter()
                .map(|worker| {
                    self.server
                        .worker(worker.worker_id())
                        .map(|info| (info.worker_id.clone(), info.online))
                })
                .collect::<Vec<_>>(),
            self.errors
        );
    }

    fn is_ready_and_all_online(&self) -> bool {
        self.server.is_ready()
            && self.workers.iter().all(|worker| {
                self.server
                    .worker(worker.worker_id())
                    .map(|info| info.online)
                    .unwrap_or(false)
            })
    }

    /// 模拟服务端链路断开：broker 侧清空连接，ControlPlane link_lost。
    pub fn server_link_lost(&mut self) {
        let now_unix = self.now_unix_ms();
        let _ = self.broker.reset_client(SERVER_CLIENT);
        self.server.link_lost(now_unix);
        self.settle();
    }

    /// 服务端重新连接（Worker保持在线），等待 retained Hello 重新发现Worker。
    pub fn server_reconnect(&mut self) {
        let now_unix = self.now_unix_ms();
        self.server
            .link_up(self.endpoint.clone(), now_unix)
            .expect("服务端重新 link_up 失败");
        self.pump_until_ready();
    }

    /// 模拟Worker断链并触发遗嘱（retained Offline）。
    pub fn worker_link_lost(&mut self, worker_id: &str) {
        let client = self.worker_client(worker_id);
        let _ = self.broker.disconnect_client(client, true);
        if let Some(worker) = self.worker_mut(worker_id) {
            worker.link_lost();
        }
        self.settle();
    }

    /// Worker重新连接（同一 boot_id）。
    pub fn worker_reconnect(&mut self, worker_id: &str) {
        let endpoint = self.endpoint.clone();
        let _ = self.broker.reset_client(self.worker_client(worker_id));
        if let Some(worker) = self.worker_mut(worker_id) {
            worker.link_up(&endpoint).expect("Worker重新 link_up 失败");
        }
        self.pump_until_worker_online(worker_id, None);
    }

    /// 模拟Worker重启：新 boot_id 重建 Runtime、重新连接并等待新 Hello 被服务端接受。
    pub fn restart_worker(&mut self, worker_id: &str, new_boot_id: u64) {
        let client = self.worker_client(worker_id);
        let _ = self.broker.reset_client(client);
        let endpoint = self.endpoint.clone();
        if let Some(worker) = self.worker_mut(worker_id) {
            worker.restart(new_boot_id).expect("Worker重启失败");
            worker.link_up(&endpoint).expect("重启后 link_up 失败");
        }
        self.pump_until_worker_online(worker_id, Some(new_boot_id));
    }

    fn pump_until_worker_online(&mut self, worker_id: &str, boot_id: Option<u64>) {
        for _ in 0..2048 {
            self.pump();
            let online = self
                .server
                .worker(worker_id)
                .map(|info| info.online && boot_id.map(|boot| info.boot_id == boot).unwrap_or(true))
                .unwrap_or(false);
            if online {
                self.settle();
                return;
            }
        }
        panic!(
            "harness: Worker {worker_id} 未在限制内上线；errors={:?}",
            self.errors
        );
    }

    /// 静默/恢复某Worker的出站应用报文（模拟Worker卡死，用于超时测试）。
    pub fn silence_worker(&mut self, worker_id: &str, silenced: bool) {
        let client = self.worker_client(worker_id);
        self.broker
            .set_silenced(client, silenced)
            .expect("broker 端点存在");
    }

    fn worker_client(&self, worker_id: &str) -> ClientId {
        self.worker(worker_id)
            .unwrap_or_else(|| panic!("未知Worker {worker_id}"))
            .broker_client()
    }

    // ---- 命令 ----

    /// 提交命令并返回 cmd_id；提交失败时 panic。
    pub fn submit(&mut self, request: CommandRequest) -> u64 {
        self.try_submit(request)
            .unwrap_or_else(|error| panic!("提交命令失败：{error}"))
    }

    /// 提交命令，保留 ControlError 供断言“提交前检查”。
    pub fn try_submit(&mut self, request: CommandRequest) -> Result<u64, ControlError> {
        let now_unix = self.now_unix_ms();
        let now_mono = self.now_mono_ms();
        self.server.submit(request, now_unix, now_mono)
    }

    /// 提交命令并推进时钟直到 CommandFinished（或等待预算耗尽时 panic）。
    pub fn run_command(&mut self, request: CommandRequest, timeout_ms: u64) -> CommandResult {
        let cmd_id = self.submit(request);
        self.run_until_command(cmd_id, timeout_ms)
            .unwrap_or_else(|| {
                panic!(
                    "harness: 命令 {cmd_id} 在 {timeout_ms}ms 内没有最终事件；events={}",
                    self.events.len()
                )
            })
    }

    /// 提交命令并尝试等待，找不到返回 None。
    pub fn try_run_command(
        &mut self,
        request: CommandRequest,
        timeout_ms: u64,
    ) -> Option<CommandResult> {
        let cmd_id = self.try_submit(request).ok()?;
        self.run_until_command(cmd_id, timeout_ms)
    }

    /// 推进时钟直到该 cmd_id 出现 CommandFinished，或等待预算耗尽。
    pub fn run_until_command(&mut self, cmd_id: u64, timeout_ms: u64) -> Option<CommandResult> {
        self.settle();
        if let Some(result) = self.finished(cmd_id) {
            return Some(result);
        }
        let mut spent = 0u64;
        while spent < timeout_ms {
            let remaining = timeout_ms - spent;
            let step = if remaining >= 100 { 10 } else { 1 };
            self.advance(step);
            spent += step;
            if let Some(result) = self.finished(cmd_id) {
                return Some(result);
            }
        }
        None
    }

    // ---- 注入（模拟Worker侧帧 / broker 重放） ----

    /// 以某Worker身份注入一条 PUBLISH，返回 broker 记录索引。
    pub fn inject_publish(
        &mut self,
        worker_id: &str,
        topic: String,
        payload: Vec<u8>,
        retain: bool,
        content_type: Option<&str>,
    ) -> usize {
        let client = self.worker_client(worker_id);
        self.broker
            .inject_publish(client, &topic, payload, retain, content_type)
            .expect("broker 注入失败")
    }

    /// 以某Worker身份注入一条 Stdout 帧（按 phase 选择 out/state 主题）。
    pub fn inject_stdout(
        &mut self,
        worker_id: &str,
        stdout: &Stdout,
        retain: bool,
        content_type: Option<&str>,
    ) -> usize {
        let topic = match stdout.phase {
            Phase::Hello | Phase::Offline => self.topic_root.worker_state(worker_id),
            _ => self.topic_root.worker_out(worker_id),
        };
        let payload = stdout.encode().expect("Stdout 编码失败");
        self.inject_publish(worker_id, topic, payload, retain, content_type)
    }

    /// 注入一条自定义 Hello（可用于测试能力更新、Worker id 不匹配等）。
    pub fn inject_hello(
        &mut self,
        worker_id: &str,
        hello: &Hello,
        boot_id: u64,
        seq: u32,
        retain: bool,
    ) -> Option<usize> {
        let payload = hello.encode().ok()?;
        let frame = Stdout::new(boot_id, 0, seq, Phase::Hello, self.now_unix_ms(), payload).ok()?;
        Some(self.inject_stdout(worker_id, &frame, retain, Some(CONTENT_TYPE_STDOUT)))
    }

    /// 以Worker身份发布 retained Offline（LWT 语义），返回记录索引。
    pub fn publish_retained_offline(&mut self, worker_id: &str) -> usize {
        let boot_id = self.worker(worker_id).expect("未知Worker").boot_id();
        let frame = Stdout::new(
            boot_id,
            0,
            0,
            Phase::Offline,
            self.now_unix_ms(),
            Vec::new(),
        )
        .expect("Offline 帧合法");
        self.inject_stdout(worker_id, &frame, true, Some(CONTENT_TYPE_STDOUT))
    }

    /// 重放一条历史 PUBLISH 记录（模拟 QoS1 重复投递），返回新记录索引。
    pub fn replay_publish(&mut self, index: usize) -> usize {
        self.broker.replay_publish(index).expect("重放失败")
    }

    /// 构造一条与已有记录同载荷的新 Publish（测试辅助）。
    pub fn republish_record(&mut self, index: usize) -> usize {
        self.replay_publish(index)
    }
}

/// 便捷：把 Stdout 载荷解码成 `Stdout`。
pub fn decode_stdout(publish: &Publish) -> Option<Stdout> {
    Stdout::decode(&publish.payload).ok()
}
