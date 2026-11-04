//! PC Worker 的宿主能力：系统时钟 + 能力注册（virtual GPIO 工具 + 示范任务）。
//!
//! PC Worker 声明的宿主工具以 `sekai-gpio` 的 virtual 后端为主
//! （`gpio.config` / `gpio.info` / `gpio.read` / `gpio.write`，见
//! `crate/sekai-gpio/src/virtual_gpio.rs`、`tools.rs`；Router 侧的
//! dev-dependencies 同样用它做环回测试）。任务注册表提供 `echo`（立即完成）
//! 与 `sleep`（协作式多周期推进）两个示范任务，覆盖 Task 状态机的
//! Started→Pending→Completed 全路径。

use std::cell::Cell;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use sekai::sekai_clock::Clock;
use sekai::sekai_task::{TaskContext, TaskHandler, TaskPoll, TaskRegistry};
use sekai::sekai_tool::ToolRegistry;
use sekai::sekai_wire::{CommandError, ErrorCode};
use sekai_gpio::{ChipFamily, VirtualGpio, register_tools};

/// 宿主系统时钟：单调时间取 `Instant`，墙上时间取 `SystemTime`。
///
/// [`Clock::calibrate_unix_ms`]（`sekai.set_time` 内置工具触发）通过维护一个
/// 校准偏移生效，且不影响单调时间基准——与 `sekai_clock` 的时间约定一致。
#[derive(Debug, Clone)]
pub struct SystemClock {
    started: Instant,
    offset_ms: Cell<i128>,
}

impl SystemClock {
    /// 以当前时刻创建。
    pub fn new() -> Self {
        Self {
            started: Instant::now(),
            offset_ms: Cell::new(0),
        }
    }

    /// 未校准时的墙上时间（Unix 毫秒）；取不到系统时间时返回 0。
    fn raw_unix_ms(&self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_millis() as u64)
            .unwrap_or(0)
    }
}

impl Default for SystemClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for SystemClock {
    fn monotonic_ms(&self) -> u64 {
        self.started.elapsed().as_millis() as u64
    }

    fn unix_ms(&self) -> Option<u64> {
        let raw = self.raw_unix_ms() as i128 + self.offset_ms.get();
        u64::try_from(raw.max(0)).ok()
    }

    fn calibrate_unix_ms(&self, unix_ms: u64) {
        self.offset_ms
            .set(unix_ms as i128 - self.raw_unix_ms() as i128);
    }
}

/// 构造 PC Worker 的能力注册表：virtual GPIO 工具 + `echo`/`sleep` 任务。
///
/// `family` 决定虚拟 GPIO 的引脚表（`esp32` / `stm32f1` / `stm32f4`）。
pub fn host_registries(family: ChipFamily) -> Result<(TaskRegistry, ToolRegistry), String> {
    let mut tools = ToolRegistry::new();
    register_tools(&mut tools, VirtualGpio::new(family))
        .map_err(|error| format!("注册 gpio 工具失败：{error}"))?;

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
            let text = core::str::from_utf8(args).map_err(|_| {
                CommandError::new(ErrorCode::InvalidArgument, "sleep 参数必须是毫秒文本")
            })?;
            let millis: u64 = text.trim().parse().map_err(|_| {
                CommandError::new(ErrorCode::InvalidArgument, "sleep 参数必须是毫秒文本")
            })?;
            if millis > 3_600_000 {
                return Err(CommandError::new(
                    ErrorCode::InvalidArgument,
                    "sleep 时长不能超过 1 小时",
                ));
            }
            Ok(Box::new(SleepTask {
                until_ms: None,
                duration_ms: millis,
            }) as Box<dyn TaskHandler>)
        })
        .map_err(|error| format!("注册 sleep 任务失败：{error}"))?;
    Ok((tasks, tools))
}

/// `echo` 任务：一个 poll 周期内原样返回参数。
struct EchoTask {
    args: Vec<u8>,
}

impl TaskHandler for EchoTask {
    fn poll(&mut self, ctx: &mut TaskContext<'_>) -> TaskPoll {
        if ctx.is_expired() {
            return TaskPoll::Failed(CommandError::from(ErrorCode::Timeout));
        }
        TaskPoll::Done(self.args.clone())
    }
}

/// `sleep` 任务：协作式等待指定毫秒后完成，验证 Pending→Done 的多周期推进。
struct SleepTask {
    until_ms: Option<u64>,
    duration_ms: u64,
}

impl TaskHandler for SleepTask {
    fn poll(&mut self, ctx: &mut TaskContext<'_>) -> TaskPoll {
        let until = *self
            .until_ms
            .get_or_insert_with(|| ctx.now_mono_ms + self.duration_ms);
        if ctx.now_mono_ms >= until {
            TaskPoll::Done(Vec::new())
        } else {
            TaskPoll::Pending {
                wake_after_ms: (until - ctx.now_mono_ms).min(u32::MAX as u64) as u32,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sekai::sekai_tool::{ToolContext, ToolRegistry};

    fn call_tool(registry: &mut ToolRegistry, name: &str, args: &[u8]) -> Result<Vec<u8>, String> {
        let tool = registry
            .get_mut(name)
            .ok_or_else(|| format!("工具 {name} 未注册"))?;
        let workers = sekai::sekai_device_list::DeviceList::new();
        let mut context = ToolContext {
            args,
            now_mono_ms: 0,
            now_unix_ms: Some(0),
            workers: &workers,
        };
        tool.call(&mut context).map_err(|error| error.to_string())
    }

    #[test]
    fn system_clock_is_monotonic_and_calibratable() {
        let clock = SystemClock::new();
        let first = clock.monotonic_ms();
        assert!(clock.unix_ms().is_some());
        clock.calibrate_unix_ms(1_700_000_000_000);
        let calibrated = clock.unix_ms().unwrap();
        assert!(
            (1_700_000_000_000..=1_700_000_000_120).contains(&calibrated),
            "校准后墙上时间应接近校准值：{calibrated}"
        );
        assert!(clock.monotonic_ms() >= first);
    }

    #[test]
    fn host_registries_register_gpio_and_tasks() {
        let (tasks, mut tools) = host_registries(ChipFamily::Esp32).unwrap();
        assert!(tasks.contains("echo"));
        assert!(tasks.contains("sleep"));
        for name in sekai_gpio::GPIO_TOOL_NAMES {
            assert!(tools.contains(name), "缺少工具 {name}");
        }
        // virtual gpio 真实可用：配置 IO4 为输入后读到低电平。
        call_tool(&mut tools, "gpio.config", b"IO4 in").unwrap();
        let result = call_tool(&mut tools, "gpio.read", b"IO4").unwrap();
        let text = String::from_utf8(result).unwrap();
        assert!(text.contains("pin=IO4"), "{text}");
        assert!(text.contains("level=0"), "{text}");
    }

    #[test]
    fn echo_and_sleep_tasks_advance() {
        let (tasks, _) = host_registries(ChipFamily::Esp32).unwrap();
        let workers = sekai::sekai_device_list::DeviceList::new();

        let factory = tasks.get("echo").unwrap();
        let mut handler = factory(b"payload").unwrap();
        let mut context = TaskContext {
            now_mono_ms: 0,
            deadline_mono_ms: 500,
            now_unix_ms: None,
            workers: &workers,
        };
        match handler.poll(&mut context) {
            TaskPoll::Done(result) => assert_eq!(result, b"payload"),
            other => panic!("echo 应立即完成：{other:?}"),
        }

        let factory = tasks.get("sleep").unwrap();
        let mut handler = factory(b"100").unwrap();
        let mut context = TaskContext {
            now_mono_ms: 0,
            deadline_mono_ms: 500,
            now_unix_ms: None,
            workers: &workers,
        };
        assert!(matches!(
            handler.poll(&mut context),
            TaskPoll::Pending { .. }
        ));
        context.now_mono_ms = 100;
        match handler.poll(&mut context) {
            TaskPoll::Done(result) => assert!(result.is_empty()),
            other => panic!("sleep 到期应完成：{other:?}"),
        }
    }
}
