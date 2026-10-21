use esp_hal::clock::CpuClock;
use esp_hal::time::{Duration, Instant};

// 这里创建 esp-idf bootloader 所需的默认 app descriptor。
// 更多信息参见：<https://docs.espressif.com/projects/esp-idf/en/stable/esp32/api-reference/system/app_image_format.html#application-description>
esp_bootloader_esp_idf::esp_app_desc!();

/// 硬件初始化。真正的入口点（`main`）由 `crate::start` 上的
/// `#[esp_hal::main]` 生成。
pub fn start_() -> ! {
    // 生成器版本：1.0.1

    let config = esp_hal::Config::default().with_cpu_clock(CpuClock::max());
    let _peripherals = esp_hal::init(config);

    loop {
        let delay_start = Instant::now();
        while delay_start.elapsed() < Duration::from_millis(500) {}
    }

    // 想找些灵感，可以参考这里的示例：https://github.com/esp-rs/esp-hal/tree/esp-hal-v1.0.0/examples/src/bin
}
