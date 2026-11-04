//! 运行时时钟抽象。
//!
//! 运行时本身不读时钟、不睡眠；所有时间都通过 [`Clock`] 注入，这样：
//! - 固件与宿主可以各自实现；
//! - 测试可以用手动时钟精确推进，不依赖真实等待。
//!
//! # 两个时间基准
//!
//! - [`Clock::monotonic_ms`]：自启动以来的毫秒数，单调递增，**不得回退**。
//!   调度、DDL、Keep Alive 等超时全部使用它。
//! - [`Clock::unix_ms`]：墙上时间（Unix 毫秒），可能未校准，也可能因
//!   [`Clock::calibrate_unix_ms`] 向前/向后跳变；只用于线上时间戳。
//!
//! 校准只影响 [`Clock::unix_ms`]，不得影响 [`Clock::monotonic_ms`]。

/// 运行时使用的时间源。
pub trait Clock {
    /// 单调递增的毫秒数；用于所有超时/调度计算。
    fn monotonic_ms(&self) -> u64;

    /// 当前 Unix 毫秒；未校准/不可用时返回 `None`。
    fn unix_ms(&self) -> Option<u64>;

    /// 用一次可信的 Unix 毫秒校准墙上时间。
    ///
    /// 实现可以忽略重复校准；运行时负责限制校准频率。
    fn calibrate_unix_ms(&self, unix_ms: u64);
}

impl<T: Clock + ?Sized> Clock for &T {
    fn monotonic_ms(&self) -> u64 {
        (**self).monotonic_ms()
    }

    fn unix_ms(&self) -> Option<u64> {
        (**self).unix_ms()
    }

    fn calibrate_unix_ms(&self, unix_ms: u64) {
        (**self).calibrate_unix_ms(unix_ms)
    }
}

#[cfg(test)]
mod tests {
    use super::Clock;
    use core::cell::Cell;

    struct ManualClock {
        monotonic_ms: Cell<u64>,
        unix_ms: Cell<Option<u64>>,
        calibrations: Cell<u32>,
    }

    impl ManualClock {
        fn new() -> Self {
            Self {
                monotonic_ms: Cell::new(0),
                unix_ms: Cell::new(None),
                calibrations: Cell::new(0),
            }
        }
    }

    impl Clock for ManualClock {
        fn monotonic_ms(&self) -> u64 {
            self.monotonic_ms.get()
        }

        fn unix_ms(&self) -> Option<u64> {
            self.unix_ms.get()
        }

        fn calibrate_unix_ms(&self, unix_ms: u64) {
            self.unix_ms.set(Some(unix_ms));
            self.calibrations.set(self.calibrations.get() + 1);
        }
    }

    #[test]
    fn reference_impl_forwards() {
        let clock = ManualClock::new();
        let reference = &clock;
        assert_eq!(reference.monotonic_ms(), 0);
        assert_eq!(reference.unix_ms(), None);
        reference.calibrate_unix_ms(1_700_000_000_000);
        assert_eq!(reference.unix_ms(), Some(1_700_000_000_000));
        assert_eq!(clock.calibrations.get(), 1);
    }
}
