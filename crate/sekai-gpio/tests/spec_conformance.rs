#![deny(unsafe_code)]
//! 计划文档 docs/sekai-gpio-plan.md 的规格一致性测试。
//!
//! 覆盖范围：
//! - §5.2 引脚名解析/归一化与非法输入；
//! - §4.2 家族归属（FamilySpec::resolve）与错误类型/消息；
//! - §4.5/§6.4/§12.1 三家族引脚表（数量、槽位、保留/输入专用/不存在、标志、摘要）；
//! - §4.3 配置合并九条规则；
//! - §5.9 错误码映射与消息关键字。
//!
//! 这些测试与实现相互独立：只通过 sekai-gpio 的公开 API 断言文档语义，
//! 实现漂移时会失败。

use sekai::sekai_wire::{CommandError, ErrorCode};
use sekai_gpio::config::{
    ConfigRequest, Direction, Level, PinConfig, Pull, ensure_not_reserved, resolve_config,
    validate_config,
};
use sekai_gpio::family::{ChipFamily, FamilyKind, FamilySpec, PinCaps, PinFlags, PinSpec};
use sekai_gpio::pin::{PinId, PinParseError, Port};
use sekai_gpio::port::{GpioError, lookup};

/// 当前 crate 提供的全部芯片家族。
const FAMILIES: [ChipFamily; 3] = [ChipFamily::Esp32, ChipFamily::Stm32F1, ChipFamily::Stm32F4];

fn spec(family: ChipFamily) -> &'static FamilySpec {
    family.spec()
}

/// 取家族表内的引脚；不存在时直接让测试失败。
fn pin(family: ChipFamily, id: PinId) -> &'static PinSpec {
    spec(family)
        .by_id(id)
        .unwrap_or_else(|| panic!("{} 家族表缺少引脚 {id}", family.chip()))
}

/// 构造 gpio.config 的部分更新请求。
fn request(
    direction: Option<Direction>,
    level: Option<Level>,
    pull: Option<Pull>,
    open_drain: Option<bool>,
) -> ConfigRequest {
    ConfigRequest {
        direction,
        level,
        pull,
        open_drain,
    }
}

/// 分配一个 'static 的合成引脚表项，用于覆盖真实引脚表没有的能力组合。
fn leaked_spec(id: PinId, caps: PinCaps, flags: PinFlags) -> &'static PinSpec {
    Box::leak(Box::new(PinSpec {
        id,
        slot: 0,
        caps,
        flags,
        note: "",
    }))
}

/// 按谓词筛出带某标志的引脚（保持表内顺序）。
fn flagged(family: ChipFamily, predicate: impl Fn(&PinFlags) -> bool) -> Vec<PinId> {
    spec(family)
        .pins
        .iter()
        .filter(|entry| predicate(&entry.flags))
        .map(|entry| entry.id)
        .collect()
}

fn assert_code(error: &GpioError, expected: ErrorCode) {
    assert_eq!(error.code(), expected, "错误消息：{}", error.message());
}

// ---------------------------------------------------------------------------
// §5.2 引脚名解析
// ---------------------------------------------------------------------------

#[test]
fn esp32_pin_forms_are_accepted_and_normalized() {
    for text in ["IO4", "GPIO4", "4", "io4", "gpio4", "Gpio4"] {
        assert_eq!(PinId::parse(text), Ok(PinId::Io(4)), "text = {text:?}");
    }
    for text in ["IO0", "io0", "0", "GPIO0"] {
        assert_eq!(PinId::parse(text), Ok(PinId::Io(0)), "text = {text:?}");
    }
    for text in ["IO39", "GPIO39", "39", "io39"] {
        assert_eq!(PinId::parse(text), Ok(PinId::Io(39)), "text = {text:?}");
    }
    // 规范化输出：IO4（不是 io4/GPIO4/4）。
    assert_eq!(PinId::parse("gPiO4").unwrap().to_string(), "IO4");
    assert_eq!(PinId::parse("4").unwrap().label().to_string(), "IO4");
}

#[test]
fn stm32_pin_forms_are_accepted_and_normalized() {
    for text in ["PA5", "pa5", "Pa5", "pA5"] {
        assert_eq!(
            PinId::parse(text),
            Ok(PinId::port(Port::A, 5)),
            "text = {text:?}"
        );
    }
    let cases = [
        ("PC13", Port::C, 13),
        ("PD0", Port::D, 0),
        ("PE15", Port::E, 15),
        ("PJ0", Port::J, 0),
        ("PK15", Port::K, 15),
    ];
    for (text, port, number) in cases {
        assert_eq!(
            PinId::parse(text),
            Ok(PinId::port(port, number)),
            "text = {text:?}"
        );
    }
    assert_eq!(PinId::port(Port::A, 5).to_string(), "PA5");
    assert_eq!(PinId::parse("pE15").unwrap().to_string(), "PE15");
}

#[test]
fn pin_syntax_errors_match_plan() {
    let cases = [
        ("", PinParseError::Empty),
        ("IO", PinParseError::InvalidSyntax),
        ("io", PinParseError::InvalidSyntax),
        ("A5", PinParseError::InvalidSyntax),
        ("P5", PinParseError::InvalidSyntax),
        ("P", PinParseError::InvalidSyntax),
        ("IO4x", PinParseError::InvalidSyntax),
        ("PAA1", PinParseError::InvalidSyntax),
        ("P#0", PinParseError::InvalidPort),
        ("PL0", PinParseError::InvalidPort),
        ("IO04", PinParseError::LeadingZero),
        ("04", PinParseError::LeadingZero),
        ("GPIO04", PinParseError::LeadingZero),
        ("PA05", PinParseError::LeadingZero),
        ("IO40", PinParseError::IoOutOfRange),
        ("40", PinParseError::IoOutOfRange),
        ("999", PinParseError::IoOutOfRange),
        ("GPIO40", PinParseError::IoOutOfRange),
        ("PA16", PinParseError::PinOutOfRange),
        ("PJ16", PinParseError::PinOutOfRange),
        ("P123456789", PinParseError::TooLong),
        ("IO12345678", PinParseError::TooLong),
        ("GPIO12345", PinParseError::TooLong),
    ];
    for (text, expected) in cases {
        assert_eq!(PinId::parse(text), Err(expected), "text = {text:?}");
    }

    // 每条语法错误都有可读消息（会拼进 gpio.* 的 InvalidArgument 消息）。
    let messages = [
        (PinParseError::Empty, "不能为空"),
        (PinParseError::TooLong, "8"),
        (PinParseError::InvalidSyntax, "无法识别"),
        (PinParseError::InvalidPort, "A..K"),
        (PinParseError::LeadingZero, "前导零"),
        (PinParseError::IoOutOfRange, "0..=39"),
        (PinParseError::PinOutOfRange, "0..=15"),
    ];
    for (error, needle) in messages {
        let message = error.to_string();
        assert!(
            message.contains(needle),
            "语法错误 {error:?} 的消息 {message:?} 应包含 {needle:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// §4.2 / §5.2 家族归属与 resolve 错误
// ---------------------------------------------------------------------------

#[test]
fn resolve_accepts_documented_forms() {
    let cases = [
        (ChipFamily::Esp32, "IO4", PinId::Io(4)),
        (ChipFamily::Esp32, "GPIO4", PinId::Io(4)),
        (ChipFamily::Esp32, "4", PinId::Io(4)),
        (ChipFamily::Esp32, "io4", PinId::Io(4)),
        (ChipFamily::Esp32, "gPiO4", PinId::Io(4)),
        (ChipFamily::Esp32, "IO0", PinId::Io(0)),
        (ChipFamily::Esp32, "IO34", PinId::Io(34)),
        (ChipFamily::Stm32F1, "PA5", PinId::port(Port::A, 5)),
        (ChipFamily::Stm32F1, "pa5", PinId::port(Port::A, 5)),
        (ChipFamily::Stm32F1, "PC13", PinId::port(Port::C, 13)),
        (ChipFamily::Stm32F4, "PA5", PinId::port(Port::A, 5)),
        (ChipFamily::Stm32F4, "PE15", PinId::port(Port::E, 15)),
        (ChipFamily::Stm32F4, "PC0", PinId::port(Port::C, 0)),
    ];
    for (family, text, expected) in cases {
        let resolved = spec(family)
            .resolve(text)
            .unwrap_or_else(|error| panic!("{} 应接受 {text:?}：{error}", family.chip()));
        assert_eq!(resolved.id, expected, "{} resolve({text:?})", family.chip());
        assert_eq!(PinId::parse(text).unwrap(), expected);
        assert_eq!(resolved.slot, expected.slot(spec(family)).unwrap());
    }
}

#[test]
fn resolve_reports_syntax_errors_as_invalid_argument() {
    let cases = [
        (ChipFamily::Esp32, ""),
        (ChipFamily::Esp32, "IO"),
        (ChipFamily::Esp32, "IO04"),
        (ChipFamily::Esp32, "A5"),
        (ChipFamily::Esp32, "P5"),
        (ChipFamily::Stm32F1, ""),
        (ChipFamily::Stm32F1, "PA05"),
        (ChipFamily::Stm32F4, "P5"),
    ];
    for (family, text) in cases {
        let error = spec(family).resolve(text).unwrap_err();
        assert_code(&error, ErrorCode::InvalidArgument);
        assert!(
            matches!(&error, GpioError::InvalidArgument(_)),
            "{:?} 应为 InvalidArgument，实际 {error:?}",
            text
        );
        assert!(
            error.message().contains("非法"),
            "消息应说明参数非法：{}",
            error.message()
        );
    }

    // 语法错误消息带具体原因，方便 AI 纠正。
    let message = spec(ChipFamily::Esp32)
        .resolve("IO04")
        .unwrap_err()
        .message()
        .to_string();
    assert!(message.contains("前导零"), "{message}");
    let message = spec(ChipFamily::Esp32)
        .resolve("")
        .unwrap_err()
        .message()
        .to_string();
    assert!(message.contains("不能为空"), "{message}");
    let message = spec(ChipFamily::Esp32)
        .resolve("IO")
        .unwrap_err()
        .message()
        .to_string();
    assert!(message.contains("无法识别"), "{message}");
}

#[test]
fn resolve_reports_pins_outside_family_as_unknown_pin() {
    let cases = [
        // ESP32 芯片未引出 / 非 ESP32 家族。
        (ChipFamily::Esp32, "IO20"),
        (ChipFamily::Esp32, "IO24"),
        (ChipFamily::Esp32, "IO28"),
        (ChipFamily::Esp32, "IO29"),
        (ChipFamily::Esp32, "IO30"),
        (ChipFamily::Esp32, "IO31"),
        (ChipFamily::Esp32, "IO40"),
        (ChipFamily::Esp32, "PA5"),
        (ChipFamily::Esp32, "PC0"),
        (ChipFamily::Esp32, "PJ0"),
        (ChipFamily::Esp32, "PA16"),
        // F1 表外：PC0..PC12、D/E/J/K 端口、越界编号。
        (ChipFamily::Stm32F1, "PC0"),
        (ChipFamily::Stm32F1, "PC12"),
        (ChipFamily::Stm32F1, "PD0"),
        (ChipFamily::Stm32F1, "PE15"),
        (ChipFamily::Stm32F1, "PJ0"),
        (ChipFamily::Stm32F1, "PA16"),
        (ChipFamily::Stm32F1, "4"),
        // F4 表外：F 及之后的端口。
        (ChipFamily::Stm32F4, "PF0"),
        (ChipFamily::Stm32F4, "PG0"),
        (ChipFamily::Stm32F4, "PJ0"),
        (ChipFamily::Stm32F4, "PK15"),
        (ChipFamily::Stm32F4, "IO4"),
        (ChipFamily::Stm32F4, "4"),
    ];
    for (family, text) in cases {
        let error = spec(family).resolve(text).unwrap_err();
        assert_code(&error, ErrorCode::InvalidArgument);
        assert!(
            matches!(&error, GpioError::UnknownPin(_)),
            "{} resolve({text:?}) 应为 UnknownPin，实际 {error:?}",
            family.chip()
        );
        assert!(
            error.message().contains("不存在"),
            "消息应说明引脚不存在：{}",
            error.message()
        );
        assert!(
            error.message().contains(text),
            "消息应回显用户输入 {text:?}：{}",
            error.message()
        );
    }

    // §5.9 的精确示例消息。
    assert_eq!(
        spec(ChipFamily::Esp32)
            .resolve("IO40")
            .unwrap_err()
            .message(),
        "引脚 IO40 不存在"
    );
}

#[test]
fn resolve_still_returns_reserved_pins_for_info() {
    // §4.3 规则 8：保留引脚只拒绝 configure/read/write，gpio.info 详情仍可见。
    let io6 = spec(ChipFamily::Esp32).resolve("IO6").unwrap();
    assert!(io6.flags.reserved);
    let pa13 = spec(ChipFamily::Stm32F1).resolve("PA13").unwrap();
    assert!(pa13.flags.reserved);
    assert_eq!(pa13.note, "SWD");
}

// ---------------------------------------------------------------------------
// §4.5 / §6.4 / §12.1 家族引脚表
// ---------------------------------------------------------------------------

#[test]
fn family_metadata_matches_plan() {
    assert_eq!(ChipFamily::Esp32.kind(), FamilyKind::Esp32);
    assert_eq!(ChipFamily::Stm32F1.kind(), FamilyKind::Stm32);
    assert_eq!(ChipFamily::Stm32F4.kind(), FamilyKind::Stm32);
    assert_eq!(ChipFamily::Esp32.chip(), "esp32");
    assert_eq!(ChipFamily::Stm32F1.chip(), "stm32f1");
    assert_eq!(ChipFamily::Stm32F4.chip(), "stm32f4");
    assert_eq!(ChipFamily::Esp32.to_string(), "esp32");

    // 宿主环境变量 SEKAI_GPIO_FAMILY 的解析（大小写不敏感、允许首尾空白）。
    assert_eq!(ChipFamily::parse("ESP32"), Some(ChipFamily::Esp32));
    assert_eq!(ChipFamily::parse(" esp32 "), Some(ChipFamily::Esp32));
    assert_eq!(ChipFamily::parse("Stm32F1"), Some(ChipFamily::Stm32F1));
    assert_eq!(ChipFamily::parse("stm32f4"), Some(ChipFamily::Stm32F4));
    assert_eq!(ChipFamily::parse("esp8266"), None);
    assert_eq!(ChipFamily::parse("stm32"), None);
    assert_eq!(ChipFamily::parse(""), None);
}

#[test]
fn esp32_table_contains_exactly_the_classic_pins() {
    let family = ChipFamily::Esp32;
    let family_spec = spec(family);
    // 计划文档的“35”把不存在的 IO20 误算进 IO18..IO23；逐脚清单与硬件事实是
    // 34 个可用引脚（§4.5 表格、§6.4 明确 IO20/24/28..31 不存在）。
    assert_eq!(family_spec.pin_count(), 34);
    assert_eq!(family_spec.slots, 40);
    assert!(family_spec.ports.is_empty());
    assert_eq!(family_spec.family, FamilyKind::Esp32);

    let expected: Vec<u16> = (0u16..=19)
        .chain(21..=23)
        .chain(25..=27)
        .chain(32..=39)
        .collect();
    let actual: Vec<u16> = family_spec
        .pins
        .iter()
        .map(|entry| match entry.id {
            PinId::Io(number) => u16::from(number),
            PinId::Port { .. } => panic!("ESP32 表不应出现 STM32 引脚"),
        })
        .collect();
    assert_eq!(actual, expected);

    for number in [20u8, 24, 28, 29, 30, 31] {
        assert!(
            family_spec.by_id(PinId::io(number)).is_none(),
            "IO{number} 不存在"
        );
        assert!(
            family_spec.resolve(&format!("IO{number}")).is_err(),
            "IO{number} 应解析失败"
        );
    }
}

#[test]
fn esp32_slots_equal_io_numbers() {
    let family = ChipFamily::Esp32;
    for entry in spec(family).pins {
        match entry.id {
            PinId::Io(number) => {
                assert_eq!(entry.slot, u16::from(number), "IO{number} 槽位");
                assert!(usize::from(entry.slot) < spec(family).slots);
            }
            PinId::Port { .. } => panic!("ESP32 表不应出现 STM32 引脚"),
        }
    }
}

#[test]
fn esp32_capabilities_and_flags_match_plan() {
    let family = ChipFamily::Esp32;

    // IO6..=IO11：SPI flash 保留，无任何能力。
    for number in 6..=11u8 {
        let entry = pin(family, PinId::io(number));
        assert_eq!(entry.caps, PinCaps::NONE, "IO{number} 能力");
        assert!(entry.flags.reserved, "IO{number} 应保留");
        assert!(!entry.flags.input_only);
        assert_eq!(entry.note, "SPI flash");
    }

    // IO34..=IO39：仅输入、无内部上下拉、不可输出。
    for number in 34..=39u8 {
        let entry = pin(family, PinId::io(number));
        assert_eq!(entry.caps, PinCaps::INPUT_ONLY, "IO{number} 能力");
        assert!(entry.flags.input_only, "IO{number} 应标记 input_only");
        assert!(!entry.flags.reserved);
        assert_eq!(entry.note, "");
    }

    // 其余引脚全部能力。
    for entry in spec(family).pins {
        if entry.flags.reserved || entry.flags.input_only {
            continue;
        }
        assert_eq!(entry.caps, PinCaps::ALL, "{} 能力", entry.id);
    }

    // 标志集合必须精确，不能多也不能少。
    assert_eq!(
        flagged(family, |flags| flags.strap),
        vec![
            PinId::io(0),
            PinId::io(2),
            PinId::io(5),
            PinId::io(12),
            PinId::io(15)
        ]
    );
    assert_eq!(
        flagged(family, |flags| flags.uart0),
        vec![PinId::io(1), PinId::io(3)]
    );
    assert_eq!(
        flagged(family, |flags| flags.psram),
        vec![PinId::io(16), PinId::io(17)]
    );
    assert_eq!(
        flagged(family, |flags| flags.reserved),
        (6..=11u8).map(PinId::io).collect::<Vec<_>>()
    );
    assert_eq!(
        flagged(family, |flags| flags.input_only),
        (34..=39u8).map(PinId::io).collect::<Vec<_>>()
    );
    assert!(
        flagged(family, |flags| flags.swd
            || flags.jtag
            || flags.osc
            || flags.boot)
        .is_empty()
    );
}

#[test]
fn stm32f1_table_matches_plan() {
    let family = ChipFamily::Stm32F1;
    let family_spec = spec(family);
    assert_eq!(family_spec.pin_count(), 35);
    assert_eq!(family_spec.slots, 48);
    assert_eq!(family_spec.family, FamilyKind::Stm32);
    assert_eq!(family_spec.chip, "stm32f1");
    assert_eq!(family_spec.ports, &[Port::A, Port::B, Port::C]);

    // F1 的 GPIO 全部支持输入/输出/上下拉/开漏。
    for entry in family_spec.pins {
        assert_eq!(entry.caps, PinCaps::ALL, "{} 能力", entry.id);
        assert!(!entry.flags.input_only, "{} 不应标记 input_only", entry.id);
    }

    let reserved = [
        PinId::port(Port::A, 13),
        PinId::port(Port::A, 14),
        PinId::port(Port::A, 15),
        PinId::port(Port::B, 3),
        PinId::port(Port::B, 4),
    ];
    assert_eq!(flagged(family, |flags| flags.reserved), reserved.to_vec());
    assert_eq!(
        flagged(family, |flags| flags.swd),
        vec![PinId::port(Port::A, 13), PinId::port(Port::A, 14)]
    );
    assert_eq!(
        flagged(family, |flags| flags.jtag),
        vec![
            PinId::port(Port::A, 15),
            PinId::port(Port::B, 3),
            PinId::port(Port::B, 4)
        ]
    );
    assert_eq!(
        flagged(family, |flags| flags.boot),
        vec![PinId::port(Port::B, 2)]
    );
    assert_eq!(
        flagged(family, |flags| flags.osc),
        vec![PinId::port(Port::C, 14), PinId::port(Port::C, 15)]
    );
    assert!(
        flagged(family, |flags| flags.strap
            || flags.uart0
            || flags.psram
            || flags.input_only)
        .is_empty()
    );

    // 保留原因写在 note 里，会拼进 ReservedPin 消息。
    assert_eq!(pin(family, PinId::port(Port::A, 13)).note, "SWD");
    assert_eq!(pin(family, PinId::port(Port::A, 15)).note, "JTAG");
    assert_eq!(pin(family, PinId::port(Port::B, 2)).note, "BOOT1");
    assert_eq!(pin(family, PinId::port(Port::C, 13)).note, "RTC");
    assert_eq!(pin(family, PinId::port(Port::C, 14)).note, "LSE");
    assert_eq!(pin(family, PinId::port(Port::C, 15)).note, "LSE");

    // F1 只引出 PC13..PC15；PA/PB 全 16 位。
    for number in 0..=12u8 {
        assert!(
            family_spec.by_id(PinId::port(Port::C, number)).is_none(),
            "F1 不应有 PC{number}"
        );
    }
    assert!(family_spec.by_id(PinId::port(Port::D, 0)).is_none());
    for number in 0..=15u8 {
        assert!(family_spec.by_id(PinId::port(Port::A, number)).is_some());
        assert!(family_spec.by_id(PinId::port(Port::B, number)).is_some());
    }
}

#[test]
fn stm32f4_table_matches_plan() {
    let family = ChipFamily::Stm32F4;
    let family_spec = spec(family);
    assert_eq!(family_spec.pin_count(), 80);
    assert_eq!(family_spec.slots, 80);
    assert_eq!(family_spec.family, FamilyKind::Stm32);
    assert_eq!(family_spec.chip, "stm32f4");
    assert_eq!(
        family_spec.ports,
        &[Port::A, Port::B, Port::C, Port::D, Port::E]
    );

    for entry in family_spec.pins {
        assert_eq!(entry.caps, PinCaps::ALL, "{} 能力", entry.id);
        assert!(!entry.flags.input_only);
    }

    let reserved = [
        PinId::port(Port::A, 13),
        PinId::port(Port::A, 14),
        PinId::port(Port::A, 15),
        PinId::port(Port::B, 3),
        PinId::port(Port::B, 4),
    ];
    assert_eq!(flagged(family, |flags| flags.reserved), reserved.to_vec());
    assert_eq!(
        flagged(family, |flags| flags.swd),
        vec![PinId::port(Port::A, 13), PinId::port(Port::A, 14)]
    );
    assert_eq!(
        flagged(family, |flags| flags.jtag),
        vec![
            PinId::port(Port::A, 15),
            PinId::port(Port::B, 3),
            PinId::port(Port::B, 4)
        ]
    );
    assert_eq!(
        flagged(family, |flags| flags.osc),
        vec![PinId::port(Port::C, 14), PinId::port(Port::C, 15)]
    );
    // 计划 §4.5 的 F4 行没有 BOOT1 标志，与 F1 不同。
    assert!(flagged(family, |flags| flags.boot).is_empty());

    // A..E 全部 16 位；F 及之后的端口不在表内。
    for port in [Port::A, Port::B, Port::C, Port::D, Port::E] {
        for number in 0..=15u8 {
            assert!(
                family_spec.by_id(PinId::port(port, number)).is_some(),
                "F4 缺少 P{port}{number}"
            );
        }
    }
    for port in [Port::F, Port::G, Port::H, Port::I, Port::J, Port::K] {
        assert!(family_spec.by_id(PinId::port(port, 0)).is_none());
    }
}

#[test]
fn stm32_slots_are_port_16_plus_pin() {
    for family in [ChipFamily::Stm32F1, ChipFamily::Stm32F4] {
        for entry in spec(family).pins {
            match entry.id {
                PinId::Port { port, pin: number } => {
                    let expected = u16::from(port.index()) * 16 + u16::from(number);
                    assert_eq!(entry.slot, expected, "{} 槽位", entry.id);
                    assert!(usize::from(entry.slot) < spec(family).slots);
                }
                PinId::Io(_) => panic!("STM32 表不应出现 ESP32 引脚"),
            }
        }
    }
}

#[test]
fn pin_ids_and_slots_are_unique_per_family() {
    use std::collections::BTreeSet;

    for family in FAMILIES {
        let mut ids = BTreeSet::new();
        let mut slots = BTreeSet::new();
        for entry in spec(family).pins {
            assert!(
                ids.insert(entry.id),
                "{} 表内引脚 {} 重复",
                family.chip(),
                entry.id
            );
            assert!(
                slots.insert(entry.slot),
                "{} 表内槽位 {} 重复",
                family.chip(),
                entry.slot
            );
            assert!(
                usize::from(entry.slot) < spec(family).slots,
                "{} 的 {} 槽位越界",
                family.chip(),
                entry.id
            );
            assert_eq!(spec(family).by_id(entry.id), Some(entry));
            assert_eq!(entry.id.slot(spec(family)), Some(entry.slot));
        }
        assert_eq!(ids.len(), spec(family).pin_count());
        assert_eq!(slots.len(), spec(family).pin_count());
    }
}

#[test]
fn family_lookup_is_scoped_to_the_family() {
    assert!(
        spec(ChipFamily::Esp32)
            .by_id(PinId::port(Port::A, 5))
            .is_none()
    );
    assert!(spec(ChipFamily::Stm32F1).by_id(PinId::io(4)).is_none());
    assert!(
        spec(ChipFamily::Stm32F1)
            .by_id(PinId::port(Port::C, 0))
            .is_none()
    );
    assert!(
        spec(ChipFamily::Stm32F4)
            .by_id(PinId::port(Port::J, 0))
            .is_none()
    );
    assert!(
        spec(ChipFamily::Stm32F4)
            .by_id(PinId::port(Port::E, 0))
            .is_some()
    );

    let error = lookup(spec(ChipFamily::Esp32), PinId::port(Port::A, 5)).unwrap_err();
    assert_code(&error, ErrorCode::InvalidArgument);
    assert!(matches!(&error, GpioError::UnknownPin(_)));
    assert_eq!(error.message(), "引脚 PA5 不存在");
}

#[test]
fn family_summaries_match_documentation() {
    let esp32 = spec(ChipFamily::Esp32).summary();
    assert_eq!(esp32.pins, 34);
    assert!(esp32.ports.is_empty());
    // 注：计划 §4.5 的 labels 示例写的是 IO0-IO23,IO25-IO27,IO32-IO39，
    // 把不存在的 IO20/IO24 算进了范围，与同节的“不存在 IO20、IO24”矛盾；
    // 以逐脚清单（34 引脚）为准。
    assert_eq!(esp32.labels, "IO0-IO19,IO21-IO23,IO25-IO27,IO32-IO39");
    assert_eq!(esp32.reserved, "IO6-IO11");
    assert_eq!(esp32.input_only, "IO34-IO39");

    let f1 = spec(ChipFamily::Stm32F1).summary();
    assert_eq!(f1.pins, 35);
    assert_eq!(f1.ports, &[Port::A, Port::B, Port::C]);
    assert_eq!(f1.labels, "PA0-PA15,PB0-PB15,PC13-PC15");
    assert_eq!(f1.reserved, "PA13-PA15,PB3,PB4");
    assert_eq!(f1.input_only, "");

    let f4 = spec(ChipFamily::Stm32F4).summary();
    assert_eq!(f4.pins, 80);
    assert_eq!(f4.ports, &[Port::A, Port::B, Port::C, Port::D, Port::E]);
    assert_eq!(f4.labels, "PA0-PA15,PB0-PB15,PC0-PC15,PD0-PD15,PE0-PE15");
    assert_eq!(f4.reserved, "PA13-PA15,PB3,PB4");
    assert_eq!(f4.input_only, "");
}

#[test]
fn base_types_match_plan() {
    assert_eq!(Level::from_bool(true), Level::High);
    assert_eq!(Level::from_bool(false), Level::Low);
    assert!(Level::High.as_bool());
    assert!(!Level::Low.as_bool());
    assert_eq!(Direction::default(), Direction::Input);
    assert_eq!(Direction::Input.as_str(), "input");
    assert_eq!(Direction::Output.as_str(), "output");
    assert_eq!(Pull::default(), Pull::None);
    assert_eq!(Pull::None.as_str(), "none");
    assert_eq!(Pull::Up.as_str(), "up");
    assert_eq!(Pull::Down.as_str(), "down");

    let config = PinConfig::default();
    assert_eq!(config.direction, Direction::Input);
    assert_eq!(config.level, Level::Low);
    assert_eq!(config.pull, Pull::None);
    assert!(!config.open_drain);

    // Port：A..K，序号 A=0。
    assert_eq!(Port::A.index(), 0);
    assert_eq!(Port::K.index(), 10);
    assert_eq!(Port::from_ascii(b'j'), Some(Port::J));
    assert_eq!(Port::from_ascii(b'L'), None);
    assert_eq!(Port::C.to_string(), "C");

    // PinCaps 的 gpio.info 名称顺序。
    assert_eq!(PinCaps::ALL.names(), vec!["in", "out", "pu", "pd", "od"]);
    assert_eq!(PinCaps::INPUT_ONLY.names(), vec!["in"]);
    assert!(PinCaps::NONE.names().is_empty());

    // PinFlags 的 gpio.info 名称顺序（reserved → input_only → … → boot）。
    assert_eq!(
        pin(ChipFamily::Esp32, PinId::io(6)).flags.names(),
        vec!["reserved"]
    );
    assert_eq!(
        pin(ChipFamily::Esp32, PinId::io(34)).flags.names(),
        vec!["input_only"]
    );
    assert_eq!(
        pin(ChipFamily::Stm32F1, PinId::port(Port::A, 13))
            .flags
            .names(),
        vec!["reserved", "swd"]
    );
    assert_eq!(
        pin(ChipFamily::Stm32F1, PinId::port(Port::B, 2))
            .flags
            .names(),
        vec!["boot"]
    );
    assert!(PinFlags::NONE.is_empty());
}

// ---------------------------------------------------------------------------
// §4.3 配置合并九条规则
// ---------------------------------------------------------------------------

#[test]
fn config_rule1_default_is_safe_input() {
    let expected = PinConfig {
        direction: Direction::Input,
        level: Level::Low,
        pull: Pull::None,
        open_drain: false,
    };
    assert_eq!(PinConfig::default(), expected);

    let targets = [
        (ChipFamily::Esp32, PinId::io(4)),
        (ChipFamily::Stm32F1, PinId::port(Port::A, 5)),
        (ChipFamily::Stm32F4, PinId::port(Port::E, 0)),
    ];
    for (family, id) in targets {
        let resolved = resolve_config(pin(family, id), None, &ConfigRequest::new()).unwrap();
        assert_eq!(resolved, expected, "{} {id} 默认配置", family.chip());
    }

    // 规则 1：起始配置 = current.unwrap_or_default()；空请求不应改动当前配置。
    let current = PinConfig {
        direction: Direction::Output,
        level: Level::High,
        pull: Pull::None,
        open_drain: true,
    };
    let resolved = resolve_config(
        pin(ChipFamily::Esp32, PinId::io(4)),
        Some(current),
        &ConfigRequest::new(),
    )
    .unwrap();
    assert_eq!(resolved, current);
}

#[test]
fn config_rule2_direction_switch_normalizes_fields() {
    let io4 = pin(ChipFamily::Esp32, PinId::io(4));

    // 输出 → 输入：level=Low、open_drain=false 强制；pull 保留/应用。
    let output = PinConfig {
        direction: Direction::Output,
        level: Level::High,
        pull: Pull::None,
        open_drain: true,
    };
    let input = resolve_config(
        io4,
        Some(output),
        &request(Some(Direction::Input), None, Some(Pull::Down), None),
    )
    .unwrap();
    assert_eq!(
        input,
        PinConfig {
            direction: Direction::Input,
            level: Level::Low,
            pull: Pull::Down,
            open_drain: false,
        }
    );

    // 输入 → 输入：不传 pull 时保留当前上下拉。
    let input_pull = PinConfig {
        direction: Direction::Input,
        level: Level::Low,
        pull: Pull::Up,
        open_drain: false,
    };
    let resolved = resolve_config(
        io4,
        Some(input_pull),
        &request(Some(Direction::Input), None, None, None),
    )
    .unwrap();
    assert_eq!(resolved, input_pull);

    // 输入 → 输出：pull 强制 none，level/open_drain 应用。
    let output = resolve_config(
        io4,
        Some(input_pull),
        &request(Some(Direction::Output), Some(Level::High), None, Some(true)),
    )
    .unwrap();
    assert_eq!(
        output,
        PinConfig {
            direction: Direction::Output,
            level: Level::High,
            pull: Pull::None,
            open_drain: true,
        }
    );

    // 输出 → 输出：未给 level/open_drain 时沿用当前值。
    let resolved = resolve_config(
        io4,
        Some(output),
        &request(Some(Direction::Output), None, None, None),
    )
    .unwrap();
    assert_eq!(resolved, output);
}

#[test]
fn config_rule3_absent_direction_keeps_current_direction() {
    let io4 = pin(ChipFamily::Esp32, PinId::io(4));

    // 未配置 + 空请求：默认 Input。
    let resolved = resolve_config(io4, None, &ConfigRequest::new()).unwrap();
    assert_eq!(resolved.direction, Direction::Input);

    // 当前输出：只更新 level，方向保持输出。
    let current = PinConfig {
        direction: Direction::Output,
        level: Level::High,
        pull: Pull::None,
        open_drain: false,
    };
    let resolved = resolve_config(
        io4,
        Some(current),
        &request(None, Some(Level::Low), None, None),
    )
    .unwrap();
    assert_eq!(resolved.direction, Direction::Output);
    assert_eq!(resolved.level, Level::Low);

    // 当前输入：只更新 pull，方向保持输入。
    let current = PinConfig {
        direction: Direction::Input,
        level: Level::Low,
        pull: Pull::Up,
        open_drain: false,
    };
    let resolved = resolve_config(
        io4,
        Some(current),
        &request(None, None, Some(Pull::Down), None),
    )
    .unwrap();
    assert_eq!(resolved.direction, Direction::Input);
    assert_eq!(resolved.pull, Pull::Down);
    assert_eq!(resolved.level, Level::Low);
}

#[test]
fn config_rule4_level_requires_output_direction() {
    let io4 = pin(ChipFamily::Esp32, PinId::io(4));

    // 未配置（默认输入）给 level → 拒绝。
    let error =
        resolve_config(io4, None, &request(None, Some(Level::High), None, None)).unwrap_err();
    assert_code(&error, ErrorCode::InvalidArgument);
    assert!(matches!(&error, GpioError::InvalidArgument(_)));
    assert!(
        error.message().contains("输入模式") && error.message().contains("level"),
        "{}",
        error.message()
    );

    // 显式切输入同时给 level → 拒绝。
    let error = resolve_config(
        io4,
        None,
        &request(Some(Direction::Input), Some(Level::Low), None, None),
    )
    .unwrap_err();
    assert_eq!(error.code(), ErrorCode::InvalidArgument);

    // 已配置输入（无方向）给 level → 拒绝。
    let input = PinConfig::default();
    let error = resolve_config(
        io4,
        Some(input),
        &request(None, Some(Level::High), None, None),
    )
    .unwrap_err();
    assert_eq!(error.code(), ErrorCode::InvalidArgument);

    // 输出方向给 level → 接受。
    for current in [None, Some(input)] {
        let resolved = resolve_config(
            io4,
            current,
            &request(Some(Direction::Output), Some(Level::High), None, None),
        )
        .unwrap();
        assert_eq!(resolved.direction, Direction::Output);
        assert_eq!(resolved.level, Level::High);
    }

    // 已配置输出（无方向）给 level → 接受。
    let output = PinConfig {
        direction: Direction::Output,
        level: Level::Low,
        ..PinConfig::default()
    };
    let resolved = resolve_config(
        io4,
        Some(output),
        &request(None, Some(Level::High), None, None),
    )
    .unwrap();
    assert_eq!(resolved.direction, Direction::Output);
    assert_eq!(resolved.level, Level::High);
}

#[test]
fn config_rule5_pull_requires_input_direction() {
    let io4 = pin(ChipFamily::Esp32, PinId::io(4));

    // 未配置（默认输入）给 pull → 接受。
    let resolved = resolve_config(io4, None, &request(None, None, Some(Pull::Up), None)).unwrap();
    assert_eq!(resolved.direction, Direction::Input);
    assert_eq!(resolved.pull, Pull::Up);

    // 显式输入给 pull → 接受。
    let resolved = resolve_config(
        io4,
        None,
        &request(Some(Direction::Input), None, Some(Pull::Down), None),
    )
    .unwrap();
    assert_eq!(resolved.pull, Pull::Down);

    // 输出方向给 pull → 拒绝（含显式 pull=none：规则 5 是字面规则）。
    for pull in [Pull::Up, Pull::Down, Pull::None] {
        let error = resolve_config(
            io4,
            None,
            &request(Some(Direction::Output), None, Some(pull), None),
        )
        .unwrap_err();
        assert_code(&error, ErrorCode::InvalidArgument);
        assert!(matches!(&error, GpioError::InvalidArgument(_)));
        assert!(
            error.message().contains("输出模式") && error.message().contains("pull"),
            "{}",
            error.message()
        );
    }

    // 已配置输出（无方向）给 pull → 拒绝。
    let output = PinConfig {
        direction: Direction::Output,
        ..PinConfig::default()
    };
    let error = resolve_config(
        io4,
        Some(output),
        &request(None, None, Some(Pull::Up), None),
    )
    .unwrap_err();
    assert_eq!(error.code(), ErrorCode::InvalidArgument);
}

#[test]
fn config_rule6_open_drain_true_requires_output_direction() {
    let io4 = pin(ChipFamily::Esp32, PinId::io(4));

    // 未配置（默认输入）给 open_drain=true → 拒绝。
    let error = resolve_config(io4, None, &request(None, None, None, Some(true))).unwrap_err();
    assert_code(&error, ErrorCode::InvalidArgument);
    assert!(
        error.message().contains("输入模式") && error.message().contains("open_drain"),
        "{}",
        error.message()
    );

    // 显式输入 + open_drain=true → 拒绝。
    let error = resolve_config(
        io4,
        None,
        &request(Some(Direction::Input), None, None, Some(true)),
    )
    .unwrap_err();
    assert_eq!(error.code(), ErrorCode::InvalidArgument);

    // open_drain=false 在输入方向是合法的（no-op）。
    let resolved = resolve_config(io4, None, &request(None, None, None, Some(false))).unwrap();
    assert!(!resolved.open_drain);
    let resolved = resolve_config(
        io4,
        None,
        &request(Some(Direction::Input), None, None, Some(false)),
    )
    .unwrap();
    assert!(!resolved.open_drain);

    // 输出方向 + open_drain=true → 接受。
    let resolved = resolve_config(
        io4,
        None,
        &request(Some(Direction::Output), Some(Level::High), None, Some(true)),
    )
    .unwrap();
    assert!(resolved.open_drain);
    assert_eq!(resolved.level, Level::High);

    // 已配置输出（无方向）给 open_drain=true → 接受。
    let output = PinConfig {
        direction: Direction::Output,
        ..PinConfig::default()
    };
    let resolved =
        resolve_config(io4, Some(output), &request(None, None, None, Some(true))).unwrap();
    assert!(resolved.open_drain);

    // 已配置输入（无方向）给 open_drain=true → 拒绝。
    let input = PinConfig::default();
    let error =
        resolve_config(io4, Some(input), &request(None, None, None, Some(true))).unwrap_err();
    assert_eq!(error.code(), ErrorCode::InvalidArgument);
}

#[test]
fn config_rule7_capability_checks() {
    // 真实表：IO34 仅输入。
    let io34 = pin(ChipFamily::Esp32, PinId::io(34));
    let error = resolve_config(
        io34,
        None,
        &request(Some(Direction::Output), None, None, None),
    )
    .unwrap_err();
    assert_code(&error, ErrorCode::InvalidArgument);
    assert!(matches!(&error, GpioError::Unsupported(_)));
    assert_eq!(error.message(), "引脚 IO34 仅支持输入");

    for (pull, needle) in [(Pull::Up, "不支持上拉"), (Pull::Down, "不支持下拉")] {
        let error = resolve_config(
            io34,
            None,
            &request(Some(Direction::Input), None, Some(pull), None),
        )
        .unwrap_err();
        assert!(matches!(&error, GpioError::Unsupported(_)));
        assert!(error.message().contains(needle), "{}", error.message());
    }

    // 输入专用引脚仍然可以输入。
    assert!(
        resolve_config(
            io34,
            None,
            &request(Some(Direction::Input), None, None, None)
        )
        .is_ok()
    );

    // 合成表项覆盖真实表没有的能力组合。
    let input_only = leaked_spec(PinId::io(0), PinCaps::INPUT_ONLY, PinFlags::NONE);
    let error = resolve_config(
        input_only,
        None,
        &request(Some(Direction::Output), None, None, None),
    )
    .unwrap_err();
    assert!(matches!(&error, GpioError::Unsupported(_)));
    assert!(
        error.message().contains("不支持输出"),
        "{}",
        error.message()
    );

    let output_only = leaked_spec(
        PinId::io(1),
        PinCaps {
            input: false,
            output: true,
            pull_up: true,
            pull_down: true,
            open_drain: true,
        },
        PinFlags::NONE,
    );
    let error = resolve_config(
        output_only,
        None,
        &request(Some(Direction::Input), None, None, None),
    )
    .unwrap_err();
    assert!(matches!(&error, GpioError::Unsupported(_)));
    assert!(
        error.message().contains("不支持输入"),
        "{}",
        error.message()
    );

    let no_open_drain = leaked_spec(
        PinId::io(2),
        PinCaps {
            input: true,
            output: true,
            pull_up: false,
            pull_down: true,
            open_drain: false,
        },
        PinFlags::NONE,
    );
    let error = resolve_config(
        no_open_drain,
        None,
        &request(Some(Direction::Output), None, None, Some(true)),
    )
    .unwrap_err();
    assert!(matches!(&error, GpioError::Unsupported(_)));
    assert!(
        error.message().contains("不支持开漏"),
        "{}",
        error.message()
    );
    assert!(
        resolve_config(
            no_open_drain,
            None,
            &request(Some(Direction::Output), Some(Level::Low), None, Some(false)),
        )
        .is_ok()
    );
    // 支持下拉、不支持上拉。
    assert!(
        resolve_config(
            no_open_drain,
            None,
            &request(Some(Direction::Input), None, Some(Pull::Down), None),
        )
        .is_ok()
    );
    let error = resolve_config(
        no_open_drain,
        None,
        &request(Some(Direction::Input), None, Some(Pull::Up), None),
    )
    .unwrap_err();
    assert!(
        error.message().contains("不支持上拉"),
        "{}",
        error.message()
    );
}

#[test]
fn config_rule8_reserved_pins_are_rejected_first() {
    let io6 = pin(ChipFamily::Esp32, PinId::io(6));
    let error = resolve_config(io6, None, &ConfigRequest::new()).unwrap_err();
    assert!(matches!(&error, GpioError::ReservedPin(_)));
    assert_eq!(error.message(), "引脚 IO6 已保留（SPI flash）");
    assert_eq!(error.code(), ErrorCode::InvalidArgument);

    // 即使请求本身也非法，保留检查仍然优先（不触碰硬件）。
    let error =
        resolve_config(io6, None, &request(None, Some(Level::High), None, None)).unwrap_err();
    assert!(
        matches!(&error, GpioError::ReservedPin(_)),
        "保留检查应优先：{error:?}"
    );

    // 直接校验入口同样拒绝。
    assert!(matches!(
        ensure_not_reserved(io6),
        Err(GpioError::ReservedPin(_))
    ));
    assert!(ensure_not_reserved(pin(ChipFamily::Esp32, PinId::io(4))).is_ok());

    // STM32 两个家族的保留原因取自 note。
    for family in [ChipFamily::Stm32F1, ChipFamily::Stm32F4] {
        for id in [
            PinId::port(Port::A, 13),
            PinId::port(Port::A, 14),
            PinId::port(Port::A, 15),
            PinId::port(Port::B, 3),
            PinId::port(Port::B, 4),
        ] {
            let error = resolve_config(pin(family, id), None, &ConfigRequest::new()).unwrap_err();
            assert!(
                matches!(&error, GpioError::ReservedPin(_)),
                "{} {id} 应保留",
                family.chip()
            );
            assert!(error.message().contains("已保留"), "{}", error.message());
        }
    }
    let error = resolve_config(
        pin(ChipFamily::Stm32F4, PinId::port(Port::A, 13)),
        None,
        &ConfigRequest::new(),
    )
    .unwrap_err();
    assert!(error.message().contains("SWD"), "{}", error.message());
}

#[test]
fn config_rule9_error_variants_cover_unknown_reserved_and_capability() {
    // 引脚不存在 → UnknownPin（由 FamilySpec 判定，不属于配置合并本身）。
    let error = spec(ChipFamily::Esp32).resolve("IO20").unwrap_err();
    assert!(matches!(&error, GpioError::UnknownPin(_)));
    assert_eq!(error.code(), ErrorCode::InvalidArgument);

    // 保留 → ReservedPin。
    let error = resolve_config(
        pin(ChipFamily::Esp32, PinId::io(6)),
        None,
        &ConfigRequest::new(),
    )
    .unwrap_err();
    assert!(matches!(&error, GpioError::ReservedPin(_)));

    // 能力不足 → Unsupported。
    let error = resolve_config(
        pin(ChipFamily::Esp32, PinId::io(34)),
        None,
        &request(Some(Direction::Output), None, None, None),
    )
    .unwrap_err();
    assert!(matches!(&error, GpioError::Unsupported(_)));
}

#[test]
fn validate_config_is_defensive_for_all_backends() {
    let io6 = pin(ChipFamily::Esp32, PinId::io(6));
    assert!(matches!(
        validate_config(io6, &PinConfig::default()),
        Err(GpioError::ReservedPin(_))
    ));

    let io34 = pin(ChipFamily::Esp32, PinId::io(34));
    let output = PinConfig {
        direction: Direction::Output,
        ..PinConfig::default()
    };
    assert!(matches!(
        validate_config(io34, &output),
        Err(GpioError::Unsupported(_))
    ));

    let no_open_drain = leaked_spec(
        PinId::io(3),
        PinCaps {
            input: true,
            output: true,
            pull_up: true,
            pull_down: true,
            open_drain: false,
        },
        PinFlags::NONE,
    );
    let open_drain = PinConfig {
        direction: Direction::Output,
        open_drain: true,
        ..PinConfig::default()
    };
    assert!(matches!(
        validate_config(no_open_drain, &open_drain),
        Err(GpioError::Unsupported(_))
    ));

    // 正常引脚通过。
    assert!(validate_config(pin(ChipFamily::Esp32, PinId::io(4)), &output).is_ok());
    assert!(validate_config(pin(ChipFamily::Stm32F4, PinId::port(Port::A, 5)), &output).is_ok());
}

// ---------------------------------------------------------------------------
// §5.9 错误码映射
// ---------------------------------------------------------------------------

#[test]
fn error_codes_and_messages_match_section_5_9() {
    let io4 = pin(ChipFamily::Esp32, PinId::io(4));
    let input = PinConfig::default();
    let output = PinConfig {
        direction: Direction::Output,
        ..PinConfig::default()
    };

    let cases: [(GpioError, &str); 8] = [
        (GpioError::invalid("参数 foo 非法"), "参数 foo 非法"),
        (GpioError::unknown_pin(PinId::io(40)), "引脚 IO40 不存在"),
        (GpioError::unknown_pin_named("IO40"), "引脚 IO40 不存在"),
        (
            GpioError::reserved_pin(pin(ChipFamily::Esp32, PinId::io(6))),
            "引脚 IO6 已保留（SPI flash）",
        ),
        (
            GpioError::unsupported("引脚 IO34 仅支持输入"),
            "引脚 IO34 仅支持输入",
        ),
        (GpioError::not_configured(PinId::io(4)), "未配置"),
        (
            GpioError::wrong_mode_read(PinId::io(4), output),
            "引脚 IO4 处于输出模式，无法读取",
        ),
        (
            GpioError::wrong_mode_write(PinId::io(4), input),
            "引脚 IO4 处于输入模式，无法写入",
        ),
    ];
    for (error, needle) in cases {
        assert_code(&error, ErrorCode::InvalidArgument);
        assert!(
            error.message().contains(needle),
            "消息 {:?} 应包含 {needle:?}",
            error.message()
        );
        assert_eq!(error.to_string(), error.message());
    }

    // 后端内部错误 → Internal（§5.9 “GPIO 后端错误：…”）。
    let backend = GpioError::backend("GPIO 后端错误：寄存器访问失败");
    assert_eq!(backend.code(), ErrorCode::Internal);
    assert_eq!(backend.message(), "GPIO 后端错误：寄存器访问失败");

    // 枚举每个变体的 code()：除 Backend 外全部 InvalidArgument，
    // 且任何 GPIO 错误都不映射到协议保留的 Unsupported。
    let variants = [
        GpioError::UnknownPin("a".into()),
        GpioError::ReservedPin("b".into()),
        GpioError::NotConfigured("c".into()),
        GpioError::WrongMode("d".into()),
        GpioError::Unsupported("e".into()),
        GpioError::InvalidArgument("f".into()),
    ];
    for error in &variants {
        assert_eq!(error.code(), ErrorCode::InvalidArgument, "{error:?}");
        assert!(
            !matches!(error.code(), ErrorCode::Unsupported),
            "{error:?} 不应映射到协议保留错误码"
        );
        assert_eq!(error.to_string(), error.message());
    }

    // 映射到 CommandError 时保留错误码与消息。
    let command: CommandError = GpioError::wrong_mode_read(PinId::io(4), output).into();
    assert_eq!(command.code, ErrorCode::InvalidArgument);
    assert!(command.message.contains("输出模式"), "{}", command.message);
    let internal: CommandError = GpioError::backend("GPIO 后端错误：x").into();
    assert_eq!(internal.code, ErrorCode::Internal);
    assert_eq!(internal.message, "GPIO 后端错误：x");

    // 保留引脚的消息包含引脚与原因。
    let reserved = GpioError::reserved_pin(pin(ChipFamily::Esp32, PinId::io(6)));
    assert!(reserved.message().contains("IO6"));
    assert!(reserved.message().contains("SPI flash"));

    // 未配置/模式不匹配的消息提示修复方式。
    let error = GpioError::not_configured(io4.id);
    assert!(error.message().contains("gpio.config IO4 in"));
}

#[test]
fn tool_text_limits_match_section_5_9() {
    use sekai_gpio::text::{MAX_ARGS_BYTES, MAX_RESULT_BYTES, args_text, finish_result};

    let exact = vec![b'a'; MAX_ARGS_BYTES];
    assert!(args_text(&exact).is_ok());
    let too_long = vec![b'a'; MAX_ARGS_BYTES + 1];
    let error = args_text(&too_long).unwrap_err();
    assert_code(&error, ErrorCode::InvalidArgument);
    assert!(error.message().contains("256"), "{}", error.message());
    let error = args_text(&[0xff, 0xfe]).unwrap_err();
    assert_code(&error, ErrorCode::InvalidArgument);
    assert!(error.message().contains("UTF-8"), "{}", error.message());

    let exact = vec![b'x'; MAX_RESULT_BYTES];
    assert!(finish_result(exact).is_ok());
    let too_large = vec![b'x'; MAX_RESULT_BYTES + 1];
    let error = finish_result(too_large).unwrap_err();
    assert_eq!(error.code, ErrorCode::ResultTooLarge);
    assert_eq!(error.message, "GPIO 结果过大");
}
