//! AI 可直接调用的 `gpio.*` 用户工具。
//!
//! 工具参数是 UTF-8 文本（最多 256 字节），结果也是 UTF-8 文本（最多 1024 字节）。
//! 语法与结果格式见实施计划 §5：
//!
//! - 分词只按 ASCII 空白（空格/Tab）切分，不支持引号与转义；
//! - 键/值大小写不敏感，引脚名规范化输出（`IO4` / `PA5`）；
//! - 多行结果用换行分隔且末尾不带换行；
//! - `config`/`read`/`write` 的结果是单行空格分隔的 `key=value`；
//! - `info help` 是唯一非 `key=value` 的结果，固定自描述文本。
//!
//! 四个工具共享同一个端口句柄：默认 `Rc<RefCell<P>>`（宿主单线程），
//! `send` 特性下改为 `Arc<critical_section::Mutex<RefCell<P>>>`，
//! 使处理器可以跨线程移动、供 Embassy 异步运行时持有。

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use sekai::sekai_tool::{RegistryError, ToolContext, ToolHandler, ToolRegistry};
use sekai::sekai_wire::{CommandError, ErrorCode};

use crate::config::{ConfigRequest, Level, PinConfig, resolve_config};
use crate::family::{FamilySpec, PinCaps, PinFlags, PinSpec};
use crate::pin::{PinId, Port};
use crate::port::{GpioError, GpioPort};
use crate::text;

/// 本 crate 注册的全部工具名（按名称升序，与注册表遍历顺序一致）。
pub const GPIO_TOOL_NAMES: [&str; 4] = ["gpio.config", "gpio.info", "gpio.read", "gpio.write"];

/// `gpio.info help` 的固定自描述文本；末尾不带换行。
///
/// 该文本是给 AI 的使用说明，属于对外契约的一部分，改动需同步更新文档与测试。
pub const INFO_HELP: &str = "gpio tools v1\n\
info [help|PIN|port=A]\n\
config PIN [in|out] [level=0|1] [pull=none|up|down] [open_drain=0|1]\n\
read PIN\n\
write PIN 0|1\n\
pins: IO4 / PA5\n\
read requires input mode; write requires output mode";

/// 工具共享的端口句柄。
///
/// 默认构建用 `Rc<RefCell<P>>`（宿主单线程）；`send` 特性下用
/// `Arc<critical_section::Mutex<RefCell<P>>>`，使工具处理器可以跨线程移动。
#[cfg(not(feature = "send"))]
pub(crate) type SharedPort<P> = alloc::rc::Rc<core::cell::RefCell<P>>;

/// `send` 特性下的工具共享句柄。
#[cfg(feature = "send")]
pub(crate) type SharedPort<P> = alloc::sync::Arc<critical_section::Mutex<core::cell::RefCell<P>>>;

/// 共享句柄借用失败时的统一错误（计划 §5.9：`Internal` / `GPIO 被占用`）。
fn busy_error() -> CommandError {
    CommandError::new(ErrorCode::Internal, "GPIO 被占用")
}

/// 借出端口并执行一次工具逻辑。
///
/// 默认构建用 `RefCell::try_borrow_mut`；`send` 特性下用
/// `critical_section::with` 关中断后借出。两种构建下借用失败都映射为
/// `Internal("GPIO 被占用")`。
#[cfg(not(feature = "send"))]
fn with_port<P, R>(
    port: &SharedPort<P>,
    f: impl FnOnce(&mut P) -> Result<R, GpioError>,
) -> Result<R, CommandError>
where
    P: GpioPort,
{
    let mut state = port.try_borrow_mut().map_err(|_| busy_error())?;
    f(&mut state).map_err(CommandError::from)
}

/// 借出端口并执行一次工具逻辑；`send` 特性版本。
#[cfg(feature = "send")]
fn with_port<P, R>(
    port: &SharedPort<P>,
    f: impl FnOnce(&mut P) -> Result<R, GpioError>,
) -> Result<R, CommandError>
where
    P: GpioPort,
{
    critical_section::with(|cs| {
        let cell = port.borrow(cs);
        match cell.try_borrow_mut() {
            Ok(mut state) => f(&mut state).map_err(CommandError::from),
            Err(_) => Err(busy_error()),
        }
    })
}

/// `gpio.info` 处理器。
struct InfoTool<P> {
    shared: SharedPort<P>,
}

/// `gpio.config` 处理器。
struct ConfigTool<P> {
    shared: SharedPort<P>,
}

/// `gpio.read` 处理器。
struct ReadTool<P> {
    shared: SharedPort<P>,
}

/// `gpio.write` 处理器。
struct WriteTool<P> {
    shared: SharedPort<P>,
}

impl<P: GpioPort> ToolHandler for InfoTool<P> {
    fn call(&mut self, ctx: &mut ToolContext<'_>) -> Result<Vec<u8>, CommandError> {
        let args = text::args_text(ctx.args)?;
        let result = with_port(&self.shared, |port| info(port, args))?;
        text::finish_result(result)
    }
}

impl<P: GpioPort> ToolHandler for ConfigTool<P> {
    fn call(&mut self, ctx: &mut ToolContext<'_>) -> Result<Vec<u8>, CommandError> {
        let args = text::args_text(ctx.args)?;
        let result = with_port(&self.shared, |port| config(port, args))?;
        text::finish_result(result)
    }
}

impl<P: GpioPort> ToolHandler for ReadTool<P> {
    fn call(&mut self, ctx: &mut ToolContext<'_>) -> Result<Vec<u8>, CommandError> {
        let args = text::args_text(ctx.args)?;
        let result = with_port(&self.shared, |port| read(port, args))?;
        text::finish_result(result)
    }
}

impl<P: GpioPort> ToolHandler for WriteTool<P> {
    fn call(&mut self, ctx: &mut ToolContext<'_>) -> Result<Vec<u8>, CommandError> {
        let args = text::args_text(ctx.args)?;
        let result = with_port(&self.shared, |port| write(port, args))?;
        text::finish_result(result)
    }
}

/// 追加一行；行间用换行分隔，不产生末尾换行。
fn push_line(out: &mut String, line: &str) {
    if !out.is_empty() {
        out.push('\n');
    }
    out.push_str(line);
}

/// `caps=` 文本；无能力时输出 `none`。
fn caps_text(caps: PinCaps) -> String {
    text::join_or_none(&caps.names())
}

/// `flags=` 文本；无标志时输出 `none`。
fn flags_text(flags: PinFlags) -> String {
    text::join_or_none(&flags.names())
}

/// `gpio.info`：help 文本、家族摘要、单引脚详情、STM32 端口列表。
fn info<P: GpioPort>(port: &mut P, args: &str) -> Result<Vec<u8>, GpioError> {
    let tokens: Vec<&str> = text::tokens(args).collect();
    match tokens.as_slice() {
        [] => Ok(summary(&*port)),
        [token] => info_token(port, token),
        _ => Err(GpioError::invalid(
            "gpio.info 参数非法；用法见 gpio.info help",
        )),
    }
}

/// 处理 `gpio.info` 的单个参数：`help`、`port=A` 或引脚名。
fn info_token<P: GpioPort>(port: &mut P, token: &str) -> Result<Vec<u8>, GpioError> {
    if token.eq_ignore_ascii_case("help") {
        return Ok(INFO_HELP.as_bytes().to_vec());
    }
    if let Some((key, value)) = text::split_key_value(token) {
        if text::key_eq(key, "port") {
            let target = text::parse_port(value)?;
            return port_listing(port.spec(), target);
        }
        return Err(GpioError::invalid(format!(
            "gpio.info 未知参数 {token}；用法见 gpio.info help"
        )));
    }
    let spec = port.spec();
    let pin = spec.resolve(token)?;
    pin_detail(port, pin)
}

/// 无参数摘要（计划 §5.3）：`key=value` 行，STM32 额外输出 `ports=`。
fn summary<P: GpioPort>(port: &P) -> Vec<u8> {
    let spec = port.spec();
    let family_summary = spec.summary();
    let family = spec.family.as_str();
    let chip = spec.chip;
    let backend = port.backend().as_str();
    let pins = family_summary.pins;
    let labels = family_summary.labels;
    let reserved = family_summary.reserved;
    let input_only = family_summary.input_only;
    let configured = port.configured_count();

    let mut out = String::new();
    push_line(&mut out, "gpio=1");
    push_line(&mut out, &format!("family={family}"));
    push_line(&mut out, &format!("chip={chip}"));
    push_line(&mut out, &format!("backend={backend}"));
    if !spec.ports.is_empty() {
        let mut ports = String::new();
        for port in spec.ports {
            if !ports.is_empty() {
                ports.push(',');
            }
            ports.push(port.as_char());
        }
        push_line(&mut out, &format!("ports={ports}"));
    }
    push_line(&mut out, &format!("pins={pins}"));
    push_line(&mut out, &format!("labels={labels}"));
    push_line(&mut out, &format!("reserved={reserved}"));
    push_line(&mut out, &format!("input_only={input_only}"));
    push_line(&mut out, &format!("configured={configured}"));
    out.into_bytes()
}

/// 单引脚详情（计划 §5.3）；未配置也允许，保留引脚同样可看原因。
fn pin_detail<P: GpioPort>(port: &mut P, spec: &'static PinSpec) -> Result<Vec<u8>, GpioError> {
    let description = port.describe(spec.id)?;
    let label = spec.id;
    let caps = caps_text(spec.caps);
    let flags = flags_text(spec.flags);
    let note = spec.note.trim();
    let configured = u8::from(description.config.is_some());

    let mut out = String::new();
    push_line(&mut out, &format!("pin={label}"));
    push_line(&mut out, &format!("caps={caps}"));
    push_line(&mut out, &format!("flags={flags}"));
    if !note.is_empty() {
        push_line(&mut out, &format!("note={note}"));
    }
    match description.config {
        None => push_line(&mut out, "mode=unconfigured"),
        Some(config) => {
            let mode = config.direction.as_str();
            push_line(&mut out, &format!("mode={mode}"));
            if let Some(level) = description.level {
                let level = level.as_str();
                push_line(&mut out, &format!("level={level}"));
            }
            let pull = config.pull.as_str();
            let open_drain = u8::from(config.open_drain);
            push_line(&mut out, &format!("pull={pull}"));
            push_line(&mut out, &format!("open_drain={open_drain}"));
        }
    }
    push_line(&mut out, &format!("configured={configured}"));
    Ok(out.into_bytes())
}

/// STM32 端口列表（计划 §5.3）：首行 `port=A`，之后每个表内引脚一行。
fn port_listing(spec: &'static FamilySpec, target: Port) -> Result<Vec<u8>, GpioError> {
    if spec.ports.is_empty() {
        return Err(GpioError::invalid(format!(
            "{} 家族没有端口；请使用 gpio.info PIN 查看单个引脚",
            spec.chip
        )));
    }
    if !spec.ports.contains(&target) {
        return Err(GpioError::invalid(format!(
            "{} 家族没有端口 {target}",
            spec.chip
        )));
    }

    let mut out = String::new();
    let port_label = target.as_char();
    push_line(&mut out, &format!("port={port_label}"));
    for pin in spec.pins {
        let PinId::Port { port: pin_port, .. } = pin.id else {
            continue;
        };
        if pin_port != target {
            continue;
        }
        let label = pin.id;
        let caps = caps_text(pin.caps);
        let flags = flags_text(pin.flags);
        push_line(&mut out, &format!("{label} caps={caps} flags={flags}"));
    }
    Ok(out.into_bytes())
}

/// `gpio.config`：部分更新、裸方向/键值、重复键报错。
fn config<P: GpioPort>(port: &mut P, args: &str) -> Result<Vec<u8>, GpioError> {
    let tokens: Vec<&str> = text::tokens(args).collect();
    let Some((pin_token, options)) = tokens.split_first() else {
        return Err(config_usage());
    };
    let spec = port.spec();
    let pin = spec.resolve(pin_token)?;
    let request = parse_config_request(options)?;
    let current = port.describe(pin.id)?.config;
    let resolved = resolve_config(pin, current, &request)?;
    port.configure(pin.id, resolved)?;
    Ok(format_config(pin.id, &resolved).into_bytes())
}

/// `gpio.config` 缺少引脚时的统一提示。
fn config_usage() -> GpioError {
    GpioError::invalid("gpio.config 缺少引脚；用法见 gpio.info help")
}

/// 参数重复时的统一错误。
fn duplicate_argument(name: &str) -> GpioError {
    GpioError::invalid(format!("gpio.config 参数重复：{name}"))
}

/// 解析 `gpio.config` 的裸方向与键值参数。
///
/// 裸方向 `in/input/out/output` 最多一个；键 `dir|direction`、
/// `level|init`、`pull`、`open_drain|od` 各最多一次。
fn parse_config_request(options: &[&str]) -> Result<ConfigRequest, GpioError> {
    let mut request = ConfigRequest::new();
    let mut direction_seen = false;
    let mut level_seen = false;
    let mut pull_seen = false;
    let mut open_drain_seen = false;

    for token in options {
        let Some((key, value)) = text::split_key_value(token) else {
            let direction = text::parse_direction(token).map_err(|_| {
                GpioError::invalid(format!(
                    "gpio.config 未知参数 {token}；用法见 gpio.info help"
                ))
            })?;
            if direction_seen {
                return Err(duplicate_argument("方向"));
            }
            request.direction = Some(direction);
            direction_seen = true;
            continue;
        };

        if text::key_eq(key, "dir") || text::key_eq(key, "direction") {
            if direction_seen {
                return Err(duplicate_argument("dir/direction"));
            }
            request.direction = Some(text::parse_direction(value)?);
            direction_seen = true;
        } else if text::key_eq(key, "level") || text::key_eq(key, "init") {
            if level_seen {
                return Err(duplicate_argument("level/init"));
            }
            request.level = Some(text::parse_level(value)?);
            level_seen = true;
        } else if text::key_eq(key, "pull") {
            if pull_seen {
                return Err(duplicate_argument("pull"));
            }
            request.pull = Some(text::parse_pull(value)?);
            pull_seen = true;
        } else if text::key_eq(key, "open_drain") || text::key_eq(key, "od") {
            if open_drain_seen {
                return Err(duplicate_argument("open_drain/od"));
            }
            request.open_drain = Some(text::parse_bool(value)?);
            open_drain_seen = true;
        } else {
            return Err(GpioError::invalid(format!(
                "gpio.config 未知键 {key}；用法见 gpio.info help"
            )));
        }
    }
    Ok(request)
}

/// `gpio.config` 成功结果的单行文本。
fn format_config(pin: PinId, config: &PinConfig) -> String {
    let mode = config.direction.as_str();
    let level = config.level.as_str();
    let pull = config.pull.as_str();
    let open_drain = u8::from(config.open_drain);
    format!("pin={pin} mode={mode} level={level} pull={pull} open_drain={open_drain}")
}

/// `gpio.read`：只允许输入模式。
fn read<P: GpioPort>(port: &mut P, args: &str) -> Result<Vec<u8>, GpioError> {
    let tokens: Vec<&str> = text::tokens(args).collect();
    let spec = port.spec();
    let pin = single_pin_argument(spec, &tokens, "gpio.read")?;
    let level = port.read(pin.id)?;
    Ok(format_level_line(pin.id, level))
}

/// `gpio.write`：只允许输出模式。
fn write<P: GpioPort>(port: &mut P, args: &str) -> Result<Vec<u8>, GpioError> {
    let tokens: Vec<&str> = text::tokens(args).collect();
    let [pin_token, level_token] = tokens.as_slice() else {
        return Err(GpioError::invalid(
            "gpio.write 需要引脚和电平；用法见 gpio.info help",
        ));
    };
    let spec = port.spec();
    let pin = spec.resolve(pin_token)?;
    let level = text::parse_level(level_token)?;
    port.write(pin.id, level)?;
    Ok(format_level_line(pin.id, level))
}

/// 从分词结果中取唯一引脚并解析到家族表项。
fn single_pin_argument(
    spec: &'static FamilySpec,
    tokens: &[&str],
    tool: &str,
) -> Result<&'static PinSpec, GpioError> {
    let [pin_token] = tokens else {
        return Err(GpioError::invalid(format!(
            "{tool} 需要且仅需要一个引脚；用法见 gpio.info help"
        )));
    };
    spec.resolve(pin_token)
}

/// `read`/`write` 成功结果的单行文本。
fn format_level_line(pin: PinId, level: Level) -> Vec<u8> {
    let level = level.as_str();
    format!("pin={pin} level={level}").into_bytes()
}

/// 创建共享句柄；默认构建用 `Rc<RefCell<P>>`。
#[cfg(not(feature = "send"))]
fn new_shared<P: GpioPort>(port: P) -> SharedPort<P> {
    alloc::rc::Rc::new(core::cell::RefCell::new(port))
}

/// 创建共享句柄；`send` 特性下用 `Arc<critical_section::Mutex<RefCell<P>>>`。
#[cfg(feature = "send")]
fn new_shared<P: GpioPort>(port: P) -> SharedPort<P> {
    alloc::sync::Arc::new(critical_section::Mutex::new(core::cell::RefCell::new(port)))
}

/// 注册 `gpio.*` 工具。
///
/// 默认构建使用 `Rc<RefCell<P>>` 共享句柄；`send` 特性下处理器要求
/// `Send`，因此端口也必须可跨线程移动。
///
/// 如果注册表已存在同名 `gpio.*` 工具，返回
/// [`RegistryError::Duplicate`]；此时可能已部分注册（按
/// [`GPIO_TOOL_NAMES`] 的顺序逐个注册，遇到错误立即返回）。
#[cfg(not(feature = "send"))]
pub fn register_tools<P>(registry: &mut ToolRegistry, port: P) -> Result<(), RegistryError>
where
    P: GpioPort + 'static,
{
    register_impl(registry, new_shared(port))
}

/// 注册 `gpio.*` 工具；`send` 特性下端口句柄必须可跨线程移动。
#[cfg(feature = "send")]
pub fn register_tools<P>(registry: &mut ToolRegistry, port: P) -> Result<(), RegistryError>
where
    P: GpioPort + Send + 'static,
{
    register_impl(registry, new_shared(port))
}

/// 用已有的共享句柄注册四个工具；[`register_tools`] 的公共实现。
#[cfg(not(feature = "send"))]
pub(crate) fn register_impl<P>(
    registry: &mut ToolRegistry,
    shared: SharedPort<P>,
) -> Result<(), RegistryError>
where
    P: GpioPort + 'static,
{
    registry.register(
        GPIO_TOOL_NAMES[0],
        ConfigTool {
            shared: shared.clone(),
        },
    )?;
    registry.register(
        GPIO_TOOL_NAMES[1],
        InfoTool {
            shared: shared.clone(),
        },
    )?;
    registry.register(
        GPIO_TOOL_NAMES[2],
        ReadTool {
            shared: shared.clone(),
        },
    )?;
    registry.register(GPIO_TOOL_NAMES[3], WriteTool { shared })?;
    Ok(())
}

/// 用已有的共享句柄注册四个工具；`send` 特性版本。
#[cfg(feature = "send")]
pub(crate) fn register_impl<P>(
    registry: &mut ToolRegistry,
    shared: SharedPort<P>,
) -> Result<(), RegistryError>
where
    P: GpioPort + Send + 'static,
{
    registry.register(
        GPIO_TOOL_NAMES[0],
        ConfigTool {
            shared: shared.clone(),
        },
    )?;
    registry.register(
        GPIO_TOOL_NAMES[1],
        InfoTool {
            shared: shared.clone(),
        },
    )?;
    registry.register(
        GPIO_TOOL_NAMES[2],
        ReadTool {
            shared: shared.clone(),
        },
    )?;
    registry.register(GPIO_TOOL_NAMES[3], WriteTool { shared })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{GPIO_TOOL_NAMES, INFO_HELP, register_tools};
    use crate::config::{Level, PinConfig};
    use crate::family::{ChipFamily, FamilyKind, FamilySpec, PinCaps, PinFlags, PinSpec};
    use crate::pin::{PinId, Port};
    use crate::port::{GpioError, GpioPort, PinDescription};
    use crate::virtual_gpio::VirtualGpio;
    use alloc::boxed::Box;
    use alloc::vec::Vec;
    use sekai::sekai_device_list::DeviceList;
    use sekai::sekai_tool::{ToolContext, ToolRegistry};
    use sekai::sekai_wire::CommandError;

    fn call(registry: &mut ToolRegistry, name: &str, args: &[u8]) -> Result<Vec<u8>, CommandError> {
        let workers = DeviceList::new();
        let mut ctx = ToolContext {
            args,
            now_mono_ms: 0,
            now_unix_ms: None,
            workers: &workers,
        };
        let tool = registry.get_mut(name).expect("工具应已注册");
        tool.call(&mut ctx)
    }

    #[test]
    fn tool_names_are_stable() {
        assert_eq!(GPIO_TOOL_NAMES.len(), 4);
        assert_eq!(GPIO_TOOL_NAMES[0], "gpio.config");
        assert_eq!(GPIO_TOOL_NAMES[1], "gpio.info");
        assert_eq!(GPIO_TOOL_NAMES[2], "gpio.read");
        assert_eq!(GPIO_TOOL_NAMES[3], "gpio.write");
        assert!(!INFO_HELP.ends_with('\n'));
    }

    #[cfg(not(feature = "send"))]
    #[test]
    fn borrow_failure_maps_to_internal_error() {
        use super::{SharedPort, with_port};
        use sekai::sekai_wire::ErrorCode;

        let shared: SharedPort<VirtualGpio> = alloc::rc::Rc::new(core::cell::RefCell::new(
            VirtualGpio::new(ChipFamily::Esp32),
        ));
        let guard = shared.borrow_mut();
        let error = with_port(&shared, |_port| Ok::<(), GpioError>(())).unwrap_err();
        assert_eq!(error.code, ErrorCode::Internal);
        assert_eq!(error.message, "GPIO 被占用");
        drop(guard);
        assert!(with_port(&shared, |_port| Ok::<(), GpioError>(())).is_ok());
    }

    /// 假后端：只提供引脚表，用于构造超出结果上限的摘要。
    struct FakePort {
        spec: &'static FamilySpec,
    }

    impl GpioPort for FakePort {
        fn spec(&self) -> &'static FamilySpec {
            self.spec
        }

        fn configure(&mut self, _pin: PinId, _config: PinConfig) -> Result<(), GpioError> {
            Ok(())
        }

        fn read(&mut self, _pin: PinId) -> Result<Level, GpioError> {
            Ok(Level::Low)
        }

        fn write(&mut self, _pin: PinId, _level: Level) -> Result<(), GpioError> {
            Ok(())
        }

        fn describe(&mut self, _pin: PinId) -> Result<PinDescription, GpioError> {
            Err(GpioError::invalid("fake"))
        }
    }

    /// 构造一个引脚数足够多的假家族表：相邻引脚跨端口，范围压缩不会生效，
    /// 因此 `gpio.info` 摘要会超过 1024 字节的结果上限。
    fn oversized_spec() -> &'static FamilySpec {
        let pins: Vec<PinSpec> = (0..400u16)
            .map(|index| {
                let port = Port::from_ascii(b'A' + (index % 11) as u8).expect("端口合法");
                PinSpec {
                    id: PinId::port(port, (index % 16) as u8),
                    slot: index,
                    caps: PinCaps::ALL,
                    flags: PinFlags::NONE,
                    note: "",
                }
            })
            .collect();
        let pins: &'static [PinSpec] = Box::leak(pins.into_boxed_slice());
        Box::leak(Box::new(FamilySpec {
            family: FamilyKind::Stm32,
            chip: "fake",
            pins,
            slots: pins.len(),
            ports: &[],
        }))
    }

    #[test]
    fn oversized_result_maps_to_result_too_large() {
        use sekai::sekai_wire::ErrorCode;

        assert!(oversized_spec().summary().labels.len() > 1024);
        let mut registry = ToolRegistry::new();
        register_tools(
            &mut registry,
            FakePort {
                spec: oversized_spec(),
            },
        )
        .unwrap();
        let error = call(&mut registry, "gpio.info", b"").unwrap_err();
        assert_eq!(error.code, ErrorCode::ResultTooLarge);
        assert_eq!(error.message, "GPIO 结果过大");
    }

    #[test]
    fn args_longer_than_256_bytes_are_rejected() {
        use sekai::sekai_wire::ErrorCode;

        let mut registry = ToolRegistry::new();
        register_tools(&mut registry, VirtualGpio::new(ChipFamily::Esp32)).unwrap();
        let mut long = alloc::string::String::from("IO4");
        for _ in 0..254 {
            long.push(' ');
        }
        assert_eq!(long.len(), 257);
        let error = call(&mut registry, "gpio.info", long.as_bytes()).unwrap_err();
        assert_eq!(error.code, ErrorCode::InvalidArgument);
        assert!(error.message.contains("256"), "{}", error.message);

        long.pop();
        assert_eq!(long.len(), 256);
        let result = call(&mut registry, "gpio.info", long.as_bytes()).unwrap();
        assert_eq!(
            alloc::string::String::from_utf8(result).unwrap(),
            "pin=IO4\ncaps=in,out,pu,pd,od\nflags=none\nmode=unconfigured\nconfigured=0"
        );
    }
}
