#![deny(unsafe_code)]
//! 虚拟后端的端到端测试（计划 §4.6/§13）。
//!
//! 虚拟后端是 AI 在宿主机上学习 GPIO 语义的唯一入口，因此这里按真实硬件的
//! 契约逐项验证：未配置读写、模式错配、保留/仅输入、外部电平、configured_count、
//! 跨家族引脚与配置状态机。所有断言都通过 GpioPort trait + 公开辅助函数进行，
//! 不依赖内部字段。

use sekai::sekai_wire::ErrorCode;
use sekai_gpio::config::{ConfigRequest, Direction, Level, PinConfig, Pull, resolve_config};
use sekai_gpio::family::ChipFamily;
use sekai_gpio::pin::{PinId, Port};
use sekai_gpio::port::{BackendKind, GpioError, GpioPort};
use sekai_gpio::virtual_gpio::VirtualGpio;

/// 当前 crate 提供的全部芯片家族（ESP32/F1/F4；虚拟后端对每个家族都可用）。
const FAMILIES: [ChipFamily; 3] = [ChipFamily::Esp32, ChipFamily::Stm32F1, ChipFamily::Stm32F4];

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

/// 模拟工具层：先 describe 取当前配置，再 resolve_config 合并校验，最后 configure。
fn apply(
    port: &mut VirtualGpio,
    pin: PinId,
    request: ConfigRequest,
) -> Result<PinConfig, GpioError> {
    let current = port.describe(pin)?.config;
    let spec = port
        .spec()
        .by_id(pin)
        .expect("describe 已确认引脚属于当前家族");
    let config = resolve_config(spec, current, &request)?;
    port.configure(pin, config)?;
    Ok(config)
}

/// 每个家族选一组代表引脚：(可输出, 可输入, 保留)。
fn representatives(family: ChipFamily) -> (PinId, PinId, PinId) {
    match family {
        ChipFamily::Esp32 => (PinId::io(4), PinId::io(34), PinId::io(6)),
        ChipFamily::Stm32F1 => (
            PinId::port(Port::A, 5),
            PinId::port(Port::C, 13),
            PinId::port(Port::A, 13),
        ),
        ChipFamily::Stm32F4 => (
            PinId::port(Port::E, 0),
            PinId::port(Port::A, 0),
            PinId::port(Port::A, 13),
        ),
    }
}

fn assert_invalid_argument(error: &GpioError) {
    assert_eq!(
        error.code(),
        ErrorCode::InvalidArgument,
        "错误应为 InvalidArgument：{error:?}"
    );
}

fn assert_error_contains(error: &GpioError, needles: &[&str]) {
    assert_invalid_argument(error);
    for needle in needles {
        assert!(
            error.message().contains(needle),
            "错误消息 {:?} 应包含 {needle:?}",
            error.message()
        );
    }
}

// ---------------------------------------------------------------------------
// 后端身份与家族作用域
// ---------------------------------------------------------------------------

#[test]
fn virtual_backend_identity_for_every_family() {
    for family in FAMILIES {
        let port = VirtualGpio::new(family);
        assert_eq!(port.backend(), BackendKind::Virtual);
        assert_eq!(port.backend().as_str(), "virtual");
        assert_eq!(port.family(), family);
        assert_eq!(port.spec().chip, family.chip());
        assert_eq!(port.spec().pins.len(), port.spec().pin_count());
        assert_eq!(port.configured_count(), 0);
    }
}

#[test]
fn cross_family_pins_are_unknown() {
    // ESP32 端口不认识 STM32 引脚。
    let mut esp32 = VirtualGpio::new(ChipFamily::Esp32);
    for pin in [
        PinId::port(Port::A, 5),
        PinId::port(Port::C, 0),
        PinId::port(Port::J, 0),
    ] {
        let error = esp32.describe(pin).unwrap_err();
        assert!(
            matches!(&error, GpioError::UnknownPin(_)),
            "{pin} 应为 UnknownPin：{error:?}"
        );
        assert_error_contains(&error, &["不存在"]);
        let error = apply(&mut esp32, pin, ConfigRequest::new()).unwrap_err();
        assert!(matches!(&error, GpioError::UnknownPin(_)));
    }

    // F1 表外：PC0..PC12、D/E/J/K 端口；ESP32 引脚也不认识。
    let mut f1 = VirtualGpio::new(ChipFamily::Stm32F1);
    for pin in [
        PinId::io(4),
        PinId::port(Port::C, 0),
        PinId::port(Port::D, 0),
        PinId::port(Port::E, 0),
        PinId::port(Port::J, 0),
    ] {
        let error = f1.describe(pin).unwrap_err();
        assert!(matches!(&error, GpioError::UnknownPin(_)), "{pin}");
        assert_invalid_argument(&error);
    }
    assert!(f1.describe(PinId::port(Port::A, 5)).is_ok());
    assert!(f1.describe(PinId::port(Port::C, 13)).is_ok());

    // F4 有 PC0/PE15，但没有 F..K 端口。
    let mut f4 = VirtualGpio::new(ChipFamily::Stm32F4);
    assert!(f4.describe(PinId::port(Port::C, 0)).is_ok());
    assert!(f4.describe(PinId::port(Port::E, 15)).is_ok());
    for pin in [
        PinId::io(4),
        PinId::port(Port::F, 0),
        PinId::port(Port::G, 0),
        PinId::port(Port::J, 0),
    ] {
        let error = f4.describe(pin).unwrap_err();
        assert!(matches!(&error, GpioError::UnknownPin(_)), "{pin}");
    }
}

// ---------------------------------------------------------------------------
// 未配置 / 模式错配
// ---------------------------------------------------------------------------

#[test]
fn unconfigured_pins_reject_read_write_but_describe() {
    for family in FAMILIES {
        let (output_pin, input_pin, _) = representatives(family);
        let mut port = VirtualGpio::new(family);

        for pin in [output_pin, input_pin] {
            let error = port.read(pin).unwrap_err();
            assert_invalid_argument(&error);
            assert!(
                matches!(&error, GpioError::NotConfigured(_)),
                "{pin} 未配置 read 应为 NotConfigured：{error:?}"
            );
            assert_error_contains(&error, &["未配置", "gpio.config"]);

            let error = port.write(pin, Level::High).unwrap_err();
            assert_invalid_argument(&error);
            assert!(matches!(&error, GpioError::NotConfigured(_)));

            // describe 未配置也允许：gpio.info 需要它。
            let description = port.describe(pin).unwrap();
            assert_eq!(description.spec.id, pin);
            assert_eq!(description.config, None);
            assert_eq!(description.level, None);
        }
        assert_eq!(port.configured_count(), 0);
    }
}

#[test]
fn mode_mismatch_errors_are_invalid_argument() {
    let mut port = VirtualGpio::new(ChipFamily::Esp32);

    apply(
        &mut port,
        PinId::io(4),
        request(Some(Direction::Output), Some(Level::Low), None, None),
    )
    .unwrap();
    let error = port.read(PinId::io(4)).unwrap_err();
    assert!(matches!(&error, GpioError::WrongMode(_)));
    assert_error_contains(&error, &["输出模式", "无法读取", "gpio.config IO4 in"]);

    apply(
        &mut port,
        PinId::io(5),
        request(Some(Direction::Input), None, None, None),
    )
    .unwrap();
    let error = port.write(PinId::io(5), Level::High).unwrap_err();
    assert!(matches!(&error, GpioError::WrongMode(_)));
    assert_error_contains(&error, &["输入模式", "无法写入", "gpio.config IO5 out"]);
}

// ---------------------------------------------------------------------------
// 输出锁存 / 输入外部电平
// ---------------------------------------------------------------------------

#[test]
fn output_write_updates_latch_and_describe() {
    let mut port = VirtualGpio::new(ChipFamily::Esp32);
    let config = apply(
        &mut port,
        PinId::io(4),
        request(Some(Direction::Output), Some(Level::Low), None, None),
    )
    .unwrap();
    assert_eq!(config.direction, Direction::Output);
    assert_eq!(port.describe(PinId::io(4)).unwrap().level, Some(Level::Low));

    port.write(PinId::io(4), Level::High).unwrap();
    let description = port.describe(PinId::io(4)).unwrap();
    assert_eq!(description.config.unwrap().direction, Direction::Output);
    assert_eq!(description.level, Some(Level::High));

    port.write(PinId::io(4), Level::Low).unwrap();
    assert_eq!(port.describe(PinId::io(4)).unwrap().level, Some(Level::Low));
    assert_eq!(port.configured_count(), 1);
}

#[test]
fn input_reads_external_level_and_defaults_low() {
    let mut port = VirtualGpio::new(ChipFamily::Esp32);
    // 未配置引脚的外部电平默认 Low。
    assert_eq!(port.external_level(PinId::io(4)).unwrap(), Level::Low);

    apply(
        &mut port,
        PinId::io(4),
        request(Some(Direction::Input), None, None, None),
    )
    .unwrap();
    assert_eq!(port.read(PinId::io(4)).unwrap(), Level::Low);
    assert_eq!(port.describe(PinId::io(4)).unwrap().level, Some(Level::Low));

    port.set_external_level(PinId::io(4), Level::High).unwrap();
    assert_eq!(port.read(PinId::io(4)).unwrap(), Level::High);
    assert_eq!(
        port.describe(PinId::io(4)).unwrap().level,
        Some(Level::High)
    );

    port.set_external_level(PinId::io(4), Level::Low).unwrap();
    assert_eq!(port.read(PinId::io(4)).unwrap(), Level::Low);
}

#[test]
fn external_level_semantics_in_output_mode() {
    let mut port = VirtualGpio::new(ChipFamily::Esp32);
    apply(
        &mut port,
        PinId::io(4),
        request(Some(Direction::Output), Some(Level::High), None, None),
    )
    .unwrap();
    port.write(PinId::io(4), Level::High).unwrap();

    // 输出模式下外部电平仍可设置（模拟外部世界），但不影响 describe/read。
    port.set_external_level(PinId::io(4), Level::Low).unwrap();
    assert_eq!(port.external_level(PinId::io(4)).unwrap(), Level::Low);
    assert_eq!(
        port.describe(PinId::io(4)).unwrap().level,
        Some(Level::High)
    );
    let error = port.read(PinId::io(4)).unwrap_err();
    assert!(matches!(&error, GpioError::WrongMode(_)));

    // write 只更新锁存，不改外部电平。
    port.write(PinId::io(4), Level::Low).unwrap();
    assert_eq!(port.describe(PinId::io(4)).unwrap().level, Some(Level::Low));
    assert_eq!(port.external_level(PinId::io(4)).unwrap(), Level::Low);

    // 切回输入后读到保存的外部电平。
    apply(
        &mut port,
        PinId::io(4),
        request(Some(Direction::Input), None, None, None),
    )
    .unwrap();
    assert_eq!(port.read(PinId::io(4)).unwrap(), Level::Low);
    port.set_external_level(PinId::io(4), Level::High).unwrap();
    assert_eq!(port.read(PinId::io(4)).unwrap(), Level::High);

    // 再次切输出：describe 回到锁存语义，外部电平被忽略。
    apply(
        &mut port,
        PinId::io(4),
        request(Some(Direction::Output), Some(Level::High), None, None),
    )
    .unwrap();
    assert_eq!(
        port.describe(PinId::io(4)).unwrap().level,
        Some(Level::High)
    );
}

#[test]
fn output_latch_after_mode_switch() {
    let mut port = VirtualGpio::new(ChipFamily::Esp32);
    apply(
        &mut port,
        PinId::io(4),
        request(Some(Direction::Output), Some(Level::High), None, None),
    )
    .unwrap();
    port.write(PinId::io(4), Level::Low).unwrap();
    assert_eq!(port.describe(PinId::io(4)).unwrap().level, Some(Level::Low));

    // 输出 → 输入：输入模式读外部电平（默认 Low），锁存值不可见。
    apply(
        &mut port,
        PinId::io(4),
        request(Some(Direction::Input), None, None, None),
    )
    .unwrap();
    assert_eq!(port.describe(PinId::io(4)).unwrap().level, Some(Level::Low));

    // 输入 → 输出：未给 level 时沿用输入配置的 level（恒 Low）。
    let config = apply(
        &mut port,
        PinId::io(4),
        request(Some(Direction::Output), None, None, None),
    )
    .unwrap();
    assert_eq!(config.level, Level::Low);
    assert_eq!(port.describe(PinId::io(4)).unwrap().level, Some(Level::Low));

    // 显式给 level 时立即反映到 describe。
    apply(
        &mut port,
        PinId::io(4),
        request(Some(Direction::Output), Some(Level::High), None, None),
    )
    .unwrap();
    assert_eq!(
        port.describe(PinId::io(4)).unwrap().level,
        Some(Level::High)
    );
}

#[test]
fn pull_and_open_drain_round_trip_through_describe() {
    let mut port = VirtualGpio::new(ChipFamily::Esp32);

    // 输入 + 上拉。
    let config = apply(
        &mut port,
        PinId::io(4),
        request(Some(Direction::Input), None, Some(Pull::Up), None),
    )
    .unwrap();
    assert_eq!(config.pull, Pull::Up);
    let description = port.describe(PinId::io(4)).unwrap();
    assert_eq!(description.config.unwrap().pull, Pull::Up);

    // 切换到输出：pull 被清除，open_drain 生效。
    let config = apply(
        &mut port,
        PinId::io(4),
        request(Some(Direction::Output), Some(Level::High), None, Some(true)),
    )
    .unwrap();
    assert_eq!(config.pull, Pull::None);
    assert!(config.open_drain);
    let description = port.describe(PinId::io(4)).unwrap();
    assert_eq!(description.config.unwrap().pull, Pull::None);
    assert!(description.config.unwrap().open_drain);

    // 切回输入：open_drain 被清除。
    let config = apply(
        &mut port,
        PinId::io(4),
        request(Some(Direction::Input), None, None, None),
    )
    .unwrap();
    assert!(!config.open_drain);
    assert!(
        !port
            .describe(PinId::io(4))
            .unwrap()
            .config
            .unwrap()
            .open_drain
    );

    // STM32 家族同样支持开漏输出。
    let mut f4 = VirtualGpio::new(ChipFamily::Stm32F4);
    let config = apply(
        &mut f4,
        PinId::port(Port::E, 0),
        request(Some(Direction::Output), Some(Level::High), None, Some(true)),
    )
    .unwrap();
    assert!(config.open_drain);
}

// ---------------------------------------------------------------------------
// 保留 / 仅输入 / 外部电平校验
// ---------------------------------------------------------------------------

#[test]
fn reserved_pins_are_rejected_everywhere_but_describable() {
    let mut port = VirtualGpio::new(ChipFamily::Esp32);
    let io6 = PinId::io(6);

    let error = port
        .configure(
            io6,
            PinConfig {
                direction: Direction::Output,
                ..PinConfig::default()
            },
        )
        .unwrap_err();
    assert!(matches!(&error, GpioError::ReservedPin(_)));
    assert_error_contains(&error, &["IO6", "已保留", "SPI flash"]);

    for error in [
        port.read(io6).unwrap_err(),
        port.write(io6, Level::High).unwrap_err(),
        port.set_external_level(io6, Level::High).unwrap_err(),
    ] {
        assert!(matches!(&error, GpioError::ReservedPin(_)), "{error:?}");
        assert_invalid_argument(&error);
        assert!(error.message().contains("IO6"));
    }

    // gpio.info 详情例外：describe 允许看到保留原因。
    let description = port.describe(io6).unwrap();
    assert!(description.spec.flags.reserved);
    assert_eq!(description.spec.note, "SPI flash");
    assert_eq!(description.config, None);
    assert_eq!(description.level, None);
    // 失败操作没有留下任何配置。
    assert_eq!(port.configured_count(), 0);

    // STM32 保留引脚同样拒绝，原因来自 note。
    let mut f1 = VirtualGpio::new(ChipFamily::Stm32F1);
    let pa13 = PinId::port(Port::A, 13);
    let error = apply(&mut f1, pa13, ConfigRequest::new()).unwrap_err();
    assert!(matches!(&error, GpioError::ReservedPin(_)));
    assert_error_contains(&error, &["PA13", "SWD"]);
}

#[test]
fn input_only_pins_cannot_output_or_pull() {
    let mut port = VirtualGpio::new(ChipFamily::Esp32);
    let io34 = PinId::io(34);

    let error = port
        .configure(
            io34,
            PinConfig {
                direction: Direction::Output,
                ..PinConfig::default()
            },
        )
        .unwrap_err();
    assert!(matches!(&error, GpioError::Unsupported(_)));
    assert_eq!(error.message(), "引脚 IO34 仅支持输入");
    assert_invalid_argument(&error);
    assert_eq!(port.configured_count(), 0);

    // 上拉/下拉能力也不存在。
    for (pull, needle) in [(Pull::Up, "不支持上拉"), (Pull::Down, "不支持下拉")] {
        let error = apply(
            &mut port,
            io34,
            request(Some(Direction::Input), None, Some(pull), None),
        )
        .unwrap_err();
        assert!(matches!(&error, GpioError::Unsupported(_)));
        assert!(error.message().contains(needle), "{}", error.message());
    }

    // 未配置时 write 是 NotConfigured（不是 Unsupported）。
    assert!(matches!(
        port.write(io34, Level::High),
        Err(GpioError::NotConfigured(_))
    ));

    // 输入是允许的：外部电平照常读取。
    apply(
        &mut port,
        io34,
        request(Some(Direction::Input), None, None, None),
    )
    .unwrap();
    port.set_external_level(io34, Level::High).unwrap();
    assert_eq!(port.read(io34).unwrap(), Level::High);
    let error = port.write(io34, Level::Low).unwrap_err();
    assert!(matches!(&error, GpioError::WrongMode(_)));
}

#[test]
fn set_external_level_validates_pin() {
    let mut port = VirtualGpio::new(ChipFamily::Esp32);

    let error = port
        .set_external_level(PinId::io(20), Level::High)
        .unwrap_err();
    assert!(matches!(&error, GpioError::UnknownPin(_)));
    assert_error_contains(&error, &["IO20", "不存在"]);

    let error = port
        .set_external_level(PinId::port(Port::A, 5), Level::High)
        .unwrap_err();
    assert!(matches!(&error, GpioError::UnknownPin(_)));

    let error = port
        .set_external_level(PinId::io(6), Level::High)
        .unwrap_err();
    assert!(matches!(&error, GpioError::ReservedPin(_)));

    // 输入专用引脚可以设置外部电平（这正是它的用途）。
    port.set_external_level(PinId::io(34), Level::High).unwrap();
    assert_eq!(port.external_level(PinId::io(34)).unwrap(), Level::High);
    assert_eq!(port.external_level(PinId::io(4)).unwrap(), Level::Low);
}

// ---------------------------------------------------------------------------
// configured_count 与四/三家族矩阵
// ---------------------------------------------------------------------------

#[test]
fn configured_count_tracks_configured_pins_only() {
    let mut port = VirtualGpio::new(ChipFamily::Esp32);
    apply(
        &mut port,
        PinId::io(4),
        request(Some(Direction::Input), None, None, None),
    )
    .unwrap();
    assert_eq!(port.configured_count(), 1);
    apply(
        &mut port,
        PinId::io(5),
        request(Some(Direction::Input), None, None, None),
    )
    .unwrap();
    assert_eq!(port.configured_count(), 2);

    // 重复配置同一引脚不增加计数。
    apply(
        &mut port,
        PinId::io(4),
        request(Some(Direction::Output), None, None, None),
    )
    .unwrap();
    assert_eq!(port.configured_count(), 2);

    // 失败的操作不增加计数。
    assert!(
        apply(&mut port, PinId::io(6), ConfigRequest::new()).is_err(),
        "保留引脚应失败"
    );
    assert!(
        apply(&mut port, PinId::io(20), ConfigRequest::new()).is_err(),
        "不存在引脚应失败"
    );
    assert_eq!(port.configured_count(), 2);

    // 不同端口各自独立计数。
    let mut f1 = VirtualGpio::new(ChipFamily::Stm32F1);
    assert_eq!(f1.configured_count(), 0);
    apply(
        &mut f1,
        PinId::port(Port::A, 5),
        request(Some(Direction::Output), None, None, None),
    )
    .unwrap();
    assert_eq!(f1.configured_count(), 1);
}

/// 计划 §12.2 的家族矩阵：每个芯片家族都用虚拟后端跑一遍完整场景。
#[test]
fn family_matrix_end_to_end() {
    for family in FAMILIES {
        let (output_pin, input_pin, reserved_pin) = representatives(family);
        let mut port = VirtualGpio::new(family);
        assert_eq!(port.backend(), BackendKind::Virtual);
        assert_eq!(port.spec().chip, family.chip());
        assert_eq!(port.configured_count(), 0);

        // 输出：configure → write → describe。
        let config = apply(
            &mut port,
            output_pin,
            request(Some(Direction::Output), Some(Level::Low), None, None),
        )
        .unwrap();
        assert_eq!(config.direction, Direction::Output);
        assert_eq!(config.level, Level::Low);
        port.write(output_pin, Level::High).unwrap();
        let description = port.describe(output_pin).unwrap();
        assert_eq!(description.config.unwrap().direction, Direction::Output);
        assert_eq!(description.level, Some(Level::High));
        let error = port.read(output_pin).unwrap_err();
        assert_error_contains(&error, &["输出模式", "无法读取"]);
        assert_eq!(port.configured_count(), 1);

        // 输入：外部电平可以被读取。
        apply(
            &mut port,
            input_pin,
            request(Some(Direction::Input), None, None, None),
        )
        .unwrap();
        assert_eq!(port.read(input_pin).unwrap(), Level::Low);
        port.set_external_level(input_pin, Level::High).unwrap();
        assert_eq!(port.read(input_pin).unwrap(), Level::High);
        let error = port.write(input_pin, Level::Low).unwrap_err();
        assert_error_contains(&error, &["输入模式", "无法写入"]);
        assert_eq!(port.configured_count(), 2);

        // 保留引脚：拒绝且不改变状态。
        let error = apply(&mut port, reserved_pin, ConfigRequest::new()).unwrap_err();
        assert!(
            matches!(&error, GpioError::ReservedPin(_)),
            "{} {reserved_pin} 应保留：{error:?}",
            family.chip()
        );
        assert_eq!(error.code(), ErrorCode::InvalidArgument);
        assert_eq!(port.configured_count(), 2);

        // 跨家族引脚：不存在。
        let foreign = match family {
            ChipFamily::Esp32 => PinId::port(Port::A, 5),
            ChipFamily::Stm32F1 | ChipFamily::Stm32F4 => PinId::io(4),
        };
        let error = port.describe(foreign).unwrap_err();
        assert!(matches!(&error, GpioError::UnknownPin(_)));
        assert_error_contains(&error, &["不存在"]);
    }
}

#[test]
fn direct_configure_keeps_hardware_parity() {
    // GpioPort::configure 的契约是“配置已经过 resolve_config 校验”，
    // 但后端仍必须做防御性能力校验；失败时绝不修改状态。
    let mut port = VirtualGpio::new(ChipFamily::Esp32);

    let error = port
        .configure(PinId::io(6), PinConfig::default())
        .unwrap_err();
    assert!(matches!(&error, GpioError::ReservedPin(_)));
    assert_eq!(port.configured_count(), 0);

    let output = PinConfig {
        direction: Direction::Output,
        ..PinConfig::default()
    };
    let error = port.configure(PinId::io(34), output).unwrap_err();
    assert!(matches!(&error, GpioError::Unsupported(_)));
    assert_eq!(port.configured_count(), 0);

    let error = port
        .configure(PinId::io(20), PinConfig::default())
        .unwrap_err();
    assert!(matches!(&error, GpioError::UnknownPin(_)));

    assert!(port.configure(PinId::io(4), PinConfig::default()).is_ok());
    assert_eq!(port.configured_count(), 1);
}
