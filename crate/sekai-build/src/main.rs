//! ESP32 固件的构建与烧录驱动。
//!
//! 固件构建配置写在本程序里，而不是放在 `.cargo/config.toml` 中，
//! 这样无论从哪个目录调用 cargo，用到的参数都一致。
//!
//! ```text
//! cargo run -p sekai-build -- build [--release]
//! cargo run -p sekai-build -- flash [--release] [--monitor] [-- <espflash args>]
//! ```
//!
//! 两个命令都依赖由 `espup install -t esp32` 安装的 Espressif 工具链：
//! 上游 nightly 的 Xtensa 后端在编译 `compiler_builtins` 时会崩溃，
//! 因此必须使用打过补丁的 fork。可通过 `SEKAI_TOOLCHAIN` 改用其他工具链。

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

/// `sekai-embedded` 中 `esp32` 驱动所针对的芯片。
const CHIP: &str = "esp32";
/// 该芯片的 target triple。
const TARGET: &str = "xtensa-esp32-none-elf";
/// 本包中定义的固件二进制 target。
const FIRMWARE_BIN: &str = "sekai-fw";
/// 由 `espup` 安装的工具链。
const DEFAULT_TOOLCHAIN: &str = "esp";
/// 用于覆盖 [`DEFAULT_TOOLCHAIN`]。
const TOOLCHAIN_ENV: &str = "SEKAI_TOOLCHAIN";
/// `esp-hal` 会把 `linkall.x` —— 它又引入 `memory.x`、`alias.x`、
/// `esp32.x` 和 `hal-defaults.x` —— 复制到自己的 `OUT_DIR`，并把该目录
/// 加入链接搜索路径，因此直接用文件名即可解析到。
const LINKER_ARGS: &str =
    r#"target."xtensa-esp32-none-elf".rustflags=["-C","link-arg=-Tlinkall.x"]"#;

const USAGE: &str = "\
sekai-build — ESP32 firmware builder

USAGE:
    cargo run -p sekai-build -- build [--release]
    cargo run -p sekai-build -- flash [--release] [--monitor] [-- <espflash args>]

COMMANDS:
    build    Compile the firmware image for xtensa-esp32-none-elf
    flash    Build it, then write it over serial with espflash
    help     Show this message

OPTIONS:
    --release    Use the release profile (default: debug)
    --monitor    Keep the serial monitor open after flashing (flash only)
    --           Forward the remaining arguments to espflash (flash only)

ENVIRONMENT:
    SEKAI_TOOLCHAIN    Rust toolchain to build with (default: esp)

`esp` comes from `espup install -t esp32` and is required: the upstream Xtensa
LLVM backend cannot compile `compiler_builtins`.

COMPILE-TIME FIRMWARE ENVIRONMENT:
The firmware reads these with `option_env!` when it is built, so they must be
set for `build` (not when flashing). Missing required variables only produce
warnings, so CI can still compile an image that reports the configuration error
over serial at startup.

    SEKAI_WIFI_SSID        required   2.4GHz SSID
    SEKAI_WIFI_PASSWORD    optional   empty = Open, non-empty = WPA2 Personal
    SEKAI_MQTT_ADDR        required   IPv4:port, e.g. 192.168.1.10:1883 (no DNS)
    SEKAI_MQTT_USERNAME    optional   MQTT username
    SEKAI_MQTT_PASSWORD    optional   MQTT password
    SEKAI_DEVICE_ID        optional   worker id (default: sekai-esp32)
    SEKAI_TOPIC_ROOT       optional   MQTT topic root (default: sekai/v1)
    SEKAI_DEVICES          optional   jump targets, name=IPv4:port,...";

/// 构建前提示缺失的必填固件环境变量：只警告不失败，方便 CI 只编译。
///
/// 固件通过 `option_env!` 在编译期读取这些变量；缺失时构建仍会成功，
/// 但固件启动后会打印配置错误并空转。
fn warn_missing_firmware_env() {
    const REQUIRED: [&str; 2] = ["SEKAI_WIFI_SSID", "SEKAI_MQTT_ADDR"];

    let missing: Vec<&str> = REQUIRED
        .into_iter()
        .filter(|name| {
            std::env::var(name)
                .map(|value| value.trim().is_empty())
                .unwrap_or(true)
        })
        .collect();
    if missing.is_empty() {
        return;
    }
    for name in &missing {
        eprintln!("warning: required firmware environment variable `{name}` is not set");
    }
    eprintln!(
        "warning: the built firmware will report a configuration error over serial at startup; \
         see the COMPILE-TIME FIRMWARE ENVIRONMENT section of \
         `cargo run -p sekai-build -- help`"
    );
}

fn main() -> ExitCode {
    match run(std::env::args().skip(1).collect()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("\nerror: {error}");
            ExitCode::FAILURE
        }
    }
}

#[derive(Default)]
struct Options {
    release: bool,
    monitor: bool,
    toolchain: String,
    passthrough: Vec<String>,
}

fn run(args: Vec<String>) -> Result<(), String> {
    let Some((command, rest)) = args.split_first() else {
        println!("{USAGE}");
        return Err("no command given".to_owned());
    };

    if matches!(command.as_str(), "help" | "--help" | "-h") {
        println!("{USAGE}");
        return Ok(());
    }

    let mut options = Options {
        toolchain: std::env::var(TOOLCHAIN_ENV).unwrap_or_else(|_| DEFAULT_TOOLCHAIN.to_owned()),
        ..Options::default()
    };
    let mut forwarding = false;
    for arg in rest {
        if forwarding {
            options.passthrough.push(arg.clone());
            continue;
        }
        match arg.as_str() {
            "--release" => options.release = true,
            "--monitor" => options.monitor = true,
            "--" => forwarding = true,
            other => return Err(format!("unknown argument `{other}`\n\n{USAGE}")),
        }
    }

    match command.as_str() {
        "build" => {
            if options.monitor || !options.passthrough.is_empty() {
                return Err("`--monitor` and arguments after `--` only apply to `flash`".to_owned());
            }
            build(&options).map(|_| ())
        }
        "flash" => flash(&options),
        other => Err(format!("unknown command `{other}`\n\n{USAGE}")),
    }
}

/// 编译固件镜像，并返回生成的 ELF 路径。
fn build(options: &Options) -> Result<PathBuf, String> {
    warn_missing_firmware_env();
    preflight_toolchain(&options.toolchain)?;

    let root = workspace_root()?;
    let mut command = Command::new("cargo");
    command
        .current_dir(&root)
        .arg(format!("+{}", options.toolchain))
        .args([
            "build",
            "--package",
            "sekai-build",
            "--bin",
            FIRMWARE_BIN,
            // 只选中 `sekai-embedded` 的 `esp32` 驱动，不选任何其他驱动：
            // `x86` 驱动与 `esp32` 驱动互斥。
            "--no-default-features",
            "--features",
            "esp32",
            "--target",
            TARGET,
            // Xtensa 没有预构建的 `core`/`alloc`。
            "-Zbuild-std=core,alloc",
            // 作用域限定在 target 上，因此不会影响宿主的 build script。
            "--config",
            LINKER_ARGS,
        ]);
    if options.release {
        command.arg("--release");
    }

    println!("$ {}", describe(&command));
    let status = command
        .status()
        .map_err(|error| format!("failed to run cargo: {error}"))?;
    if !status.success() {
        return Err(format!("cargo build failed ({status})"));
    }

    let elf = elf_path(&root, options.release);
    let size = std::fs::metadata(&elf)
        .map(|metadata| metadata.len())
        .map_err(|error| format!("cannot read {}: {error}", elf.display()))?;
    println!(
        "\nfirmware {} ({} KiB)",
        elf.display(),
        size as f64 / 1024.0
    );
    Ok(elf)
}

/// 构建固件并写入开发板。
fn flash(options: &Options) -> Result<(), String> {
    let elf = build(options)?;
    preflight_espflash()?;

    let mut command = Command::new("espflash");
    command.args(["flash", "--chip", CHIP]);
    if options.monitor {
        command.arg("--monitor");
    }
    command.arg(&elf).args(&options.passthrough);

    println!("$ {}", describe(&command));
    let status = command
        .status()
        .map_err(|error| format!("failed to run espflash: {error}"))?;
    if !status.success() {
        return Err(format!("espflash failed ({status})"));
    }
    Ok(())
}

/// 当工具链不可用时提前失败，并给出可操作的提示信息。
fn preflight_toolchain(toolchain: &str) -> Result<(), String> {
    let output = Command::new("rustc")
        .arg(format!("+{toolchain}"))
        .args(["--print", "target-list"])
        .output();

    let Ok(output) = output else {
        return Err(missing_toolchain(toolchain));
    };
    if !output.status.success() {
        return Err(missing_toolchain(toolchain));
    }

    let targets = String::from_utf8_lossy(&output.stdout);
    if !targets.lines().any(|line| line.trim() == TARGET) {
        return Err(format!(
            "toolchain `{toolchain}` does not know the target `{TARGET}`; \
             install the Espressif fork with `espup install -t {CHIP}`"
        ));
    }
    Ok(())
}

fn missing_toolchain(toolchain: &str) -> String {
    format!(
        "toolchain `{toolchain}` is not installed.\n\
         Install the Espressif fork with:  espup install -t {CHIP}\n\
         It is required, because the upstream Xtensa LLVM backend cannot compile \
         `compiler_builtins`.\n\
         Use another toolchain by setting {TOOLCHAIN_ENV}."
    )
}

fn preflight_espflash() -> Result<(), String> {
    match Command::new("espflash").arg("--version").output() {
        Ok(output) if output.status.success() => Ok(()),
        _ => Err(
            "`espflash` was not found on PATH; install it with `cargo install espflash`".to_owned(),
        ),
    }
}

/// workspace 根目录，由本 crate 所在的位置推导，而不是取当前目录，
/// 因此该驱动在任何目录下都能正常工作。
fn workspace_root() -> Result<PathBuf, String> {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .ok_or_else(|| {
            format!(
                "cannot derive the workspace root from {}",
                env!("CARGO_MANIFEST_DIR")
            )
        })
}

fn elf_path(root: &Path, release: bool) -> PathBuf {
    let target_dir = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .map(|dir| {
            if dir.is_absolute() {
                dir
            } else {
                root.join(dir)
            }
        })
        .unwrap_or_else(|| root.join("target"));

    target_dir
        .join(TARGET)
        .join(if release { "release" } else { "debug" })
        .join(FIRMWARE_BIN)
}

/// 按用户实际输入时的样子渲染命令。
fn describe(command: &Command) -> String {
    std::iter::once(command.get_program().to_string_lossy().into_owned())
        .chain(command.get_args().map(|arg| {
            let arg = arg.to_string_lossy();
            if !arg.contains([' ', '\t']) {
                arg.into_owned()
            } else if arg.contains('"') {
                format!("'{arg}'")
            } else {
                format!("\"{arg}\"")
            }
        }))
        .collect::<Vec<_>>()
        .join(" ")
}
