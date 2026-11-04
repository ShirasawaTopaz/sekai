//! sekai-worker CLI：PC Worker 宿主入口。
//!
//! 子命令风格（推荐）：
//!
//! - `sekai-worker connect <HOST:PORT>`：连接 Router 的直连接入端点，
//!   在 stderr 输出阶段化连接进度（连接中 → TCP 已连接 → Hello 已发布）
//!   与分类错误提示（提示行 + 原始错误行）；断线按指数退避（500ms 起步、
//!   30s 封顶）重连，重连后重新发布 Hello 并按 `(boot_id, seq)` 重放
//!   未确认事件。
//! - `sekai-worker stdio`：子进程模式。Router 拉起本进程，stdin/stdout 即
//!   直连链路；stdin 关闭（EOF）即正常退出。**无参数默认即本模式**，
//!   便于 Router 以 `sekai-worker --mode stdio` 或裸进程名拉起。
//! - `sekai-worker help`：帮助。
//!
//! 兼容旧旗标：`--mode stdio|tcp`、`--router-addr HOST:PORT` 等仍可用；
//! 子命令与 `--mode` 同时给出且不一致时报冲突。
//!
//! 链路帧格式：`u32 大端长度 + V1 Stdin/Stdout 帧字节`（见 `sekai_worker::link`）。

use std::io::Write;
use std::process::ExitCode;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use sekai::sekai_device_list::{DeviceList, Endpoint};
use sekai::sekai_runtime::{LinkAction, RuntimeConfig};
use sekai_gpio::ChipFamily;
use sekai_worker::capability::{SystemClock, host_registries};
use sekai_worker::host::WorkerHost;
use sekai_worker::link::{
    Backoff, Link, LinkError, StdioLink, TcpLink, connect_hint, read_frame_stream,
    write_frame_stream,
};

/// 空闲轮询间隔：读超时当作一次 tick，借此推进任务/定时器。
const TICK_MS: u64 = 50;

const USAGE: &str = "\
sekai-worker：Sekai PC Worker（直连链路，非 MQTT broker）

用法：
  sekai-worker connect <HOST:PORT> [选项]   连接 Router 直连接入端点（tcp 客户端）
  sekai-worker [stdio] [选项]               子进程模式（stdin/stdout 即链路；默认）
  sekai-worker help                         显示本帮助

选项：
  --worker-id ID          Worker id（默认 pc-worker）
  --family esp32|stm32f1|stm32f4
                          virtual GPIO 家族（默认 esp32）
  --boot-id N             启动周期 id（默认按时间+pid 生成，跨重启必变）
  -h, --help              显示本帮助

兼容旗标（等价子命令）：
  --mode stdio|tcp        运行模式（默认 stdio）
  --router-addr HOST:PORT tcp 模式的 Router 直连接入地址（tcp 模式必填）

示例：
  sekai-worker connect 127.0.0.1:7421 --worker-id desk-1
  sekai-router            # 另一终端：/link tcp 后 /wait 等本 Worker 上线
";

/// 解析后的命令行参数。
struct Args {
    mode: Mode,
    router_addr: Option<String>,
    worker_id: String,
    family: ChipFamily,
    boot_id: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Stdio,
    Tcp,
}

/// 参数错误：消息已带上下文，直接展示给用户。
struct UsageError(String);

/// 解析结果：Help 或参数。
enum Parsed {
    Help,
    Run(Box<Args>),
}

fn parse_args(argv: &[String]) -> Result<Parsed, UsageError> {
    let mut mode: Option<Mode> = None;
    let mut router_addr: Option<String> = None;
    let mut worker_id = String::from("pc-worker");
    let mut family = ChipFamily::Esp32;
    let mut boot_id = generate_boot_id();
    let mut flag_mode: Option<Mode> = None;

    let mut index = 0;
    while index < argv.len() {
        let arg = argv[index].as_str();
        // 取值闭包：吞掉下一个 token。
        let mut take_value = |name: &str| -> Result<String, UsageError> {
            index += 1;
            argv.get(index)
                .cloned()
                .ok_or_else(|| UsageError(format!("{name} 缺少取值")))
        };
        match arg {
            "help" | "-h" | "--help" => return Ok(Parsed::Help),
            // ---- 子命令 ----
            "stdio" => {
                if mode.is_some() {
                    return Err(UsageError(String::from("子命令重复")));
                }
                mode = Some(Mode::Stdio);
            }
            "connect" => {
                if mode.is_some() {
                    return Err(UsageError(String::from("子命令重复")));
                }
                let value = take_value("connect <HOST:PORT>")?;
                Endpoint::parse(&value)
                    .map_err(|error| UsageError(format!("connect 地址非法：{error}")))?;
                router_addr = Some(value);
                mode = Some(Mode::Tcp);
            }
            // ---- 兼容旗标 ----
            "--mode" => {
                let value = take_value("--mode")?;
                flag_mode = Some(match value.as_str() {
                    "stdio" => Mode::Stdio,
                    "tcp" => Mode::Tcp,
                    other => {
                        return Err(UsageError(format!(
                            "--mode 不支持 {other:?}（可选 stdio/tcp）"
                        )));
                    }
                });
            }
            "--router-addr" => {
                let value = take_value("--router-addr")?;
                Endpoint::parse(&value)
                    .map_err(|error| UsageError(format!("--router-addr 非法：{error}")))?;
                if router_addr.is_some() {
                    return Err(UsageError(String::from("--router-addr 重复")));
                }
                router_addr = Some(value);
                flag_mode = Some(Mode::Tcp);
            }
            // ---- 通用选项 ----
            "--worker-id" => {
                worker_id = take_value("--worker-id")?;
            }
            "--family" => {
                let value = take_value("--family")?;
                family = match value.as_str() {
                    "esp32" => ChipFamily::Esp32,
                    "stm32f1" => ChipFamily::Stm32F1,
                    "stm32f4" => ChipFamily::Stm32F4,
                    other => {
                        return Err(UsageError(format!(
                            "--family 不支持 {other:?}（可选 esp32/stm32f1/stm32f4）"
                        )));
                    }
                };
            }
            "--boot-id" => {
                let value = take_value("--boot-id")?;
                boot_id = value
                    .parse()
                    .map_err(|_| UsageError(String::from("--boot-id 必须是正整数")))?;
                if boot_id == 0 {
                    return Err(UsageError(String::from("--boot-id 不能为 0")));
                }
            }
            other => return Err(UsageError(format!("未知参数 {other:?}"))),
        }
        index += 1;
    }

    // 子命令与 --mode 冲突检查。
    let mode = match (mode, flag_mode) {
        (Some(sub), Some(flag)) if sub != flag => {
            return Err(UsageError(String::from(
                "子命令与 --mode 同时给出且不一致（connect=tcp，stdio=stdio）",
            )));
        }
        (Some(sub), _) | (None, Some(sub)) => sub,
        (None, None) => Mode::Stdio,
    };
    if mode == Mode::Tcp && router_addr.is_none() {
        return Err(UsageError(String::from(
            "tcp 模式必须提供地址：sekai-worker connect HOST:PORT（或 --router-addr）",
        )));
    }
    Ok(Parsed::Run(Box::new(Args {
        mode,
        router_addr,
        worker_id,
        family,
        boot_id,
    })))
}

/// 生成跨重启必变的非零 boot_id（时间纳秒 ^ pid 混淆）。
fn generate_boot_id() -> u64 {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos() as u64)
        .unwrap_or(0);
    let pid = u64::from(std::process::id());
    (nanos ^ (pid << 32) ^ 0x9E37_79B9_7F4A_7C15).max(1)
}

fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let args = match parse_args(&argv) {
        Ok(Parsed::Help) => {
            print!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        Ok(Parsed::Run(args)) => *args,
        Err(error) => {
            eprintln!("sekai-worker: {}", error.0);
            eprintln!("用 sekai-worker help 查看用法");
            return ExitCode::from(2);
        }
    };

    let result = match args.mode {
        Mode::Stdio => run_stdio(&args),
        Mode::Tcp => run_tcp(&args),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("sekai-worker: {message}");
            ExitCode::FAILURE
        }
    }
}

/// 组装 WorkerHost（能力：virtual GPIO + echo/sleep 任务）。
fn build_host(args: &Args) -> Result<WorkerHost<SystemClock>, String> {
    let (tasks, tools) = host_registries(args.family)?;
    let clock = SystemClock::new();
    let mut config = RuntimeConfig::new(args.worker_id.clone(), args.boot_id);
    config.firmware = format!("sekai-worker/{}", env!("CARGO_PKG_VERSION"));
    WorkerHost::new(config, clock, DeviceList::new(), tasks, tools)
        .map_err(|error| error.to_string())
}

// ---- stdio 模式 ----

fn run_stdio(args: &Args) -> Result<(), String> {
    let mut host = build_host(args)?;
    let endpoint = Endpoint::new("local", 1).map_err(|error| error.to_string())?;
    let (reader, writer) = StdioLink::std().into_parts();
    let mut writer = writer.ok_or("stdio 写半侧不可用")?;

    // Hello 在链路建立后的第一次 poll 中产出。
    let frames = host
        .on_link_up(endpoint)
        .map_err(|error| error.to_string())?;
    write_all(&mut writer, frames)?;

    // 读侧交给独立线程（stdin 无法设置读超时），主循环用带超时的通道做 tick。
    let (tx, rx) = mpsc::channel::<Option<Vec<u8>>>();
    if let Some(mut reader) = reader {
        thread::spawn(move || {
            loop {
                match read_frame_stream(&mut reader) {
                    Ok(Some(frame)) => {
                        if tx.send(Some(frame)).is_err() {
                            break;
                        }
                    }
                    Ok(None) => {
                        let _ = tx.send(None);
                        break;
                    }
                    Err(error) => {
                        eprintln!("sekai-worker: stdin 读取失败：{error}");
                        let _ = tx.send(None);
                        break;
                    }
                }
            }
        });
    }

    loop {
        let outcome = match rx.recv_timeout(Duration::from_millis(TICK_MS)) {
            Ok(Some(frame)) => host.on_frame(&frame),
            Ok(None) => break, // Router 关闭了 stdin，正常退出
            Err(mpsc::RecvTimeoutError::Timeout) => host.on_poll(),
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        };
        let frames = outcome.map_err(|error| error.to_string())?;
        write_all(&mut writer, frames)?;

        if host.disconnect_requested() {
            eprintln!("sekai-worker: 运行时请求断开，stdio 模式直接退出");
            break;
        }
        while let Some(action) = host.take_link_action() {
            eprintln!("sekai-worker: stdio 模式忽略链路切换：{action:?}");
        }
    }
    let _ = writer.flush();
    Ok(())
}

fn write_all<W: Write>(writer: &mut W, frames: Vec<Vec<u8>>) -> Result<(), String> {
    for frame in frames {
        write_frame_stream(writer, &frame).map_err(|error| error.to_string())?;
    }
    Ok(())
}

// ---- tcp 模式 ----

// ---- connect 模式阶段文案（纯函数，便于单测；行式输出走 stderr）----

/// 连接尝试的进度行：`sekai-worker: 正在连接 127.0.0.1:7421 …（第 1 次）`。
///
/// `attempt` 是本次「从断线到连上」周期内的尝试序号（从 1 起，连接成功后
/// 随退避一起归 1，与 `Backoff` 的节奏一致）。
fn connecting_line(address: &str, attempt: u64) -> String {
    format!("sekai-worker: 正在连接 {address} …（第 {attempt} 次）")
}

/// TCP 建立成功行。
fn connected_line(address: &str) -> String {
    format!("sekai-worker: TCP 已连接 {address}")
}

/// 链路就绪行。Hello 是链路建立后的首帧（retained state 语义，见
/// `host.rs`）：直连承载没有 Router 对 Hello 的显式确认帧，首帧写出
/// 成功即「已发布」，链路随之就绪、可收发 Router 下行。
fn link_ready_line() -> String {
    String::from("sekai-worker: Hello 已发布，链路已就绪")
}

/// 重连等待行：`sekai-worker: 3 秒后第 2 次重连…`。
///
/// `delay_ms` 为退避等待时长；`attempt` 是**下一次**尝试的序号（≥ 2）。
fn reconnect_wait_line(delay_ms: u64, attempt: u64) -> String {
    format!(
        "sekai-worker: {} 秒后第 {attempt} 次重连…",
        delay_ms as f64 / 1000.0
    )
}

/// 失败报告（两行）：可操作提示 + 原始错误。
///
/// 提示来自 [`connect_hint`] 的分类；`{error}` 原样保留诊断信息，不吞掉。
/// 错误文案若已以 `context` 开头（如 `LinkError::Connect` 的 Display 即
/// 「连接失败：…」，与 connect 的上下文标签撞词），则不再重复拼接前缀。
fn failure_report(context: &str, hint: &str, error: &LinkError) -> String {
    let text = error.to_string();
    let detail = if text.starts_with(context) {
        text
    } else {
        format!("{context}：{text}")
    };
    format!("sekai-worker: 提示：{hint}\nsekai-worker: {detail}")
}

/// 输出失败报告（提示行 + 原始错误行，均走 stderr）。
fn report_failure(context: &str, error: &LinkError) {
    eprintln!("{}", failure_report(context, connect_hint(error), error));
}

/// 取一次退避时长、打印重连等待行并等待；`attempt` 先自增到下一次序号。
fn wait_before_reconnect(backoff: &mut Backoff, attempt: &mut u64) {
    let delay = backoff.next_delay_ms();
    *attempt += 1;
    eprintln!("{}", reconnect_wait_line(delay, *attempt));
    thread::sleep(Duration::from_millis(delay));
}

/// Hello（链路建立后的首帧）本轮已写出 → 打印就绪行；每个连接周期只打一次。
fn mark_link_ready(written: usize, hello_pending: &mut bool) {
    if written > 0 && *hello_pending {
        eprintln!("{}", link_ready_line());
        *hello_pending = false;
    }
}

fn run_tcp(args: &Args) -> Result<(), String> {
    let mut host = build_host(args)?;
    let mut link = TcpLink::new();
    let mut backoff = Backoff::new();
    let mut address = args
        .router_addr
        .clone()
        .ok_or_else(|| String::from("tcp 模式必须提供 Router 地址"))?;

    eprintln!(
        "sekai-worker: connect {address}（worker_id={}，Ctrl-C 退出）",
        args.worker_id
    );
    // 本次连接周期内的尝试序号；连接成功后随退避一起归 1。
    let mut attempt: u64 = 1;
    loop {
        // 跳转（Jump）触发的切换不需要退避，普通断线重连需要。
        let mut immediate = false;
        eprintln!("{}", connecting_line(&address, attempt));
        if let Err(error) = link.connect(&address) {
            report_failure("连接失败", &error);
            wait_before_reconnect(&mut backoff, &mut attempt);
            continue;
        }
        backoff.reset();
        attempt = 1;
        eprintln!("{}", connected_line(&address));

        let endpoint = Endpoint::parse(&address).map_err(|error| error.to_string())?;
        let frames = host
            .on_link_up(endpoint)
            .map_err(|error| error.to_string())?;
        // Hello 通常在链路建立后的首个 poll 中产出（见 host.rs）；
        // 首帧写出成功即链路就绪。
        let mut hello_pending = true;
        match send_frames(&mut link, frames) {
            Ok(written) => mark_link_ready(written, &mut hello_pending),
            Err(error) => {
                report_failure("写链路失败", &error);
                host.on_link_lost();
                wait_before_reconnect(&mut backoff, &mut attempt);
                continue;
            }
        }

        loop {
            let outcome = match link.read_frame() {
                Ok(Some(frame)) => host.on_frame(&frame),
                Ok(None) => {
                    eprintln!("sekai-worker: Router 关闭连接");
                    host.on_link_lost();
                    break;
                }
                Err(LinkError::Timeout) => host.on_poll(),
                Err(error) => {
                    report_failure("读链路失败", &error);
                    host.on_link_lost();
                    break;
                }
            };
            let frames = outcome.map_err(|error| error.to_string())?;
            let written = match send_frames(&mut link, frames) {
                Ok(written) => written,
                Err(error) => {
                    report_failure("写链路失败", &error);
                    host.on_link_lost();
                    break;
                }
            };
            mark_link_ready(written, &mut hello_pending);

            if host.disconnect_requested() {
                host.on_link_closed();
                link.close();
            }
            while let Some(action) = host.take_link_action() {
                match action {
                    LinkAction::Switch { endpoint, .. } => {
                        eprintln!(
                            "sekai-worker: 切换到新端点 {}:{}",
                            endpoint.host, endpoint.port
                        );
                        address = format!("{}:{}", endpoint.host, endpoint.port);
                        immediate = true;
                    }
                }
            }
            if immediate {
                break;
            }
        }

        // Jump 触发的切换立即重连（不退避、不打印等待行）。
        if !immediate {
            wait_before_reconnect(&mut backoff, &mut attempt);
        }
    }
}

/// 逐帧写出并返回成功写出的帧数（用于判定 Hello 是否已发布）。
fn send_frames(link: &mut TcpLink, frames: Vec<Vec<u8>>) -> Result<usize, LinkError> {
    let mut written = 0;
    for frame in frames {
        link.write_frame(&frame)?;
        written += 1;
    }
    Ok(written)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(items: &[&str]) -> Vec<String> {
        items.iter().map(|item| String::from(*item)).collect()
    }

    fn parse_ok(items: &[&str]) -> Args {
        match parse_args(&argv(items)) {
            Ok(Parsed::Run(args)) => *args,
            Ok(Parsed::Help) => panic!("期望 Run，得到 Help"),
            Err(error) => panic!("解析失败：{}", error.0),
        }
    }

    fn parse_err(items: &[&str]) -> String {
        match parse_args(&argv(items)) {
            Err(error) => error.0,
            Ok(Parsed::Help) => panic!("期望失败，得到 Help"),
            Ok(Parsed::Run(_)) => panic!("期望失败，但解析成功"),
        }
    }

    #[test]
    fn bare_invocation_is_stdio() {
        let args = parse_ok(&[]);
        assert_eq!(args.mode, Mode::Stdio);
        assert!(args.router_addr.is_none());
        assert_eq!(args.worker_id, "pc-worker");
    }

    #[test]
    fn explicit_stdio_subcommand() {
        let args = parse_ok(&["stdio", "--worker-id", "w1"]);
        assert_eq!(args.mode, Mode::Stdio);
        assert_eq!(args.worker_id, "w1");
    }

    #[test]
    fn connect_subcommand_sets_tcp_and_addr() {
        let args = parse_ok(&["connect", "127.0.0.1:7421", "--worker-id", "desk-1"]);
        assert_eq!(args.mode, Mode::Tcp);
        assert_eq!(args.router_addr.as_deref(), Some("127.0.0.1:7421"));
        assert_eq!(args.worker_id, "desk-1");
    }

    #[test]
    fn legacy_flags_still_work() {
        let args = parse_ok(&["--mode", "tcp", "--router-addr", "10.0.0.5:9000"]);
        assert_eq!(args.mode, Mode::Tcp);
        assert_eq!(args.router_addr.as_deref(), Some("10.0.0.5:9000"));

        let args = parse_ok(&["--mode", "stdio"]);
        assert_eq!(args.mode, Mode::Stdio);

        // --router-addr 隐含 tcp。
        let args = parse_ok(&["--router-addr", "127.0.0.1:1"]);
        assert_eq!(args.mode, Mode::Tcp);
    }

    #[test]
    fn subcommand_and_mode_must_agree() {
        let message = parse_err(&["connect", "127.0.0.1:7421", "--mode", "stdio"]);
        assert!(message.contains("不一致"), "{message}");
        // 一致时不冲突。
        let args = parse_ok(&["connect", "127.0.0.1:7421", "--mode", "tcp"]);
        assert_eq!(args.mode, Mode::Tcp);
    }

    #[test]
    fn tcp_requires_address() {
        let message = parse_err(&["--mode", "tcp"]);
        assert!(message.contains("地址"), "{message}");
    }

    #[test]
    fn help_is_recognized() {
        for flag in ["help", "-h", "--help"] {
            assert!(matches!(parse_args(&argv(&[flag])), Ok(Parsed::Help)));
        }
    }

    #[test]
    fn invalid_values_are_rejected() {
        assert!(parse_args(&argv(&["connect", "bad addr"])).is_err());
        assert!(parse_args(&argv(&["--family", "avr"])).is_err());
        let message = parse_err(&["--boot-id", "0"]);
        assert!(message.contains("不能为 0"), "{message}");
        assert!(parse_args(&argv(&["--nope"])).is_err());
        // 子命令重复。
        assert!(parse_args(&argv(&["stdio", "connect", "127.0.0.1:1"])).is_err());
    }

    // ---- connect 模式阶段文案 ----

    #[test]
    fn connecting_line_shows_address_and_attempt() {
        assert_eq!(
            connecting_line("127.0.0.1:7421", 1),
            "sekai-worker: 正在连接 127.0.0.1:7421 …（第 1 次）"
        );
        let line = connecting_line("10.0.0.5:9000", 3);
        assert!(line.starts_with("sekai-worker: "), "{line}");
        assert!(line.contains("10.0.0.5:9000"), "{line}");
        assert!(line.contains("（第 3 次）"), "{line}");
    }

    #[test]
    fn connected_and_ready_lines_are_stable() {
        assert_eq!(
            connected_line("127.0.0.1:7421"),
            "sekai-worker: TCP 已连接 127.0.0.1:7421"
        );
        assert_eq!(link_ready_line(), "sekai-worker: Hello 已发布，链路已就绪");
    }

    #[test]
    fn reconnect_wait_line_formats_seconds_and_attempt() {
        assert_eq!(
            reconnect_wait_line(3_000, 2),
            "sekai-worker: 3 秒后第 2 次重连…"
        );
        // 亚秒退避（500ms）显示为 0.5。
        assert_eq!(
            reconnect_wait_line(500, 2),
            "sekai-worker: 0.5 秒后第 2 次重连…"
        );
        assert_eq!(
            reconnect_wait_line(30_000, 7),
            "sekai-worker: 30 秒后第 7 次重连…"
        );
    }

    #[test]
    fn failure_report_keeps_hint_and_raw_error() {
        let error = LinkError::Connect(String::from("Connection refused (os error 111)"));
        let report = failure_report("连接失败", connect_hint(&error), &error);
        // 提示行：可操作建议。
        assert!(report.contains("sekai-worker: 提示："), "{report}");
        assert!(report.contains("sekai-router"), "{report}");
        assert!(report.contains("/link tcp"), "{report}");
        // 原始错误行：诊断信息不丢。
        assert!(report.contains("sekai-worker: 连接失败："), "{report}");
        assert!(report.contains("os error 111"), "{report}");
    }
}
