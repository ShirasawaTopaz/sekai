//! 宿主时间源：把 `sekai_embedded::time` 适配成控制平面 [`Clock`]。

use sekai::sekai_clock::Clock;

use crate::time;

/// 基于硬件/系统时间的时钟。
///
/// 宿主上 `unix_ms` 默认来自系统时间，校准后跟随校准值；
/// 单调时间始终来自进程启动后的计时器。
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn monotonic_ms(&self) -> u64 {
        time::hardware_time_ms()
    }

    fn unix_ms(&self) -> Option<u64> {
        time::unix_time_ms()
    }

    fn calibrate_unix_ms(&self, unix_ms: u64) {
        let _ = time::calibrate_unix_ms(unix_ms);
    }
}
