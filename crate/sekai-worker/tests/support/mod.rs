//! 集成测试共享设施：手动时钟 + 内存链路上的 Worker 驱动辅助。

use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use sekai::sekai_clock::Clock;
use sekai::sekai_device_list::DeviceList;
use sekai::sekai_runtime::RuntimeConfig;
use sekai::sekai_task::TaskRegistry;
use sekai::sekai_tool::ToolRegistry;
use sekai_gpio::ChipFamily;
use sekai_worker::capability::{LlmMode, agent_registries, agent_registries_for};
use sekai_worker::host::WorkerHost;
use sekai_worker::link::{Link, MemoryLink, memory_pair};

/// 可共享推进的手动时钟。
#[derive(Clone, Default)]
pub struct ManualClock {
    state: Arc<Mutex<ClockState>>,
}

#[derive(Default)]
struct ClockState {
    monotonic_ms: u64,
    unix_ms: Option<u64>,
}

impl ManualClock {
    /// 单调时间从 `monotonic_ms`、墙上时间从 `unix_ms` 起步。
    pub fn new(monotonic_ms: u64, unix_ms: u64) -> Self {
        Self {
            state: Arc::new(Mutex::new(ClockState {
                monotonic_ms,
                unix_ms: Some(unix_ms),
            })),
        }
    }

    /// 单调时间前进 `ms` 毫秒。
    pub fn advance(&self, ms: u64) {
        let mut state = self.state.lock().unwrap();
        state.monotonic_ms += ms;
        if let Some(unix) = state.unix_ms.as_mut() {
            *unix += ms;
        }
    }
}

impl Clock for ManualClock {
    fn monotonic_ms(&self) -> u64 {
        self.state.lock().unwrap().monotonic_ms
    }

    fn unix_ms(&self) -> Option<u64> {
        self.state.lock().unwrap().unix_ms
    }

    fn calibrate_unix_ms(&self, unix_ms: u64) {
        self.state.lock().unwrap().unix_ms = Some(unix_ms);
    }
}

/// 测试用的固定基准墙上时间（2025-06-01 附近的整毫秒）。
pub const BASE_UNIX_MS: u64 = 1_748_736_000_000;

/// 构造默认能力（PC agent：pc.* 工具 + echo/sleep/pc.job/pc.ask 任务，
/// **无 gpio**，对齐 CLI 默认）的 WorkerHost。
pub fn test_host(worker_id: &str, boot_id: u64, clock: ManualClock) -> WorkerHost<ManualClock> {
    build_host(worker_id, boot_id, clock, false)
}

/// 构造带 gpio opt-in（等价 CLI `--gpio esp32`）的 WorkerHost：
/// agent 能力之上追加 gpio.* 工具。
pub fn test_host_with_gpio(
    worker_id: &str,
    boot_id: u64,
    clock: ManualClock,
) -> WorkerHost<ManualClock> {
    build_host(worker_id, boot_id, clock, true)
}

fn build_host(
    worker_id: &str,
    boot_id: u64,
    clock: ManualClock,
    gpio: bool,
) -> WorkerHost<ManualClock> {
    let (tasks, tools): (TaskRegistry, ToolRegistry) = if gpio {
        agent_registries_for(None, LlmMode::Off, Some(ChipFamily::Esp32)).expect("宿主能力注册失败")
    } else {
        agent_registries(None).expect("宿主能力注册失败")
    };
    let mut config = RuntimeConfig::new(worker_id, boot_id);
    config.firmware = String::from("0.1.0-test");
    WorkerHost::new(config, clock, DeviceList::new(), tasks, tools).expect("WorkerHost 构造失败")
}

/// 内存链路两端 + 挂在 Worker 侧的宿主驱动。
///
/// `router` 端由测试直接读写（扮演 Router）；`pump_*` 系列驱动 Worker 端。
pub struct Loop {
    pub router: MemoryLink,
    worker_link: MemoryLink,
    pub host: WorkerHost<ManualClock>,
    pub clock: ManualClock,
}

impl Loop {
    /// 默认能力（agent，无 gpio）的链路与宿主。
    pub fn new(worker_id: &str, boot_id: u64) -> Self {
        Self::build(worker_id, boot_id, false)
    }

    /// gpio opt-in（`--gpio` 等价）的链路与宿主。
    pub fn new_with_gpio(worker_id: &str, boot_id: u64) -> Self {
        Self::build(worker_id, boot_id, true)
    }

    /// 建立链路并完成 link_up；Hello 首帧由 [`WorkerHost::on_link_up`] 产出，
    /// 直接写入链路供测试从 Router 端读取。
    fn build(worker_id: &str, boot_id: u64, gpio: bool) -> Self {
        let clock = ManualClock::new(1_000, BASE_UNIX_MS);
        let (router, mut worker_link) = memory_pair();
        let mut host = if gpio {
            test_host_with_gpio(worker_id, boot_id, clock.clone())
        } else {
            test_host(worker_id, boot_id, clock.clone())
        };
        let frames = host
            .on_link_up(sekai::sekai_device_list::Endpoint::new("router", 7421).unwrap())
            .expect("link_up 失败");
        assert_eq!(frames.len(), 1, "link_up 应恰好产出一帧 Hello");
        for frame in frames {
            worker_link.write_frame(&frame).expect("写入链路失败");
        }
        Self {
            router,
            worker_link,
            host,
            clock,
        }
    }

    /// Worker 端收一帧（Router→Worker 方向）；没有就轮空。
    fn worker_inbox(&mut self) -> Option<Vec<u8>> {
        match self.worker_link.read_frame() {
            Ok(Some(frame)) => Some(frame),
            Ok(None) => panic!("Worker 链路被对端关闭"),
            Err(sekai_worker::link::LinkError::Timeout) => None,
            Err(error) => panic!("Worker 链路读取失败：{error}"),
        }
    }

    /// 驱动 Worker 一次：处理至多一帧入站 + 一次 poll，把产生的帧写回链路。
    /// 返回是否做了实际工作（收帧或产出帧）。
    pub fn pump_once(&mut self) -> bool {
        let mut worked = false;
        if let Some(frame) = self.worker_inbox() {
            let frames = self.host.on_frame(&frame).expect("Worker 处理帧失败");
            worked = true;
            self.send_to_router(frames);
        }
        let frames = self.host.on_poll().expect("Worker poll 失败");
        worked |= !frames.is_empty();
        self.send_to_router(frames);
        worked
    }

    /// 反复 pump 直到连续 `idle_rounds` 轮无进展（收敛）。
    pub fn pump_until_idle(&mut self, idle_rounds: usize) {
        let mut idle = 0;
        while idle < idle_rounds {
            if self.pump_once() {
                idle = 0;
            } else {
                idle += 1;
            }
        }
    }

    fn send_to_router(&mut self, frames: Vec<Vec<u8>>) {
        for frame in frames {
            self.worker_link.write_frame(&frame).expect("写入链路失败");
        }
    }

    /// Router 端读取一帧；超过 `max_ticks` 次超时即失败。
    ///
    /// 等待期间会小幅推进 [`Loop::clock`]：Task（如 `pc.exec`/`pc.job`）的后续
    /// poll 由运行时的单调时钟驱动，紧循环 pump 若不推进时钟，`Pending` 任务
    /// 永远等不到下一次 poll，终态帧也就不会出现。
    pub fn router_read(&mut self) -> Vec<u8> {
        for _ in 0..100 {
            match self.router.read_frame() {
                Ok(Some(frame)) => return frame,
                Ok(None) => panic!("Router 端链路被关闭"),
                Err(sekai_worker::link::LinkError::Timeout) => {
                    self.pump_once();
                    thread::sleep(Duration::from_millis(2));
                    self.clock.advance(2);
                }
                Err(error) => panic!("Router 端读取失败：{error}"),
            }
        }
        panic!("等待 Router 端帧超时");
    }

    /// Router 端连续读 `count` 帧。
    pub fn router_read_n(&mut self, count: usize) -> Vec<Vec<u8>> {
        (0..count).map(|_| self.router_read()).collect()
    }

    /// Router 端当前不再有帧可读（pump 若干轮后确认）。
    pub fn assert_router_idle(&mut self) {
        self.pump_until_idle(2);
        assert!(
            matches!(
                self.router.read_frame(),
                Err(sekai_worker::link::LinkError::Timeout)
            ),
            "Router 端不应还有未读帧"
        );
    }
}
