//! 硬件时间与时间校准。
//!
//! ESP32 没有可长期供电的 RTC：复位或断电后，硬件计时器从 0 重新开始，
//! 因此只能读到「自启动以来」的时间（[`hardware_time_us`]），无法直接
//! 得到 Unix 时间。Worker在启动、握手或联网时从可信对端（Router、上位机）
//! 拿到一次 Unix 时间后，用 [`calibrate`] 把它与当前硬件时间配成一对
//! 基准点；之后 [`unix_time_us`] / [`unix_time_ms`] 就能在离线状态下推算
//! Unix 时间，供凭证有效期判断、消息时间戳等使用。
//!
//! 宿主构建上，硬件时钟是进程启动后的单调时钟，Unix 时间则直接
//! 来自系统时间：未校准时 [`unix_time_us`] 返回系统时间。这样同一套调用
//! 代码在固件与宿主机上都能工作。
//!
//! # 精度与漂移
//!
//! 校准只修正「偏移」，不修正晶振的频率误差。ESP32 的计时器会随温度
//! 缓慢漂移（典型 ±10 ppm，约每天 ±1 秒），因此校准值只在短期内准确：
//! 每建立一次可信连接、或每隔一段时间就应重新校准一次。
//!
//! ESP32 的计时器在休眠期间不计数，会漏掉休眠时长。若将来使用深度休眠，
//! 需要在每次唤醒后重新校准，或额外累加休眠时间。
//!
//! # 使用约定
//!
//! - 测量时间间隔、实现超时（例如 MQTT Keep Alive）时，用
//!   [`hardware_time_us`] / [`hardware_time_ms`]：它们单调递增，不会因重新
//!   校准而跳变；
//! - 只有需要「墙上时间」（时间戳、凭证有效期）时才用
//!   [`unix_time_us`] / [`unix_time_ms`]；
//! - 重新校准后 Unix 时间可能向前或向后跳变，不能用它的差值计算间隔。
//!
//! # 示例
//!
//! ```rust, no_run
//! use sekai_embedded::time;
//!
//! // 收到可信对端下发的 Unix 时间（毫秒）后校准一次。
//! time::calibrate_unix_ms(1_700_000_000_000);
//!
//! // 之后即可在离线状态下读取 Unix 时间。
//! if let Some(now_ms) = time::unix_time_ms() {
//!     // 用 now_ms 校验凭证有效期、给消息打时间戳……
//!     let _ = now_ms;
//! }
//!
//! // 计时、超时使用单调的硬件时间。
//! let started = time::hardware_time_us();
//! let _elapsed_us = time::hardware_time_us() - started;
//! ```

/// 读取硬件时间：自启动以来经过的微秒数，单调递增。
///
/// ESP32 上来自 `esp_hal::time::Instant`（1 微秒分辨率）；宿主上来自
/// 进程启动后的单调时钟。该值不受 [`calibrate`] 影响，适合测量时间间隔。
pub fn hardware_time_us() -> u64 {
    backend::hardware_time_us()
}

/// 读取硬件时间：自启动以来经过的毫秒数，单调递增。
///
/// 等价于 [`hardware_time_us`] 除以 1000（向下取整）。
pub fn hardware_time_ms() -> u64 {
    hardware_time_us() / 1_000
}

/// 用一次可信的 Unix 时间校准硬件时钟，返回本次校准结果。
///
/// 调用时立即读取当前硬件时间作为基准点，因此 `reference_unix_us`
/// 必须对应当前时刻。若参考时间来自网络，通常会有一个单向传输延迟，
/// 校准后的 Unix 时间会相应偏慢（误差量级为链路延迟）。
///
/// 重复调用会用新的基准点覆盖旧值；校准全局生效。
pub fn calibrate(reference_unix_us: u64) -> Calibration {
    let calibration = Calibration::new(hardware_time_us(), reference_unix_us);
    state::set(Some(calibration));
    calibration
}

/// 用一次可信的 Unix 时间（毫秒）校准硬件时钟，返回本次校准结果。
///
/// 是 [`calibrate`] 的毫秒版本，方便处理以毫秒为单位的时间戳。
pub fn calibrate_unix_ms(reference_unix_ms: u64) -> Calibration {
    calibrate(reference_unix_ms.saturating_mul(1_000))
}

/// 当前生效的校准结果；从未校准过时返回 `None`。
///
/// 宿主未校准时 [`unix_time_us`] 仍会回退到系统时间，因此这里的 `None`
/// 只表示「没有显式校准过」，不表示读不到 Unix 时间。
pub fn calibration() -> Option<Calibration> {
    state::get()
}

/// 校准后的当前 Unix 时间（微秒）。
///
/// ESP32 上未校准过时返回 `None`；宿主上未校准时回退到系统时间。
pub fn unix_time_us() -> Option<u64> {
    match calibration() {
        Some(calibration) => Some(calibration.unix_time_us(hardware_time_us())),
        None => uncalibrated_unix_time_us(),
    }
}

/// 校准后的当前 Unix 时间（毫秒）。
///
/// 与 [`unix_time_us`] 的可用性规则相同；向下取整到毫秒。
pub fn unix_time_ms() -> Option<u64> {
    unix_time_us().map(|unix_us| unix_us / 1_000)
}

/// 一次校准的结果：一对「硬件时间 / Unix 时间」基准点。
///
/// 校准点本身是稳定的锚点，之后任意时刻的 Unix 时间由
/// `unix = unix_anchor + (hardware - hardware_anchor)` 换算得出。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Calibration {
    /// 校准时刻的硬件时间（微秒）。
    hardware_anchor_us: u64,
    /// 与 `hardware_anchor_us` 对应的 Unix 时间（微秒）。
    unix_anchor_us: u64,
}

impl Calibration {
    /// 用一对「硬件时间 / Unix 时间」基准点构造校准结果。
    pub const fn new(hardware_anchor_us: u64, unix_anchor_us: u64) -> Self {
        Self {
            hardware_anchor_us,
            unix_anchor_us,
        }
    }

    /// 校准时刻的硬件时间（微秒）。
    pub const fn hardware_anchor_us(&self) -> u64 {
        self.hardware_anchor_us
    }

    /// 校准时刻的 Unix 时间（微秒）。
    pub const fn unix_anchor_us(&self) -> u64 {
        self.unix_anchor_us
    }

    /// 把硬件时间换算成 Unix 时间（微秒）。
    ///
    /// 结果超出 `u64` 范围时饱和：负值取 0，溢出取 `u64::MAX`。
    pub fn unix_time_us(&self, hardware_us: u64) -> u64 {
        if hardware_us >= self.hardware_anchor_us {
            self.unix_anchor_us
                .saturating_add(hardware_us - self.hardware_anchor_us)
        } else {
            self.unix_anchor_us
                .saturating_sub(self.hardware_anchor_us - hardware_us)
        }
    }

    /// 把硬件时间换算成 Unix 时间（毫秒），向下取整。
    pub fn unix_time_ms(&self, hardware_us: u64) -> u64 {
        self.unix_time_us(hardware_us) / 1_000
    }
}

/// 未校准时的 Unix 时间来源：宿主回退到系统时间，ESP32 无来源。
#[cfg(not(feature = "esp32"))]
fn uncalibrated_unix_time_us() -> Option<u64> {
    Some(backend::system_time_us())
}

/// 未校准时的 Unix 时间来源：宿主回退到系统时间，ESP32 无来源。
#[cfg(feature = "esp32")]
fn uncalibrated_unix_time_us() -> Option<u64> {
    None
}

/// 硬件时间来源。按启用的驱动特性选择与宿主一致的实现。
#[cfg(not(feature = "esp32"))]
mod backend {
    use std::sync::OnceLock;
    use std::time::{Instant, SystemTime, UNIX_EPOCH};

    /// 进程启动时刻，作为宿主上「硬件时间」的原点。
    static START: OnceLock<Instant> = OnceLock::new();

    /// 自进程启动以来经过的微秒数。
    pub(super) fn hardware_time_us() -> u64 {
        let start = START.get_or_init(Instant::now);
        u64::try_from(start.elapsed().as_micros()).unwrap_or(u64::MAX)
    }

    /// 当前 Unix 时间（微秒），宿主上的系统时间。
    ///
    /// 系统时钟早于 Unix 纪元（或不可读）时返回 0。
    pub(super) fn system_time_us() -> u64 {
        let since_epoch = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        u64::try_from(since_epoch.as_micros()).unwrap_or(u64::MAX)
    }
}

/// 硬件时间来源。按启用的驱动特性选择与宿主一致的实现。
#[cfg(feature = "esp32")]
mod backend {
    use esp_hal::time::Instant;

    /// 自芯片启动以来的微秒数。
    ///
    /// `esp_hal::init()` 之前读到的值不可靠，因此只在启动完成后调用。
    pub(super) fn hardware_time_us() -> u64 {
        Instant::now().duration_since_epoch().as_micros()
    }
}

/// 校准值的存放位置。宿主用互斥锁，ESP32 用临界区保护。
#[cfg(not(feature = "esp32"))]
mod state {
    use std::sync::Mutex;

    use super::Calibration;

    static CALIBRATION: Mutex<Option<Calibration>> = Mutex::new(None);

    pub(super) fn get() -> Option<Calibration> {
        // 校准代码不会在持锁时 panic，毒化时仍取出内部值。
        *CALIBRATION
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub(super) fn set(calibration: Option<Calibration>) {
        *CALIBRATION
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = calibration;
    }
}

/// 校准值的存放位置。宿主用互斥锁，ESP32 用临界区保护。
#[cfg(feature = "esp32")]
mod state {
    use core::cell::Cell;

    use critical_section::Mutex;

    use super::Calibration;

    static CALIBRATION: Mutex<Cell<Option<Calibration>>> = Mutex::new(Cell::new(None));

    pub(super) fn get() -> Option<Calibration> {
        critical_section::with(|cs| CALIBRATION.borrow(cs).get())
    }

    pub(super) fn set(calibration: Option<Calibration>) {
        critical_section::with(|cs| CALIBRATION.borrow(cs).set(calibration));
    }
}

#[cfg(all(test, not(feature = "esp32")))]
mod tests {
    use super::*;

    #[test]
    fn calibration_maps_hardware_time_to_unix_time() {
        let calibration = Calibration::new(10_000_000, 1_700_000_000_000_000);

        assert_eq!(calibration.unix_time_us(10_000_000), 1_700_000_000_000_000);
        assert_eq!(calibration.unix_time_us(10_000_500), 1_700_000_000_000_500);
        assert_eq!(calibration.unix_time_us(9_999_000), 1_699_999_999_999_000);
        assert_eq!(calibration.unix_time_us(0), 1_699_999_990_000_000);

        // 毫秒向下取整。
        assert_eq!(calibration.unix_time_ms(10_000_999), 1_700_000_000_000);
        assert_eq!(calibration.unix_time_ms(10_001_000), 1_700_000_000_001);
    }

    #[test]
    fn calibration_saturates_instead_of_underflowing() {
        // 硬件时间早于校准点，且差值大于 Unix 时间：结果为负，饱和到 0。
        let calibration = Calibration::new(1_000, 500);
        assert_eq!(calibration.unix_time_us(0), 0);

        // Unix 时间接近上限时继续前进：饱和到 u64::MAX。
        let calibration = Calibration::new(0, u64::MAX);
        assert_eq!(calibration.unix_time_us(1_000), u64::MAX);
    }

    #[test]
    fn host_clock_uses_system_time_until_calibrated_then_follows_calibration() {
        // 其他测试不修改全局校准值，因此这里可以假设尚未校准。
        assert_eq!(calibration(), None);
        let system_us = backend::system_time_us();
        let uncalibrated = unix_time_us().expect("宿主未校准时回退到系统时间");
        assert!(uncalibrated.abs_diff(system_us) < 1_000_000);

        let reference_ms = 1_700_000_000_000;
        let applied = calibrate_unix_ms(reference_ms);
        assert_eq!(calibration(), Some(applied));

        // 校准后应跟随传入的参考时间，而不是系统时间。
        let calibrated_ms = unix_time_ms().expect("已校准");
        assert!(calibrated_ms >= reference_ms);
        assert!(calibrated_ms - reference_ms < 1_000);

        // 已校准后换算结果与基准点一致。
        assert_eq!(
            applied.unix_time_us(applied.hardware_anchor_us()),
            reference_ms * 1_000
        );
    }
}
