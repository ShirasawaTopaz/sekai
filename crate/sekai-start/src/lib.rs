//! 启动层：分发到平台驱动（宿主用 `x86`，硬件用 `esp32`）。
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

#[cfg(all(feature = "x86", feature = "esp32"))]
compile_error!(
    "the `x86` and `esp32` features are mutually exclusive; build the firmware with \
     `--no-default-features --features esp32`"
);

#[cfg(not(any(feature = "x86", feature = "esp32")))]
compile_error!("enable exactly one target feature: `x86` or `esp32`");

mod driver;

/// 启动应用。永不返回。
#[cfg(feature = "x86")]
pub fn start() -> ! {
    driver::x86::start_()
}

/// 启动应用。永不返回。
#[cfg(feature = "esp32")]
pub fn start() -> ! {
    driver::esp32::start_()
}

/// 固件入口点。必须保持私有：`#[esp_hal::main]` 展开成的 `#[entry]` 宏
/// 要求继承可见性，因为它会把该函数以 `main` 符号重新导出。
#[cfg(feature = "esp32")]
#[esp_hal::main]
fn main() -> ! {
    start()
}

/// Panic handler：裸机构建必需；宿主构建使用 `std` 的 panic handler。
#[cfg(feature = "esp32")]
#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    loop {}
}
