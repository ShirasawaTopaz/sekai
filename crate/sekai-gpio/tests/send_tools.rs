//! `send` 特性下 `gpio.*` 工具的注册与调用测试。
//!
//! `send` 构建下工具处理器必须可跨线程移动（`ToolRegistry::register` 要求
//! `Send`），共享句柄也从 `Rc<RefCell<P>>` 切换为
//! `Arc<critical_section::Mutex<RefCell<P>>>`。本文件验证：
//!
//! - 注册表与虚拟后端满足 `Send`；
//! - 四个工具在 `send` 构建下注册并正常调用；
//! - 处理器可以移动到另一个线程后继续调用（共享 `Arc` 句柄保持状态）；
//! - 重复注册仍返回 `RegistryError::Duplicate`。

#![cfg(feature = "send")]

use sekai::sekai_device_list::DeviceList;
use sekai::sekai_tool::{BoxedTool, RegistryError, ToolContext, ToolHandler, ToolRegistry};
use sekai::sekai_wire::CommandError;
use sekai_gpio::virtual_gpio::VirtualGpio;
use sekai_gpio::{ChipFamily, register_tools};

/// 计划 §5.3 的 `gpio.info help` 固定文本（逐字，末尾无换行）。
const EXPECTED_HELP: &str = "gpio tools v1\n\
info [help|PIN|port=A]\n\
config PIN [in|out] [level=0|1] [pull=none|up|down] [open_drain=0|1]\n\
read PIN\n\
write PIN 0|1\n\
pins: IO4 / PA5\n\
read requires input mode; write requires output mode";

/// 编译期 `Send` 断言。
fn assert_send<T: Send>() {}

/// 创建 ESP32 虚拟后端并注册四个 GPIO 工具。
fn registry() -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    register_tools(&mut registry, VirtualGpio::new(ChipFamily::Esp32)).expect("注册 GPIO 工具");
    registry
}

/// 调用工具并返回 UTF-8 结果文本。
fn call(registry: &mut ToolRegistry, name: &str, args: &str) -> Result<String, CommandError> {
    let workers = DeviceList::new();
    let mut ctx = ToolContext {
        args: args.as_bytes(),
        now_mono_ms: 0,
        now_unix_ms: None,
        workers: &workers,
    };
    let tool = registry
        .get_mut(name)
        .unwrap_or_else(|| panic!("工具 {name} 未注册"));
    let bytes = tool.call(&mut ctx)?;
    Ok(String::from_utf8(bytes).expect("工具结果必须是 UTF-8"))
}

/// 仅用于重复注册测试的占位工具。
struct DummyTool;

impl ToolHandler for DummyTool {
    fn call(&mut self, _ctx: &mut ToolContext<'_>) -> Result<Vec<u8>, CommandError> {
        Ok(Vec::new())
    }
}

#[test]
fn registry_port_and_boxed_tool_are_send() {
    assert_send::<ToolRegistry>();
    assert_send::<VirtualGpio>();
    assert_send::<BoxedTool>();
}

#[test]
fn registers_and_calls_all_four_tools() {
    let mut registry = registry();
    assert_eq!(registry.len(), 4);
    let names: Vec<&str> = registry.names().collect();
    assert_eq!(
        names,
        vec!["gpio.config", "gpio.info", "gpio.read", "gpio.write"]
    );

    assert_eq!(
        call(&mut registry, "gpio.info", "help").unwrap(),
        EXPECTED_HELP
    );
    let summary = call(&mut registry, "gpio.info", "").unwrap();
    assert!(summary.contains("family=esp32"), "{summary}");
    assert!(summary.contains("backend=virtual"), "{summary}");
    assert!(summary.contains("configured=0"), "{summary}");

    assert_eq!(
        call(&mut registry, "gpio.config", "IO4 out level=0").unwrap(),
        "pin=IO4 mode=output level=0 pull=none open_drain=0"
    );
    assert_eq!(
        call(&mut registry, "gpio.write", "IO4 1").unwrap(),
        "pin=IO4 level=1"
    );
    // 输出模式读失败，切回输入后成功。
    assert!(call(&mut registry, "gpio.read", "IO4").is_err());
    call(&mut registry, "gpio.config", "IO4 in").unwrap();
    assert_eq!(
        call(&mut registry, "gpio.read", "IO4").unwrap(),
        "pin=IO4 level=0"
    );
    let summary = call(&mut registry, "gpio.info", "").unwrap();
    assert!(summary.ends_with("configured=1"), "{summary}");
}

#[test]
fn handlers_run_on_another_thread() {
    let mut main_registry = registry();
    // 先在主线程配置一个引脚，验证 Arc 句柄状态跨线程可见。
    call(&mut main_registry, "gpio.config", "IO5 out level=0").unwrap();

    let handle = std::thread::spawn(move || {
        let mut registry = main_registry;
        let help = call(&mut registry, "gpio.info", "help").unwrap();
        let write = call(&mut registry, "gpio.write", "IO5 1").unwrap();
        let detail = call(&mut registry, "gpio.info", "IO5").unwrap();
        (help, write, detail)
    });
    let (help, write, detail) = handle.join().expect("工具应能在其他线程中调用");
    assert_eq!(help, EXPECTED_HELP);
    assert_eq!(write, "pin=IO5 level=1");
    assert!(detail.contains("mode=output"), "{detail}");
}

#[test]
fn duplicate_registration_returns_duplicate() {
    let mut registry = ToolRegistry::new();
    registry.register("gpio.read", DummyTool).unwrap();
    let error = register_tools(&mut registry, VirtualGpio::new(ChipFamily::Esp32)).unwrap_err();
    assert_eq!(error, RegistryError::Duplicate);
    assert!(registry.contains("gpio.config"));
    assert!(!registry.contains("gpio.write"));
}
