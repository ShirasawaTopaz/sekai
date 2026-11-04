//! 嵌入式 Worker 启动层（ESP32 固件入口）。
//!
//! 本 crate 只服务嵌入式 Worker：MQTT 传输、硬件时间与真实 GPIO。
//! PC Worker 是独立的 `crate/sekai-worker`（直连 TCP，零 MQTT），与本 crate 互不依赖；
//! 两者共享的协议核心在 `crate/sekai`。
//!
//! 共享模块（宿主无 feature 构建也可编译，供检查与测试）：
//! - [`config`]：Worker/端点配置解析（`no_std`）；
//! - [`clock`]：把硬件时间适配成控制平面 [`sekai::sekai_clock::Clock`]；
//! - [`framing`]：有界 MQTT 字节流分帧；
//! - [`time`]：硬件时间与 Unix 时间校准。
//!
//! 固件构建：`--no-default-features --features esp32`（见 `crate/sekai-build`）。
// 宿主构建（无 feature）只为工作区门禁编译共享模块；它们的真正使用者是
// esp32 固件侧，因此宿主构建放宽 dead_code。
#![cfg_attr(not(feature = "esp32"), allow(dead_code))]
#![cfg_attr(feature = "esp32", no_std)]
#![cfg_attr(feature = "esp32", no_main)]
#![cfg_attr(
    feature = "esp32",
    deny(
        clippy::mem_forget,
        reason = "mem::forget 通常对 esp_hal 类型不安全，尤其是那些 \
        在数据传输期间持有缓冲区的类型。"
    )
)]

extern crate alloc;

pub mod clock;
pub mod config;
mod framing;
pub mod time;

#[cfg(feature = "esp32")]
mod driver;

/// 固件入口点。
///
/// `#[esp_hal::main]` 展开成的 `#[entry]` 宏要求继承可见性，因为它会把该函数
/// 以 `main` 符号重新导出，因此这里保持私有。
#[cfg(feature = "esp32")]
#[esp_hal::main]
async fn main(spawner: embassy_executor::Spawner) {
    driver::esp32::start_(spawner).await
}

/// Panic handler：裸机构建必需；宿主构建使用 `std` 的 panic handler。
#[cfg(feature = "esp32")]
#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    esp_println::println!("[sekai] panic: {info}");
    loop {
        core::hint::spin_loop();
    }
}
