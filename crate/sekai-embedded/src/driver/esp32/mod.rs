//! ESP32 classic 驱动：真实 GPIO + WiFi/DHCP + MQTT 主循环。
//!
//! 启动顺序（见 docs/sekai-gpio-plan.md 第 8.2 节）：
//! 1. 初始化堆分配器（reclaimed + 普通两块）；
//! 2. esp_hal::init 提高主频并取得外设；
//! 3. esp_rtos::start 启动调度器（WiFi 初始化之前必须完成）；
//! 4. 校验编译期环境变量（SSID / MQTT 地址），非法时打印原因并空转；
//! 5. 初始化 WiFi Controller + station 接口，连接 AP；
//! 6. 创建 embassy-net 协议栈（DHCPv4）并 spawn runner；
//! 7. 注册 GPIO 工具、构造 Runtime，交给 net 的 MQTT 主循环。

mod net;

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;

use embassy_executor::Spawner;
use embassy_time::{Duration, Timer, with_timeout};
use esp_hal::clock::CpuClock;
use esp_hal::ram;
use esp_hal::rng::Rng;
use esp_hal::timer::timg::TimerGroup;
use esp_println::println;
use esp_radio::wifi::{
    AuthenticationMethodConfig, Config as WifiConfig, Interface, Password, Ssid, WifiController,
    sta::StationConfig,
};
use sekai::sekai_device_list::{DeviceList, Endpoint};
use sekai::sekai_runtime::{Runtime, RuntimeConfig};
use sekai::sekai_task::TaskRegistry;
use sekai::sekai_tool::ToolRegistry;

use crate::clock::SystemClock;
use crate::config::{parse_ipv4_endpoint, parse_workers_str};

// 创建 esp-idf bootloader 所需的默认 app descriptor。
// 更多信息参见：<https://docs.espressif.com/projects/esp-idf/en/stable/esp32/api-reference/system/app_image_format.html#application-description>
esp_bootloader_esp_idf::esp_app_desc!();

/// 编译期配置：WiFi SSID（必填）。
const WIFI_SSID: &str = match option_env!("SEKAI_WIFI_SSID") {
    Some(value) => value,
    None => "",
};
/// 编译期配置：WiFi 密码；空 = Open，非空 = WPA2 Personal。
const WIFI_PASSWORD: &str = match option_env!("SEKAI_WIFI_PASSWORD") {
    Some(value) => value,
    None => "",
};
/// 编译期配置：MQTT broker 的 IPv4:port（必填，本轮不做 DNS）。
const MQTT_ADDR: &str = match option_env!("SEKAI_MQTT_ADDR") {
    Some(value) => value,
    None => "",
};
/// 编译期配置：MQTT 用户名（可选）。
const MQTT_USERNAME: &str = match option_env!("SEKAI_MQTT_USERNAME") {
    Some(value) => value,
    None => "",
};
/// 编译期配置：MQTT 密码（可选）。
const MQTT_PASSWORD: &str = match option_env!("SEKAI_MQTT_PASSWORD") {
    Some(value) => value,
    None => "",
};
/// 编译期配置：Worker id（可选，默认 sekai-esp32）。
const DEVICE_ID: &str = match option_env!("SEKAI_DEVICE_ID") {
    Some(value) => value,
    None => "",
};
/// 编译期配置：主题根（可选，默认 sekai/v1）。
const TOPIC_ROOT: &str = match option_env!("SEKAI_TOPIC_ROOT") {
    Some(value) => value,
    None => "",
};
/// 编译期配置：Jump 目标列表（可选，name=IPv4:port,...）。
const DEVICES: &str = match option_env!("SEKAI_DEVICES") {
    Some(value) => value,
    None => "",
};

/// 默认Worker id。
const DEFAULT_DEVICE_ID: &str = "sekai-esp32";
/// 默认主题根。
const DEFAULT_TOPIC_ROOT: &str = "sekai/v1";
/// Hello 中上报的固件标识。
const FIRMWARE: &str = concat!("sekai-esp32/", env!("CARGO_PKG_VERSION"));

/// 固件入口。由 crate::main 在 Embassy 执行器上调用，永不返回。
pub async fn start_(spawner: Spawner) -> ! {
    esp_alloc::heap_allocator!(#[ram(reclaimed)] size: 96 * 1024);
    esp_alloc::heap_allocator!(size: 48 * 1024);

    let config = esp_hal::Config::default().with_cpu_clock(CpuClock::max());
    let peripherals = esp_hal::init(config);

    let timg0 = TimerGroup::new(peripherals.TIMG0);
    // WiFi 初始化之前必须启动调度器。
    esp_rtos::start(timg0.timer0, peripherals.FROM_CPU_INTR0);

    let settings = match Settings::from_env() {
        Ok(settings) => settings,
        Err(error) => {
            println!("[sekai] 配置错误：{error}");
            println!("[sekai] 请通过编译期环境变量提供配置后重新构建固件");
            idle_forever().await
        }
    };

    let rng = Rng::new();
    let boot_id = ((u64::from(rng.random()) << 32) | u64::from(rng.random())).max(1);

    let mut wifi = match WifiController::new(peripherals.WIFI, Default::default()) {
        Ok(wifi) => wifi,
        Err(error) => {
            println!("[sekai] WiFi 控制器初始化失败：{error}");
            idle_forever().await
        }
    };
    let sta = Interface::station();
    let station_config = WifiConfig::Station(
        StationConfig::default()
            .with_ssid(settings.ssid)
            .with_authentication(settings.authentication),
    );
    if let Err(error) = wifi.set_config(&station_config) {
        println!("[sekai] WiFi 配置失败：{error}");
        idle_forever().await
    }

    println!("[sekai] 正在连接 WiFi：SSID={}", settings.ssid.as_str());
    match with_timeout(Duration::from_secs(20), wifi.connect_async()).await {
        Ok(Ok(_info)) => println!("[sekai] WiFi 已连接"),
        Ok(Err(error)) => println!("[sekai] WiFi 连接失败：{error}"),
        Err(_) => println!("[sekai] WiFi 连接超时"),
    }

    let resources = Box::leak(Box::new(embassy_net::StackResources::<4>::new()));
    let (stack, runner) = embassy_net::new(
        sta,
        embassy_net::Config::dhcpv4(Default::default()),
        resources,
        u64::from(rng.random()),
    );
    let net_task = match net::net_task(runner) {
        Ok(task) => task,
        Err(error) => {
            println!("[sekai] 网络任务创建失败：{error:?}");
            idle_forever().await
        }
    };
    spawner.spawn(net_task);
    match with_timeout(Duration::from_secs(15), stack.wait_config_up()).await {
        Ok(()) => match stack.config_v4() {
            Some(config) => println!("[sekai] DHCP 已就绪：{}", config.address),
            None => println!("[sekai] DHCP 已就绪"),
        },
        Err(_) => println!("[sekai] DHCP 超时，稍后由 MQTT 主循环重试"),
    }

    let mut tools = ToolRegistry::new();
    if let Err(error) = sekai_gpio::register_tools(&mut tools, sekai_gpio::esp32::Esp32Gpio::new())
    {
        println!("[sekai] 注册 GPIO 工具失败：{error:?}");
        idle_forever().await
    }

    let mut runtime_config = RuntimeConfig::new(settings.worker_id.clone(), boot_id);
    runtime_config.firmware = String::from(FIRMWARE);
    runtime_config.topic_root = settings.topic_root;
    runtime_config.mqtt.username = settings.mqtt_username;
    runtime_config.mqtt.password = settings.mqtt_password;
    runtime_config.will_offline = true;
    let runtime = match Runtime::new(
        runtime_config,
        SystemClock,
        settings.workers,
        TaskRegistry::new(),
        tools,
    ) {
        Ok(runtime) => runtime,
        Err(error) => {
            println!("[sekai] 运行时初始化失败：{error}");
            idle_forever().await
        }
    };

    println!("[sekai] Worker {} 启动", settings.worker_id);
    net::run_mqtt(runtime, stack, wifi, settings.mqtt_endpoint).await
}

/// 启动失败时的空转：不 panic（panic handler 只打印 + loop），保留串口输出能力。
async fn idle_forever() -> ! {
    loop {
        Timer::after(Duration::from_secs(1)).await;
    }
}

/// 校验并保存编译期配置。
struct Settings {
    ssid: Ssid,
    authentication: AuthenticationMethodConfig,
    mqtt_endpoint: Endpoint,
    mqtt_username: Option<String>,
    mqtt_password: Option<Vec<u8>>,
    worker_id: String,
    topic_root: String,
    workers: DeviceList,
}

impl Settings {
    /// 读取 option_env! 常量并校验；任何必填项缺失/非法都返回可读错误。
    fn from_env() -> Result<Self, String> {
        if WIFI_SSID.is_empty() {
            return Err(String::from("SEKAI_WIFI_SSID 未设置（必填，2.4GHz SSID）"));
        }
        let ssid = Ssid::try_from(WIFI_SSID)
            .map_err(|error| alloc::format!("SEKAI_WIFI_SSID 非法：{error}"))?;
        let authentication = if WIFI_PASSWORD.is_empty() {
            AuthenticationMethodConfig::Open
        } else {
            let password = Password::try_from(WIFI_PASSWORD)
                .map_err(|error| alloc::format!("SEKAI_WIFI_PASSWORD 非法：{error}"))?;
            AuthenticationMethodConfig::Wpa2Personal(password)
        };

        if MQTT_ADDR.is_empty() {
            return Err(String::from(
                "SEKAI_MQTT_ADDR 未设置（必填，IPv4:port，例如 192.168.1.10:1883）",
            ));
        }
        let mqtt_endpoint = parse_ipv4_endpoint(MQTT_ADDR)
            .map_err(|error| alloc::format!("SEKAI_MQTT_ADDR 非法：{error}"))?;

        let mqtt_username = non_empty(MQTT_USERNAME).map(String::from);
        let mqtt_password = non_empty(MQTT_PASSWORD).map(|value| value.as_bytes().to_vec());

        let worker_id = non_empty(DEVICE_ID).unwrap_or(DEFAULT_DEVICE_ID);
        let topic_root = non_empty(TOPIC_ROOT).unwrap_or(DEFAULT_TOPIC_ROOT);

        let workers = match parse_workers_str(DEVICES) {
            Ok(workers) => workers,
            Err(error) => {
                println!("[sekai] SEKAI_DEVICES 非法：{error}");
                DeviceList::new()
            }
        };

        Ok(Self {
            ssid,
            authentication,
            mqtt_endpoint,
            mqtt_username,
            mqtt_password,
            worker_id: String::from(worker_id),
            topic_root: String::from(topic_root),
            workers,
        })
    }
}

/// 空字符串（含纯空白）视为未设置。
fn non_empty(value: &'static str) -> Option<&'static str> {
    let value = value.trim();
    if value.is_empty() { None } else { Some(value) }
}
