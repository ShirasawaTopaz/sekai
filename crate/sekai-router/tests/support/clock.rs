//! 手动时钟：测试中精确控制单调时间与墙上时间。
//!
//! - `monotonic_ms` 从 0 开始，只由 `advance` 推进；
//! - `unix_ms` 由 `new(unix_base_ms)` / `calibrate_unix_ms` 校准后，
//!   随 `advance` 同步前进，模拟真实墙钟；
//! - 所有句柄共享同一状态（`Clone` 是浅拷贝）。

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use sekai::sekai_clock::Clock;

/// 手动时钟。
#[derive(Clone, Debug, Default)]
pub struct ManualClock {
    inner: Arc<ClockInner>,
}

#[derive(Debug, Default)]
struct ClockInner {
    monotonic_ms: AtomicU64,
    unix_ms: AtomicU64,
    calibrated: AtomicBool,
}

impl ManualClock {
    /// 创建已校准的手动时钟：单调时间从 0 开始，墙上时间为 `unix_base_ms`。
    pub fn new(unix_base_ms: u64) -> Self {
        let clock = Self::default();
        clock.calibrate_unix_ms(unix_base_ms);
        clock
    }

    /// 创建未校准的手动时钟（`unix_ms` 返回 `None`）。
    pub fn uncalibrated() -> Self {
        Self::default()
    }

    /// 推进时间；已校准时墙钟同步前进。
    pub fn advance(&self, ms: u64) {
        self.inner.monotonic_ms.fetch_add(ms, Ordering::SeqCst);
        if self.inner.calibrated.load(Ordering::SeqCst) {
            self.inner.unix_ms.fetch_add(ms, Ordering::SeqCst);
        }
    }

    /// 当前单调毫秒。
    pub fn monotonic(&self) -> u64 {
        self.inner.monotonic_ms.load(Ordering::SeqCst)
    }

    /// 当前墙上毫秒；未校准时返回 `None`。
    pub fn unix(&self) -> Option<u64> {
        if self.inner.calibrated.load(Ordering::SeqCst) {
            Some(self.inner.unix_ms.load(Ordering::SeqCst))
        } else {
            None
        }
    }
}

impl Clock for ManualClock {
    fn monotonic_ms(&self) -> u64 {
        self.monotonic()
    }

    fn unix_ms(&self) -> Option<u64> {
        self.unix()
    }

    fn calibrate_unix_ms(&self, unix_ms: u64) {
        self.inner.unix_ms.store(unix_ms, Ordering::SeqCst);
        self.inner.calibrated.store(true, Ordering::SeqCst);
    }
}
