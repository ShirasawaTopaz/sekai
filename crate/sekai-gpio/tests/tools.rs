//! `gpio.*` 工具文本协议的集成测试。
//!
//! 覆盖实施计划 §5：help 固定文本、家族摘要、单引脚详情、STM32 端口列表、
//! config 部分更新与错误、read/write 模式校验、参数上限以及注册/重复注册行为。
//! 后端统一使用宿主虚拟后端（`VirtualGpio`），语义与真实硬件一致。

use sekai::sekai_device_list::DeviceList;
use sekai::sekai_tool::{RegistryError, ToolContext, ToolHandler, ToolRegistry};
use sekai::sekai_wire::{CommandError, ErrorCode};
use sekai_gpio::virtual_gpio::VirtualGpio;
use sekai_gpio::{ChipFamily, Level, PinId, parse_external_levels, register_tools};

/// 计划 §5.3 的 `gpio.info help` 固定文本（逐字，末尾无换行）。
const EXPECTED_HELP: &str = "gpio tools v1\n\
info [help|PIN|port=A]\n\
config PIN [in|out] [level=0|1] [pull=none|up|down] [open_drain=0|1]\n\
read PIN\n\
write PIN 0|1\n\
pins: IO4 / PA5\n\
read requires input mode; write requires output mode";

/// 用指定家族的虚拟后端创建已注册四个工具的注册表。
fn registry(family: ChipFamily) -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    register_tools(&mut registry, VirtualGpio::new(family)).expect("注册 GPIO 工具");
    registry
}

/// 创建注册表，并把某个引脚的模拟外部输入电平设为 `level`。
fn registry_with_input(family: ChipFamily, pin: PinId, level: Level) -> ToolRegistry {
    let mut gpio = VirtualGpio::new(family);
    gpio.set_external_level(pin, level).expect("设置外部电平");
    let mut registry = ToolRegistry::new();
    register_tools(&mut registry, gpio).expect("注册 GPIO 工具");
    registry
}

/// 调用工具并返回 UTF-8 结果文本。
fn call(registry: &mut ToolRegistry, name: &str, args: &str) -> Result<String, CommandError> {
    let bytes = call_bytes(registry, name, args.as_bytes())?;
    Ok(String::from_utf8(bytes).expect("工具结果必须是 UTF-8"))
}

/// 调用工具并返回原始结果字节。
fn call_bytes(
    registry: &mut ToolRegistry,
    name: &str,
    args: &[u8],
) -> Result<Vec<u8>, CommandError> {
    let workers = DeviceList::new();
    let mut ctx = ToolContext {
        args,
        now_mono_ms: 0,
        now_unix_ms: None,
        workers: &workers,
    };
    let tool = registry
        .get_mut(name)
        .unwrap_or_else(|| panic!("工具 {name} 未注册"));
    tool.call(&mut ctx)
}

/// 调用工具并断言失败，返回错误。
fn call_err(registry: &mut ToolRegistry, name: &str, args: &str) -> CommandError {
    call(registry, name, args).expect_err("调用应当失败")
}

/// 断言错误码为 `InvalidArgument`。
fn assert_invalid(error: &CommandError) {
    assert_eq!(
        error.code,
        ErrorCode::InvalidArgument,
        "消息：{}",
        error.message
    );
}

#[test]
fn help_text_is_exact_for_every_family() {
    for family in [ChipFamily::Esp32, ChipFamily::Stm32F1, ChipFamily::Stm32F4] {
        let mut registry = registry(family);
        assert_eq!(
            call(&mut registry, "gpio.info", "help").unwrap(),
            EXPECTED_HELP,
            "{family} 的 help 文本必须逐字一致"
        );
        // 大小写不敏感。
        assert_eq!(
            call(&mut registry, "gpio.info", "HELP").unwrap(),
            EXPECTED_HELP
        );
    }
}

#[test]
fn esp32_summary_matches_contract() {
    let mut registry = registry(ChipFamily::Esp32);
    assert_eq!(
        call(&mut registry, "gpio.info", "").unwrap(),
        "gpio=1\n\
family=esp32\n\
chip=esp32\n\
backend=virtual\n\
pins=34\n\
labels=IO0-IO19,IO21-IO23,IO25-IO27,IO32-IO39\n\
reserved=IO6-IO11\n\
input_only=IO34-IO39\n\
configured=0"
    );
}

#[test]
fn stm32f1_summary_matches_contract() {
    let mut registry = registry(ChipFamily::Stm32F1);
    assert_eq!(
        call(&mut registry, "gpio.info", "").unwrap(),
        "gpio=1\n\
family=stm32\n\
chip=stm32f1\n\
backend=virtual\n\
ports=A,B,C\n\
pins=35\n\
labels=PA0-PA15,PB0-PB15,PC13-PC15\n\
reserved=PA13-PA15,PB3,PB4\n\
input_only=\n\
configured=0"
    );
}

#[test]
fn stm32f4_summary_matches_contract() {
    let mut registry = registry(ChipFamily::Stm32F4);
    assert_eq!(
        call(&mut registry, "gpio.info", "").unwrap(),
        "gpio=1\n\
family=stm32\n\
chip=stm32f4\n\
backend=virtual\n\
ports=A,B,C,D,E\n\
pins=80\n\
labels=PA0-PA15,PB0-PB15,PC0-PC15,PD0-PD15,PE0-PE15\n\
reserved=PA13-PA15,PB3,PB4\n\
input_only=\n\
configured=0"
    );
}

#[test]
fn summary_counts_configured_pins() {
    let mut registry = registry(ChipFamily::Esp32);
    call(&mut registry, "gpio.config", "IO4 out").unwrap();
    call(&mut registry, "gpio.config", "IO5 in pull=up").unwrap();
    let summary = call(&mut registry, "gpio.info", "").unwrap();
    assert!(summary.ends_with("configured=2"), "{summary}");
}

#[test]
fn pin_detail_unconfigured_matches_contract() {
    let mut registry = registry(ChipFamily::Esp32);
    assert_eq!(
        call(&mut registry, "gpio.info", "IO4").unwrap(),
        "pin=IO4\n\
caps=in,out,pu,pd,od\n\
flags=none\n\
mode=unconfigured\n\
configured=0"
    );
}

#[test]
fn pin_detail_reserved_shows_flags_and_note() {
    let mut registry = registry(ChipFamily::Esp32);
    assert_eq!(
        call(&mut registry, "gpio.info", "IO6").unwrap(),
        "pin=IO6\n\
caps=none\n\
flags=reserved\n\
note=SPI flash\n\
mode=unconfigured\n\
configured=0"
    );
}

#[test]
fn pin_detail_configured_output_matches_contract() {
    let mut registry = registry(ChipFamily::Esp32);
    call(&mut registry, "gpio.config", "IO4 out level=1").unwrap();
    assert_eq!(
        call(&mut registry, "gpio.info", "IO4").unwrap(),
        "pin=IO4\n\
caps=in,out,pu,pd,od\n\
flags=none\n\
mode=output\n\
level=1\n\
pull=none\n\
open_drain=0\n\
configured=1"
    );
}

#[test]
fn pin_detail_configured_input_reports_level() {
    let mut registry = registry_with_input(ChipFamily::Esp32, PinId::io(4), Level::High);
    call(&mut registry, "gpio.config", "IO4 in pull=up").unwrap();
    assert_eq!(
        call(&mut registry, "gpio.info", "IO4").unwrap(),
        "pin=IO4\n\
caps=in,out,pu,pd,od\n\
flags=none\n\
mode=input\n\
level=1\n\
pull=up\n\
open_drain=0\n\
configured=1"
    );
}

#[test]
fn stm32_port_listing_f1_matches_contract() {
    let mut registry = registry(ChipFamily::Stm32F1);
    let listing = call(&mut registry, "gpio.info", "port=A").unwrap();
    let lines: Vec<&str> = listing.lines().collect();
    assert_eq!(lines.len(), 17, "首行端口标题 + 16 个 PAx");
    assert_eq!(lines[0], "port=A");
    assert_eq!(lines[1], "PA0 caps=in,out,pu,pd,od flags=none");
    assert_eq!(lines[13], "PA12 caps=in,out,pu,pd,od flags=none");
    assert_eq!(lines[14], "PA13 caps=in,out,pu,pd,od flags=reserved,swd");
    assert_eq!(lines[15], "PA14 caps=in,out,pu,pd,od flags=reserved,swd");
    assert_eq!(lines[16], "PA15 caps=in,out,pu,pd,od flags=reserved,jtag");
    // 值大小写不敏感，输出规范化。
    assert_eq!(call(&mut registry, "gpio.info", "PORT=a").unwrap(), listing);
}

#[test]
fn stm32_port_listing_f4_uses_all_ports() {
    let mut registry = registry(ChipFamily::Stm32F4);
    let listing = call(&mut registry, "gpio.info", "port=E").unwrap();
    assert_eq!(listing.lines().count(), 17);
    assert!(listing.starts_with("port=E\nPE0 caps="), "{listing}");
    assert!(listing.ends_with("PE15 caps=in,out,pu,pd,od flags=none"));
    let c_port = call(&mut registry, "gpio.info", "port=C").unwrap();
    assert_eq!(c_port.lines().count(), 17);
}

#[test]
fn port_listing_errors() {
    // ESP32 没有端口列表。
    let mut esp32 = registry(ChipFamily::Esp32);
    let error = call_err(&mut esp32, "gpio.info", "port=A");
    assert_invalid(&error);
    assert!(error.message.contains("没有端口"), "{}", error.message);

    // F1 只有 A/B/C。
    let mut f1 = registry(ChipFamily::Stm32F1);
    let error = call_err(&mut f1, "gpio.info", "port=D");
    assert_invalid(&error);
    assert!(error.message.contains('D'), "{}", error.message);

    // 非法端口字母。
    let error = call_err(&mut f1, "gpio.info", "port=Z");
    assert_invalid(&error);

    // 未知参数键。
    let error = call_err(&mut f1, "gpio.info", "foo=1");
    assert_invalid(&error);
}

#[test]
fn info_pin_errors() {
    let mut esp32 = registry(ChipFamily::Esp32);
    // 表内不存在。
    let error = call_err(&mut esp32, "gpio.info", "IO20");
    assert_invalid(&error);
    assert!(error.message.contains("IO20"), "{}", error.message);
    // 语法非法（前导零）。
    let error = call_err(&mut esp32, "gpio.info", "IO04");
    assert_invalid(&error);
    // 别的家族引脚。
    let error = call_err(&mut esp32, "gpio.info", "PA5");
    assert_invalid(&error);
    // 参数过多。
    let error = call_err(&mut esp32, "gpio.info", "IO4 extra");
    assert_invalid(&error);

    let mut f1 = registry(ChipFamily::Stm32F1);
    let error = call_err(&mut f1, "gpio.info", "PC0");
    assert_invalid(&error);
}

#[test]
fn config_output_matches_contract() {
    let mut registry = registry(ChipFamily::Stm32F1);
    assert_eq!(
        call(&mut registry, "gpio.config", "PA5 out level=1").unwrap(),
        "pin=PA5 mode=output level=1 pull=none open_drain=0"
    );
    // 大小写不敏感 + 键别名 + 开漏。
    assert_eq!(
        call(
            &mut registry,
            "gpio.config",
            "pa5 dir=output level=high od=on"
        )
        .unwrap(),
        "pin=PA5 mode=output level=1 pull=none open_drain=1"
    );
}

#[test]
fn config_input_matches_contract() {
    let mut registry = registry(ChipFamily::Stm32F1);
    assert_eq!(
        call(&mut registry, "gpio.config", "PA5 in pull=up").unwrap(),
        "pin=PA5 mode=input level=0 pull=up open_drain=0"
    );
}

#[test]
fn config_defaults_to_safe_input() {
    let mut registry = registry(ChipFamily::Esp32);
    assert_eq!(
        call(&mut registry, "gpio.config", "4").unwrap(),
        "pin=IO4 mode=input level=0 pull=none open_drain=0"
    );
    assert_eq!(
        call(&mut registry, "gpio.config", "GPIO5 dir=input pull=none").unwrap(),
        "pin=IO5 mode=input level=0 pull=none open_drain=0"
    );
}

#[test]
fn config_partial_update_keeps_direction() {
    let mut registry = registry(ChipFamily::Esp32);
    call(&mut registry, "gpio.config", "IO4 out level=1").unwrap();
    assert_eq!(
        call(&mut registry, "gpio.config", "IO4 level=0").unwrap(),
        "pin=IO4 mode=output level=0 pull=none open_drain=0"
    );
    assert_eq!(
        call(&mut registry, "gpio.config", "IO4 od=1").unwrap(),
        "pin=IO4 mode=output level=0 pull=none open_drain=1"
    );
}

#[test]
fn config_switch_to_input_clears_output_fields() {
    let mut registry = registry(ChipFamily::Esp32);
    call(&mut registry, "gpio.config", "IO4 out level=1 od=1").unwrap();
    assert_eq!(
        call(&mut registry, "gpio.config", "IO4 in pull=down").unwrap(),
        "pin=IO4 mode=input level=0 pull=down open_drain=0"
    );
}

#[test]
fn config_result_is_single_line() {
    let mut registry = registry(ChipFamily::Esp32);
    let result = call(&mut registry, "gpio.config", "IO4 out").unwrap();
    assert!(!result.contains('\n'));
    assert!(!result.ends_with(' '));
}

#[test]
fn config_rejects_duplicate_direction() {
    let mut registry = registry(ChipFamily::Esp32);
    for args in ["IO4 in out", "IO4 dir=in direction=out", "IO4 in dir=out"] {
        let error = call_err(&mut registry, "gpio.config", args);
        assert_invalid(&error);
        assert!(error.message.contains("重复"), "{args}: {}", error.message);
    }
}

#[test]
fn config_rejects_duplicate_keys() {
    let mut registry = registry(ChipFamily::Esp32);
    for args in [
        "IO4 out level=1 level=0",
        "IO4 out level=1 init=0",
        "IO4 in pull=up pull=down",
        "IO4 out od=1 open_drain=0",
    ] {
        let error = call_err(&mut registry, "gpio.config", args);
        assert_invalid(&error);
        assert!(error.message.contains("重复"), "{args}: {}", error.message);
    }
}

#[test]
fn config_rejects_unknown_keys_and_tokens() {
    let mut registry = registry(ChipFamily::Esp32);
    for args in ["IO4 foo=1", "IO4 bogus", "IO4 =1", "IO4 level="] {
        let error = call_err(&mut registry, "gpio.config", args);
        assert_invalid(&error);
    }
}

#[test]
fn config_rejects_direction_sensitive_fields() {
    let mut registry = registry(ChipFamily::Esp32);
    // 未配置默认输入：level 非法。
    let error = call_err(&mut registry, "gpio.config", "IO4 level=1");
    assert_invalid(&error);
    assert!(
        error.message.contains("gpio.config IO4 out"),
        "{}",
        error.message
    );
    // 输出模式禁止 pull。
    let error = call_err(&mut registry, "gpio.config", "IO4 out pull=up");
    assert_invalid(&error);
    assert!(
        error.message.contains("gpio.config IO4 in"),
        "{}",
        error.message
    );
    // 输入模式禁止 open_drain=1。
    let error = call_err(&mut registry, "gpio.config", "IO4 in od=1");
    assert_invalid(&error);
}

#[test]
fn config_rejects_reserved_unknown_and_input_only_pins() {
    let mut registry = registry(ChipFamily::Esp32);
    // 保留引脚：允许 gpio.info 详情，但 configure 一律拒绝。
    let error = call_err(&mut registry, "gpio.config", "IO6 out");
    assert_invalid(&error);
    assert!(error.message.contains("保留"), "{}", error.message);
    // 不存在。
    let error = call_err(&mut registry, "gpio.config", "IO20 out");
    assert_invalid(&error);
    // 输入专用。
    let error = call_err(&mut registry, "gpio.config", "IO34 out");
    assert_invalid(&error);
    assert!(error.message.contains("仅支持输入"), "{}", error.message);
    // 输入专用引脚可以配置为输入。
    assert_eq!(
        call(&mut registry, "gpio.config", "IO34 in").unwrap(),
        "pin=IO34 mode=input level=0 pull=none open_drain=0"
    );
}

#[test]
fn config_requires_pin() {
    let mut registry = registry(ChipFamily::Esp32);
    let error = call_err(&mut registry, "gpio.config", "   ");
    assert_invalid(&error);
}

#[test]
fn injected_inputs_are_readable_through_tools() {
    // P12：`--gpio-inputs` 的等价路径——初始外部电平在注册前注入，配置为
    // 输入后 `gpio.read` 返回注入值（覆盖高/低两种）。
    let inputs = parse_external_levels(ChipFamily::Esp32.spec(), "IO4=1, IO5=0").unwrap();
    let mut registry = ToolRegistry::new();
    register_tools(
        &mut registry,
        VirtualGpio::with_inputs(ChipFamily::Esp32, &inputs).expect("注入初始电平"),
    )
    .expect("注册 GPIO 工具");

    call(&mut registry, "gpio.config", "IO4 in").unwrap();
    call(&mut registry, "gpio.config", "IO5 in").unwrap();
    assert_eq!(
        call(&mut registry, "gpio.read", "IO4").unwrap(),
        "pin=IO4 level=1"
    );
    assert_eq!(
        call(&mut registry, "gpio.read", "IO5").unwrap(),
        "pin=IO5 level=0"
    );
}

#[test]
fn read_write_flow_matches_contract() {
    let mut registry = registry_with_input(ChipFamily::Esp32, PinId::io(4), Level::High);
    // 未配置读写：提示先 gpio.config。
    let error = call_err(&mut registry, "gpio.read", "IO4");
    assert_invalid(&error);
    assert!(
        error.message.contains("gpio.config IO4 in"),
        "{}",
        error.message
    );
    let error = call_err(&mut registry, "gpio.write", "IO4 1");
    assert_invalid(&error);
    assert!(
        error.message.contains("gpio.config IO4 out"),
        "{}",
        error.message
    );

    call(&mut registry, "gpio.config", "IO4 out level=0").unwrap();
    assert_eq!(
        call(&mut registry, "gpio.write", "IO4 1").unwrap(),
        "pin=IO4 level=1"
    );
    assert_eq!(
        call(&mut registry, "gpio.write", "IO4 low").unwrap(),
        "pin=IO4 level=0"
    );

    // 输出模式读 → 错误，提示先 gpio.config IO4 in。
    let error = call_err(&mut registry, "gpio.read", "IO4");
    assert_invalid(&error);
    assert!(
        error.message.contains("gpio.config IO4 in"),
        "{}",
        error.message
    );

    // 切回输入并读取外部电平。
    call(&mut registry, "gpio.config", "IO4 in").unwrap();
    assert_eq!(
        call(&mut registry, "gpio.read", "IO4").unwrap(),
        "pin=IO4 level=1"
    );
}

#[test]
fn write_rejects_input_mode_and_bad_arguments() {
    let mut registry = registry(ChipFamily::Esp32);
    call(&mut registry, "gpio.config", "IO4 in").unwrap();
    let error = call_err(&mut registry, "gpio.write", "IO4 1");
    assert_invalid(&error);
    assert!(
        error.message.contains("gpio.config IO4 out"),
        "{}",
        error.message
    );

    call(&mut registry, "gpio.config", "IO5 out").unwrap();
    // 非法电平。
    let error = call_err(&mut registry, "gpio.write", "IO5 2");
    assert_invalid(&error);
    // 缺少电平 / 多余参数。
    assert_invalid(&call_err(&mut registry, "gpio.write", "IO5"));
    assert_invalid(&call_err(&mut registry, "gpio.write", "IO5 1 extra"));
}

#[test]
fn read_requires_exactly_one_pin() {
    let mut registry = registry(ChipFamily::Esp32);
    call(&mut registry, "gpio.config", "IO4 in").unwrap();
    assert_invalid(&call_err(&mut registry, "gpio.read", ""));
    assert_invalid(&call_err(&mut registry, "gpio.read", "IO4 extra"));
}

#[test]
fn read_write_reject_reserved_pins() {
    let mut registry = registry(ChipFamily::Esp32);
    let error = call_err(&mut registry, "gpio.read", "IO6");
    assert_invalid(&error);
    assert!(error.message.contains("保留"), "{}", error.message);
    let error = call_err(&mut registry, "gpio.write", "IO6 1");
    assert_invalid(&error);
}

#[test]
fn arguments_are_limited_to_256_bytes() {
    let mut registry = registry(ChipFamily::Esp32);
    // 恰好 256 字节可用（多余空白被忽略）。
    let mut exact = String::from("IO4");
    exact.push_str(&" ".repeat(253));
    assert_eq!(exact.len(), 256);
    assert!(call(&mut registry, "gpio.info", &exact).is_ok());

    // 257 字节被拒绝。
    let mut too_long = String::from("IO4");
    too_long.push_str(&" ".repeat(254));
    assert_eq!(too_long.len(), 257);
    let error = call_err(&mut registry, "gpio.info", &too_long);
    assert_invalid(&error);
    assert!(error.message.contains("256"), "{}", error.message);

    // 非 UTF-8 被拒绝。
    let error = call_bytes(&mut registry, "gpio.info", &[0xff, 0xfe]).unwrap_err();
    assert_invalid(&error);
    assert!(error.message.contains("UTF-8"), "{}", error.message);
}

/// 仅用于重复注册测试的占位工具。
struct DummyTool;

impl ToolHandler for DummyTool {
    fn call(&mut self, _ctx: &mut ToolContext<'_>) -> Result<Vec<u8>, CommandError> {
        Ok(Vec::new())
    }
}

#[test]
fn register_tools_registers_four_names() {
    let registry = registry(ChipFamily::Esp32);
    let names: Vec<&str> = registry.names().collect();
    assert_eq!(
        names,
        vec!["gpio.config", "gpio.info", "gpio.read", "gpio.write"]
    );
    assert_eq!(registry.len(), 4);
}

#[test]
fn duplicate_gpio_tool_registration_returns_duplicate() {
    let mut registry = ToolRegistry::new();
    registry.register("gpio.info", DummyTool).unwrap();
    let error = register_tools(&mut registry, VirtualGpio::new(ChipFamily::Esp32)).unwrap_err();
    assert_eq!(error, RegistryError::Duplicate);
    // 按 GPIO_TOOL_NAMES 顺序逐个注册，遇到重复立即返回。
    assert!(registry.contains("gpio.config"));
    assert!(registry.contains("gpio.info"));
    assert!(!registry.contains("gpio.read"));
}
