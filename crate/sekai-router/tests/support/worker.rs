//! Worker节点：真实 `sekai::sekai_runtime::Runtime` + `sekai_gpio::VirtualGpio`。
//!
//! Worker端不使用任何自定义协议实现：工具是真实注册的 `gpio.*`，任务是真实的
//! `TaskRegistry` 处理器（`echo` / `sleep`），MQTT 状态机是真实的 `Client`。
//! 这样 ControlPlane 的测试可以覆盖与真实Worker一致的字节与事件序列。

use sekai::mqtt::packet::Packet;
use sekai::sekai_device_list::{DeviceList, Endpoint};
use sekai::sekai_runtime::{Runtime, RuntimeConfig, RuntimeStatus};
use sekai::sekai_task::{TaskContext, TaskHandler, TaskPoll, TaskRegistry};
use sekai::sekai_tool::ToolRegistry;
use sekai::sekai_wire::{CommandError, ErrorCode};
use sekai_gpio::{ChipFamily, Level, PinId, VirtualGpio, register_tools};

use super::broker::ClientId;
use super::clock::ManualClock;

/// 默认固件版本。
pub const DEFAULT_FIRMWARE: &str = "0.1.0-test";
/// 默认 boot_id。
pub const DEFAULT_BOOT_ID: u64 = 1;

/// Worker构造描述。
#[derive(Debug, Clone)]
pub struct WorkerSpec {
    /// Worker id。
    pub worker_id: String,
    /// 本次启动周期 id。
    pub boot_id: u64,
    /// GPIO 家族（决定引脚表）。
    pub family: ChipFamily,
    /// 固件版本字符串。
    pub firmware: String,
    /// 主题根。
    pub topic_root: String,
    /// 初始模拟输入电平。
    pub inputs: Vec<(PinId, Level)>,
}

impl WorkerSpec {
    /// 按家族创建描述（boot_id 默认 1，固件版本为测试版本）。
    pub fn new(worker_id: impl Into<String>, family: ChipFamily) -> Self {
        Self {
            worker_id: worker_id.into(),
            boot_id: DEFAULT_BOOT_ID,
            family,
            firmware: String::from(DEFAULT_FIRMWARE),
            topic_root: String::from("sekai/v1"),
            inputs: Vec::new(),
        }
    }

    /// 设置 boot_id。
    pub fn with_boot_id(mut self, boot_id: u64) -> Self {
        self.boot_id = boot_id;
        self
    }

    /// 设置固件版本。
    pub fn with_firmware(mut self, firmware: impl Into<String>) -> Self {
        self.firmware = firmware.into();
        self
    }

    /// 追加上电时的模拟输入电平。
    pub fn with_input(mut self, pin: PinId, level: Level) -> Self {
        self.inputs.push((pin, level));
        self
    }
}

/// 一个内存Worker：真实 Runtime + 虚拟 GPIO。
pub struct WorkerNode {
    spec: WorkerSpec,
    clock: ManualClock,
    broker_client: ClientId,
    runtime: Runtime<ManualClock>,
}

impl WorkerNode {
    /// 构造Worker并建立 Runtime（尚未 link_up）。
    pub fn new(
        spec: WorkerSpec,
        clock: ManualClock,
        broker_client: ClientId,
    ) -> Result<Self, String> {
        let runtime = build_runtime(&spec, clock.clone())?;
        Ok(Self {
            spec,
            clock,
            broker_client,
            runtime,
        })
    }

    /// Worker id。
    pub fn worker_id(&self) -> &str {
        &self.spec.worker_id
    }

    /// 当前 boot_id。
    pub fn boot_id(&self) -> u64 {
        self.spec.boot_id
    }

    /// Worker描述。
    pub fn spec(&self) -> &WorkerSpec {
        &self.spec
    }

    /// broker 端点 id。
    pub fn broker_client(&self) -> ClientId {
        self.broker_client
    }

    /// Runtime 只读访问。
    pub fn runtime(&self) -> &Runtime<ManualClock> {
        &self.runtime
    }

    /// Runtime 可变访问。
    pub fn runtime_mut(&mut self) -> &mut Runtime<ManualClock> {
        &mut self.runtime
    }

    /// 运行时状态快照。
    pub fn status(&self) -> RuntimeStatus {
        self.runtime.status()
    }

    /// 发起 MQTT 连接（CONNECT 入队）。
    pub fn link_up(&mut self, endpoint: &Endpoint) -> Result<(), String> {
        self.runtime
            .link_up(endpoint.clone())
            .map_err(|error| error.to_string())
    }

    /// 非预期断链。
    pub fn link_lost(&mut self) {
        self.runtime.link_lost();
    }

    /// 正常关闭。
    pub fn link_closed(&mut self) {
        self.runtime.link_closed();
    }

    /// 喂入一条 MQTT 报文。
    pub fn feed(&mut self, packet: Packet) -> Result<(), String> {
        self.runtime.feed(packet).map_err(|error| error.to_string())
    }

    /// 推进运行时。
    pub fn poll(&mut self) -> Result<(), String> {
        self.runtime.poll().map_err(|error| error.to_string())
    }

    /// 取出下一条待发送报文。
    pub fn next_outgoing(&mut self) -> Option<Packet> {
        self.runtime.next_outgoing()
    }

    /// 下一次需要 poll 的单调毫秒。
    pub fn next_deadline(&self) -> Option<u64> {
        self.runtime.next_deadline()
    }

    /// 模拟Worker重启：用新 boot_id 重建 Runtime（GPIO 状态与在途命令全部丢弃）。
    pub fn restart(&mut self, new_boot_id: u64) -> Result<(), String> {
        if new_boot_id == 0 {
            return Err(String::from("boot_id 不能为 0"));
        }
        self.spec.boot_id = new_boot_id;
        let runtime = build_runtime(&self.spec, self.clock.clone())?;
        self.runtime = runtime;
        Ok(())
    }
}

fn build_runtime(spec: &WorkerSpec, clock: ManualClock) -> Result<Runtime<ManualClock>, String> {
    let mut gpio = VirtualGpio::new(spec.family);
    for (pin, level) in &spec.inputs {
        gpio.set_external_level(*pin, *level)
            .map_err(|error| format!("设置初始输入电平失败：{}", error.message()))?;
    }
    let mut tools = ToolRegistry::new();
    register_tools(&mut tools, gpio).map_err(|error| format!("注册 gpio 工具失败：{error}"))?;

    let mut tasks = TaskRegistry::new();
    tasks
        .register("echo", |args: &[u8]| {
            Ok(Box::new(EchoTask {
                args: args.to_vec(),
            }) as Box<dyn TaskHandler>)
        })
        .map_err(|error| format!("注册 echo 任务失败：{error}"))?;
    tasks
        .register("sleep", |args: &[u8]| {
            let text = std::str::from_utf8(args).map_err(|_| {
                CommandError::new(ErrorCode::InvalidArgument, "sleep 参数必须是毫秒文本")
            })?;
            let ms = text.trim().parse::<u64>().map_err(|_| {
                CommandError::new(ErrorCode::InvalidArgument, "sleep 参数必须是毫秒文本")
            })?;
            Ok(Box::new(SleepTask {
                remaining_ms: ms,
                started_at: None,
            }) as Box<dyn TaskHandler>)
        })
        .map_err(|error| format!("注册 sleep 任务失败：{error}"))?;

    let mut config = RuntimeConfig::new(spec.worker_id.clone(), spec.boot_id);
    config.topic_root = spec.topic_root.clone();
    config.firmware = spec.firmware.clone();
    Runtime::new(config, clock, DeviceList::new(), tasks, tools)
        .map_err(|error| format!("构造 Runtime 失败：{error}"))
}

struct EchoTask {
    args: Vec<u8>,
}

impl TaskHandler for EchoTask {
    fn poll(&mut self, _ctx: &mut TaskContext<'_>) -> TaskPoll {
        TaskPoll::Done(self.args.clone())
    }
}

struct SleepTask {
    remaining_ms: u64,
    started_at: Option<u64>,
}

impl TaskHandler for SleepTask {
    fn poll(&mut self, ctx: &mut TaskContext<'_>) -> TaskPoll {
        let started_at = match self.started_at {
            Some(value) => value,
            None => {
                self.started_at = Some(ctx.now_mono_ms);
                ctx.now_mono_ms
            }
        };
        let deadline = started_at.saturating_add(self.remaining_ms);
        if ctx.now_mono_ms >= deadline {
            TaskPoll::Done(Vec::new())
        } else {
            let wake_after_ms = (deadline - ctx.now_mono_ms).min(10) as u32;
            TaskPoll::Pending { wake_after_ms }
        }
    }
}
