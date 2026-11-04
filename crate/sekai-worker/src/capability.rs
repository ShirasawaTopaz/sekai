//! PC Worker 的宿主能力：系统时钟 + 能力注册（PC agent 工具/任务 + 可选 virtual GPIO）。
//!
//! 默认能力是 **PC agent**（不再默认注册 virtual GPIO）：
//!
//! - 工具（同步、短小）：`pc.exec`（跨平台 shell，内部超时）、`pc.read` /
//!   `pc.write`（文件读写）、`pc.ls`（目录列举）、`pc.info`（环境摘要）；
//! - 任务（协作式 poll，不长时间阻塞）：`pc.job`（子进程命令，`try_wait`
//!   推进、DDL 到期杀进程）、`pc.ask`（经 [`PromptTransport`] 取 LLM 回答，
//!   阻塞调用放在独立线程，poll 非阻塞收结果）；保留 `echo` / `sleep`
//!   两个示范任务。
//!
//! virtual GPIO 改为 **opt-in**：CLI `--gpio esp32|stm32f1|stm32f4`（兼容别名
//! `--family`）显式开启，经 [`agent_registries_for`] 在 agent 能力之上追加
//! `gpio.*` 工具；[`host_registries`] 原样保留（gpio.* + echo/sleep，供
//! Router 侧环回测试等既有用法）。
//!
//! 输出预算：工具/任务结果统一 ≤ `MAX_PAYLOAD`（4096 字节，按 UTF-8 字符
//! 边界截断并追加 `[[truncated]]` 标记行；超时追加 `[[timeout after Nms]]`），
//! 避免 Stdout 编码时的 ResultTooLarge。

use std::cell::Cell;
use std::io::Read;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use sekai::sekai_clock::Clock;
use sekai::sekai_task::{TaskContext, TaskHandler, TaskPoll, TaskRegistry};
use sekai::sekai_tool::{ToolContext, ToolHandler, ToolRegistry};
use sekai::sekai_wire::{CommandError, ErrorCode, MAX_PAYLOAD};
use sekai_gpio::{ChipFamily, VirtualGpio, register_tools};

use crate::llm_channel::{PromptError, PromptTransport, ask_timeout_ms, env_clamped_ms};

// ---------------------------------------------------------------------------
// 系统时钟（原样保留）
// ---------------------------------------------------------------------------

/// 宿主系统时钟：单调时间取 `Instant`，墙上时间取 `SystemTime`。
///
/// [`Clock::calibrate_unix_ms`]（`sekai.set_time` 内置工具触发）通过维护一个
/// 校准偏移生效，且不影响单调时间基准——与 `sekai_clock` 的时间约定一致。
#[derive(Debug, Clone)]
pub struct SystemClock {
    started: Instant,
    offset_ms: Cell<i128>,
}

impl SystemClock {
    /// 以当前时刻创建。
    pub fn new() -> Self {
        Self {
            started: Instant::now(),
            offset_ms: Cell::new(0),
        }
    }

    /// 未校准时的墙上时间（Unix 毫秒）；取不到系统时间时返回 0。
    fn raw_unix_ms(&self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_millis() as u64)
            .unwrap_or(0)
    }
}

impl Default for SystemClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for SystemClock {
    fn monotonic_ms(&self) -> u64 {
        self.started.elapsed().as_millis() as u64
    }

    fn unix_ms(&self) -> Option<u64> {
        let raw = self.raw_unix_ms() as i128 + self.offset_ms.get();
        u64::try_from(raw.max(0)).ok()
    }

    fn calibrate_unix_ms(&self, unix_ms: u64) {
        self.offset_ms
            .set(unix_ms as i128 - self.raw_unix_ms() as i128);
    }
}

// ---------------------------------------------------------------------------
// PC agent 能力注册表
// ---------------------------------------------------------------------------

/// LLM prompt 通道模式（CLI `--llm` 与 `pc.info` 的 `llm=` 行）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LlmMode {
    /// TCP JSONL 到 Router ingress（默认 connect 模式）。
    Router,
    /// 进程 stdin/stdout JSONL，经安装本 Worker 的 DSH 插件中转。
    Dsh,
    /// 未启用（`pc.ask` 调用即失败并给提示）。
    Off,
}

impl LlmMode {
    /// 解析 CLI 取值（`router` / `dsh` / `off`）。
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "router" => Some(Self::Router),
            "dsh" => Some(Self::Dsh),
            "off" => Some(Self::Off),
            _ => None,
        }
    }

    /// 标签文本（`pc.info` 输出用）。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Router => "router",
            Self::Dsh => "dsh",
            Self::Off => "off",
        }
    }
}

/// gpio 家族的 `pc.info` 标签。
fn gpio_label(gpio: Option<ChipFamily>) -> &'static str {
    match gpio {
        Some(ChipFamily::Esp32) => "esp32",
        Some(ChipFamily::Stm32F1) => "stm32f1",
        Some(ChipFamily::Stm32F4) => "stm32f4",
        None => "none",
    }
}

/// 构造 PC Worker 的默认 agent 能力：pc.* 工具 + echo/sleep/pc.job/pc.ask 任务。
///
/// `llm` 为 `pc.ask` 使用的 prompt 通道；`None` 时 `pc.ask` 仍注册，调用即
/// 失败并给出 `--llm` 提示。注意：本入口无法区分通道具体类型，
/// `pc.info` 的 `llm=` 行在有通道时统一标 `router`；CLI 组装请用
/// [`agent_registries_for`] 传入真实模式与可选 gpio。
pub fn agent_registries(
    llm: Option<Arc<dyn PromptTransport>>,
) -> Result<(TaskRegistry, ToolRegistry), String> {
    let mode = if llm.is_some() {
        LlmMode::Router
    } else {
        LlmMode::Off
    };
    agent_registries_for(llm, mode, None)
}

/// 组装完整能力注册表：PC agent 能力（pc.* 工具 + 四个任务）+ 可选 gpio 工具。
///
/// - `mode` 决定 `pc.info` 的 `llm=` 行标签；
/// - `gpio` 为 `Some` 时在 agent 工具之上追加 `gpio.*`（`--gpio` opt-in）。
pub fn agent_registries_for(
    llm: Option<Arc<dyn PromptTransport>>,
    mode: LlmMode,
    gpio: Option<ChipFamily>,
) -> Result<(TaskRegistry, ToolRegistry), String> {
    let mut tools = ToolRegistry::new();
    tools
        .register("pc.exec", PcExecTool)
        .map_err(|error| format!("注册 pc.exec 失败：{error}"))?;
    tools
        .register("pc.read", PcReadTool)
        .map_err(|error| format!("注册 pc.read 失败：{error}"))?;
    tools
        .register("pc.write", PcWriteTool)
        .map_err(|error| format!("注册 pc.write 失败：{error}"))?;
    tools
        .register("pc.ls", PcLsTool)
        .map_err(|error| format!("注册 pc.ls 失败：{error}"))?;
    tools
        .register(
            "pc.info",
            PcInfoTool {
                llm: mode.as_str(),
                gpio: gpio_label(gpio),
            },
        )
        .map_err(|error| format!("注册 pc.info 失败：{error}"))?;
    if let Some(family) = gpio {
        register_tools(&mut tools, VirtualGpio::new(family))
            .map_err(|error| format!("注册 gpio 工具失败：{error}"))?;
    }

    let mut tasks = TaskRegistry::new();
    tasks
        .register("echo", |args: &[u8]| {
            Ok(Box::new(EchoTask {
                args: args.to_vec(),
            }) as Box<dyn TaskHandler>)
        })
        .map_err(|error| format!("注册 echo 任务失败：{error}"))?;
    tasks
        .register("sleep", |args: &[u8]| {
            let text = core::str::from_utf8(args).map_err(|_| {
                CommandError::new(ErrorCode::InvalidArgument, "sleep 参数必须是毫秒文本")
            })?;
            let millis: u64 = text.trim().parse().map_err(|_| {
                CommandError::new(ErrorCode::InvalidArgument, "sleep 参数必须是毫秒文本")
            })?;
            if millis > 3_600_000 {
                return Err(CommandError::new(
                    ErrorCode::InvalidArgument,
                    "sleep 时长不能超过 1 小时",
                ));
            }
            Ok(Box::new(SleepTask {
                until_ms: None,
                duration_ms: millis,
            }) as Box<dyn TaskHandler>)
        })
        .map_err(|error| format!("注册 sleep 任务失败：{error}"))?;
    tasks
        .register("pc.job", |args: &[u8]| {
            let text = core::str::from_utf8(args).map_err(|_| {
                CommandError::new(ErrorCode::InvalidArgument, "pc.job 参数必须是 UTF-8 文本")
            })?;
            let command = text.trim();
            if command.is_empty() {
                return Err(CommandError::new(
                    ErrorCode::InvalidArgument,
                    "pc.job 参数为空：应提供命令文本（如 echo hello）",
                ));
            }
            let mut child = spawn_shell(command).map_err(|error| {
                CommandError::new(
                    ErrorCode::InvalidArgument,
                    format!("pc.job 启动命令失败：{error}"),
                )
            })?;
            let sink = OutputSink::new(JOB_SINK_CAP, true);
            spawn_pipe_pump(child.stdout.take().expect("stdout 已 piped"), sink.clone());
            spawn_pipe_pump(child.stderr.take().expect("stderr 已 piped"), sink.clone());
            Ok(Box::new(JobTask {
                child: Some(child),
                sink,
            }) as Box<dyn TaskHandler>)
        })
        .map_err(|error| format!("注册 pc.job 任务失败：{error}"))?;
    let llm_for_task = llm;
    tasks
        .register("pc.ask", move |args: &[u8]| {
            let text = core::str::from_utf8(args).map_err(|_| {
                CommandError::new(ErrorCode::InvalidArgument, "pc.ask 参数必须是 UTF-8 文本")
            })?;
            let prompt = text.trim();
            if prompt.is_empty() {
                return Err(CommandError::new(
                    ErrorCode::InvalidArgument,
                    "pc.ask 参数为空：应提供 prompt 文本",
                ));
            }
            let Some(transport) = llm_for_task.clone() else {
                return Err(CommandError::new(
                    ErrorCode::InvalidArgument,
                    "llm 通道未启用：pc.ask 需要 prompt 通道，\
                     用 --llm router（经 Router 调 LLM）或 --llm dsh（经 DSH 插件中转）\
                     启动 sekai-worker 后重试",
                ));
            };
            // 阻塞的 ask 放独立线程；poll 只做非阻塞收结果。
            // prompt 先拥有化（args 借用不能逃出工厂闭包）。
            let prompt = String::from(prompt);
            let (sender, receiver) = mpsc::channel();
            thread::spawn(move || {
                let result = transport.ask(&prompt);
                let _ = sender.send(result);
            });
            Ok(Box::new(AskTask {
                receiver,
                started: Instant::now(),
                timeout_ms: ask_timeout_ms(),
            }) as Box<dyn TaskHandler>)
        })
        .map_err(|error| format!("注册 pc.ask 任务失败：{error}"))?;
    Ok((tasks, tools))
}

/// 构造 PC Worker 的能力注册表：virtual GPIO 工具 + `echo`/`sleep` 任务。
///
/// 原样保留（Router 侧环回测试等既有用法）。默认能力请用
/// [`agent_registries`] / [`agent_registries_for`]；CLI 的 `--gpio` 在 agent
/// 能力之上追加 gpio 工具，等价于 `agent_registries_for(.., Some(family))`。
///
/// `family` 决定虚拟 GPIO 的引脚表（`esp32` / `stm32f1` / `stm32f4`）。
pub fn host_registries(family: ChipFamily) -> Result<(TaskRegistry, ToolRegistry), String> {
    let mut tools = ToolRegistry::new();
    register_tools(&mut tools, VirtualGpio::new(family))
        .map_err(|error| format!("注册 gpio 工具失败：{error}"))?;

    let mut tasks = TaskRegistry::new();
    tasks
        .register("echo", |args: &[u8]| {
            Ok(Box::new(EchoTask {
                args: args.to_vec(),
            }) as Box<dyn TaskHandler>)
        })
        .map_err(|error| format!("注册 echo 任务失败：{error}"))?;
    tasks
        .register("sleep", |args: &[u8]| {
            let text = core::str::from_utf8(args).map_err(|_| {
                CommandError::new(ErrorCode::InvalidArgument, "sleep 参数必须是毫秒文本")
            })?;
            let millis: u64 = text.trim().parse().map_err(|_| {
                CommandError::new(ErrorCode::InvalidArgument, "sleep 参数必须是毫秒文本")
            })?;
            if millis > 3_600_000 {
                return Err(CommandError::new(
                    ErrorCode::InvalidArgument,
                    "sleep 时长不能超过 1 小时",
                ));
            }
            Ok(Box::new(SleepTask {
                until_ms: None,
                duration_ms: millis,
            }) as Box<dyn TaskHandler>)
        })
        .map_err(|error| format!("注册 sleep 任务失败：{error}"))?;
    Ok((tasks, tools))
}

// ---------------------------------------------------------------------------
// 输出预算与组装
// ---------------------------------------------------------------------------

/// `pc.exec` 内部超时（env `SEKAI_PC_EXEC_MS`，100..=300000）。
fn exec_timeout_ms() -> u64 {
    env_clamped_ms("SEKAI_PC_EXEC_MS", 10_000, 100, 300_000)
}

/// `pc.read` 的内容上限（字节）。
const MAX_READ_BYTES: usize = 4_000;

/// `pc.ls` 的行数上限。
const MAX_LS_LINES: usize = 200;

/// `pc.exec` 的累积缓冲上限（最终结果仍截到 `MAX_PAYLOAD`）。
const EXEC_SINK_CAP: usize = 64 * 1024;

/// `pc.job` 的累积缓冲上限：预留 `exit=N\n` 头行与 `[[truncated]]` 标记行的
/// 空间，使最终组装恒 ≤ `MAX_PAYLOAD`，尾部不再被二次截断。
const JOB_SINK_CAP: usize = MAX_PAYLOAD - 32;

/// 子进程状态轮询间隔（毫秒）。
const PROCESS_POLL_MS: u64 = 20;

/// 任务 Pending 时建议的下次 poll 间隔（毫秒）。
const JOB_POLL_MS: u32 = 20;
const ASK_POLL_MS: u32 = 50;

/// 进程结束后等待管道吸干的上限（毫秒）。
const PUMP_GRACE_MS: u64 = 200;

/// 截断标记行。
const MARKER_TRUNCATED: &str = "[[truncated]]";

/// 按 UTF-8 字符边界把字节切片截到 ≤ `limit` 字节；返回 (切片, 是否截断)。
fn truncate_utf8(bytes: &[u8], limit: usize) -> (&[u8], bool) {
    if bytes.len() <= limit {
        return (bytes, false);
    }
    // 回退到字符首字节：UTF-8 续字节形如 10xxxxxx。
    let mut end = limit.min(bytes.len());
    while end > 0 && (bytes[end] & 0xC0) == 0x80 {
        end -= 1;
    }
    (&bytes[..end], true)
}

/// 组装 `head + body`，总长 ≤ `limit`（UTF-8 字符边界截断 body）。
///
/// - `body_truncated`：累积阶段已丢过字节（尾部滑窗等）→ 追加
///   `[[truncated]]` 标记行；
/// - `timed_out_ms`：内部超时 → 追加 `[[timeout after Nms]]` 标记行；
/// - 超预算时截断 body（同样追加 `[[truncated]]`）。
fn assemble_bounded(
    head: &str,
    body: &[u8],
    body_truncated: bool,
    timed_out_ms: Option<u64>,
    limit: usize,
) -> Vec<u8> {
    let timeout_line = timed_out_ms.map(|ms| format!("\n[[timeout after {ms}ms]]"));
    let truncated_line = "\n[[truncated]]";
    let marker_len = usize::from(body_truncated) * truncated_line.len()
        + timeout_line.as_deref().map_or(0, |line| line.len());
    if head.len() + body.len() + marker_len <= limit {
        let mut out = Vec::with_capacity(head.len() + body.len() + marker_len);
        out.extend_from_slice(head.as_bytes());
        out.extend_from_slice(body);
        if body_truncated {
            out.extend_from_slice(truncated_line.as_bytes());
        }
        if let Some(line) = &timeout_line {
            out.extend_from_slice(line.as_bytes());
        }
        return out;
    }
    // 超预算：截断 body，头部与标记行完整保留。
    let budget = limit.saturating_sub(
        head.len() + truncated_line.len() + timeout_line.as_deref().map_or(0, |line| line.len()),
    );
    let (kept, _) = truncate_utf8(body, budget);
    let mut out = Vec::with_capacity(limit);
    out.extend_from_slice(head.as_bytes());
    out.extend_from_slice(kept);
    out.extend_from_slice(truncated_line.as_bytes());
    if let Some(line) = &timeout_line {
        out.extend_from_slice(line.as_bytes());
    }
    out
}

// ---------------------------------------------------------------------------
// 子进程基础设施（pc.exec 工具 / pc.job 任务共用）
// ---------------------------------------------------------------------------

/// 跨平台 shell 启动：Windows `cmd /C <command>`，其余 `sh -c <command>`。
fn spawn_shell(command: &str) -> std::io::Result<Child> {
    let mut builder = if cfg!(windows) {
        let mut builder = Command::new("cmd");
        builder.arg("/C").arg(command);
        builder
    } else {
        let mut builder = Command::new("sh");
        builder.arg("-c").arg(command);
        builder
    };
    let mut child = builder
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    drop(child.stdin.take()); // 关闭 stdin：命令不应等待交互输入。
    Ok(child)
}

/// 子进程输出累积缓冲：读线程并发追加；达到 `cap` 后按模式保留头部或尾部。
struct OutputSink {
    data: Mutex<Vec<u8>>,
    truncated: AtomicBool,
    closed: AtomicUsize,
    cap: usize,
    keep_tail: bool,
}

impl OutputSink {
    /// `keep_tail=false`（pc.exec，取头部）或 `true`（pc.job，只留尾部）。
    fn new(cap: usize, keep_tail: bool) -> Arc<Self> {
        Arc::new(Self {
            data: Mutex::new(Vec::new()),
            truncated: AtomicBool::new(false),
            closed: AtomicUsize::new(0),
            cap,
            keep_tail,
        })
    }

    /// 追加一批输出；可能整块丢弃/滑窗，标记 `truncated`。
    fn push(&self, bytes: &[u8]) {
        let mut data = self.data.lock().unwrap();
        if self.keep_tail {
            data.extend_from_slice(bytes);
            if data.len() > self.cap {
                let mut start = data.len() - self.cap;
                // 头部落点若是 UTF-8 续字节则右移到字符边界。
                while start < data.len() && (data[start] & 0xC0) == 0x80 {
                    start += 1;
                }
                data.drain(..start);
                self.truncated.store(true, Ordering::Relaxed);
            }
        } else if data.len() >= self.cap {
            self.truncated.store(true, Ordering::Relaxed);
        } else if data.len() + bytes.len() > self.cap {
            let room = self.cap - data.len();
            data.extend_from_slice(&bytes[..room]);
            self.truncated.store(true, Ordering::Relaxed);
        } else {
            data.extend_from_slice(bytes);
        }
    }

    /// 取走当前累积内容与「是否丢过字节」标记。
    fn take(&self) -> (Vec<u8>, bool) {
        let data = std::mem::take(&mut *self.data.lock().unwrap());
        (data, self.truncated.load(Ordering::Relaxed))
    }

    /// 登记一个泵线程结束。
    fn close_pump(&self) {
        self.closed.fetch_add(1, Ordering::Relaxed);
    }

    /// 有界等待全部（`pumps` 个）泵线程结束；超时返回 false（读线程自然退出，
    /// 不阻塞调用方——孙进程可能还握着管道）。
    fn wait_pumps(&self, pumps: usize, upto: Duration) -> bool {
        let deadline = Instant::now() + upto;
        while self.closed.load(Ordering::Relaxed) < pumps {
            if Instant::now() >= deadline {
                return false;
            }
            thread::sleep(Duration::from_millis(5));
        }
        true
    }
}

/// 把一个管道的输出泵进 sink（独立线程；进程退出/被杀后管道 EOF 自然结束）。
fn spawn_pipe_pump<R: Read + Send + 'static>(mut reader: R, sink: Arc<OutputSink>) {
    thread::spawn(move || {
        let mut buffer = [0u8; 4096];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) | Err(_) => break,
                Ok(n) => sink.push(&buffer[..n]),
            }
        }
        sink.close_pump();
    });
}

/// 从 args 解析「首行路径」（整体 trim；空路径报错）。
fn args_path(args: &[u8]) -> Result<String, CommandError> {
    let text = core::str::from_utf8(args)
        .map_err(|_| CommandError::new(ErrorCode::InvalidArgument, "参数必须是 UTF-8 文本"))?;
    let first_line = text.split('\n').next().unwrap_or("").trim();
    if first_line.is_empty() {
        return Err(CommandError::new(
            ErrorCode::InvalidArgument,
            "首行路径为空：应在首行提供文件/目录路径",
        ));
    }
    Ok(String::from(first_line))
}

// ---------------------------------------------------------------------------
// pc.* 工具
// ---------------------------------------------------------------------------

/// `pc.exec`：跨平台 shell 执行，内部超时，stdout+stderr 合并输出。
struct PcExecTool;

impl ToolHandler for PcExecTool {
    fn call(&mut self, ctx: &mut ToolContext<'_>) -> Result<Vec<u8>, CommandError> {
        let text = core::str::from_utf8(ctx.args).map_err(|_| {
            CommandError::new(ErrorCode::InvalidArgument, "pc.exec 参数必须是 UTF-8 文本")
        })?;
        let command = text.trim();
        if command.is_empty() {
            return Err(CommandError::new(
                ErrorCode::InvalidArgument,
                "pc.exec 参数为空：应提供命令文本（如 echo hello）",
            ));
        }
        let timeout_ms = exec_timeout_ms();
        let mut child = spawn_shell(command).map_err(|error| {
            CommandError::new(
                ErrorCode::InvalidArgument,
                format!("pc.exec 启动命令失败：{error}"),
            )
        })?;
        let sink = OutputSink::new(EXEC_SINK_CAP, false);
        spawn_pipe_pump(child.stdout.take().expect("stdout 已 piped"), sink.clone());
        spawn_pipe_pump(child.stderr.take().expect("stderr 已 piped"), sink.clone());

        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        let mut timed_out = false;
        let code = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status.code().unwrap_or(-1),
                Ok(None) => {
                    if Instant::now() >= deadline {
                        timed_out = true;
                        let _ = child.kill();
                        break code_after_kill(&mut child);
                    }
                    thread::sleep(Duration::from_millis(PROCESS_POLL_MS));
                }
                Err(error) => {
                    return Err(CommandError::new(
                        ErrorCode::Internal,
                        format!("pc.exec 等待命令失败：{error}"),
                    ));
                }
            }
        };
        // 等管道吸干（有界；被杀的孙进程可能还握着管道，不无限等）。
        sink.wait_pumps(2, Duration::from_millis(PUMP_GRACE_MS));
        let (body, truncated) = sink.take();
        Ok(assemble_bounded(
            &format!("exit={code}\n"),
            &body,
            truncated,
            timed_out.then_some(timeout_ms),
            MAX_PAYLOAD,
        ))
    }
}

/// 杀死后收割退出码（取不到给 -1）。
fn code_after_kill(child: &mut Child) -> i32 {
    child
        .wait()
        .ok()
        .and_then(|status| status.code())
        .unwrap_or(-1)
}

/// `pc.read`：读文件，≤4000 字节 + `[[truncated]]`。
struct PcReadTool;

impl ToolHandler for PcReadTool {
    fn call(&mut self, ctx: &mut ToolContext<'_>) -> Result<Vec<u8>, CommandError> {
        let path = args_path(ctx.args)?;
        let bytes = std::fs::read(&path).map_err(|error| {
            CommandError::new(
                ErrorCode::InvalidArgument,
                format!("pc.read 无法读取 {path}：{error}"),
            )
        })?;
        let (kept, truncated) = truncate_utf8(&bytes, MAX_READ_BYTES);
        let mut out = kept.to_vec();
        if truncated {
            out.push(b'\n');
            out.extend_from_slice(MARKER_TRUNCATED.as_bytes());
        }
        Ok(out)
    }
}

/// `pc.write`：首行路径 + 其余全部内容，覆盖写（不建父目录），输出字节数。
struct PcWriteTool;

impl ToolHandler for PcWriteTool {
    fn call(&mut self, ctx: &mut ToolContext<'_>) -> Result<Vec<u8>, CommandError> {
        let text = core::str::from_utf8(ctx.args).map_err(|_| {
            CommandError::new(ErrorCode::InvalidArgument, "pc.write 参数必须是 UTF-8 文本")
        })?;
        let (path_line, content) = match text.split_once('\n') {
            Some((path_line, content)) => (path_line, content),
            None => (text, ""),
        };
        let path = path_line.trim();
        if path.is_empty() {
            return Err(CommandError::new(
                ErrorCode::InvalidArgument,
                "pc.write 首行路径为空：应在首行提供文件路径",
            ));
        }
        std::fs::write(path, content.as_bytes()).map_err(|error| {
            CommandError::new(
                ErrorCode::InvalidArgument,
                format!("pc.write 无法写入 {path}：{error}（父目录不存在时不会自动创建）"),
            )
        })?;
        Ok(format!("written={}", content.len()).into_bytes())
    }
}

/// `pc.ls`：列举目录，`d <name>` / `f <name>` 每行一项，≤200 行。
struct PcLsTool;

impl ToolHandler for PcLsTool {
    fn call(&mut self, ctx: &mut ToolContext<'_>) -> Result<Vec<u8>, CommandError> {
        let path = args_path(ctx.args)?;
        let entries = std::fs::read_dir(&path).map_err(|error| {
            CommandError::new(
                ErrorCode::InvalidArgument,
                format!("pc.ls 无法列举 {path}：{error}"),
            )
        })?;
        let mut rows: Vec<(bool, String)> = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|error| {
                CommandError::new(
                    ErrorCode::Internal,
                    format!("pc.ls 读取目录项失败：{error}"),
                )
            })?;
            let is_dir = entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false);
            rows.push((is_dir, entry.file_name().to_string_lossy().into_owned()));
        }
        rows.sort_by(|a, b| a.1.cmp(&b.1));
        let mut out = String::new();
        for (index, (is_dir, name)) in rows.iter().enumerate() {
            if index >= MAX_LS_LINES {
                out.push_str(MARKER_TRUNCATED);
                out.push('\n');
                break;
            }
            out.push_str(if *is_dir { "d " } else { "f " });
            out.push_str(name);
            out.push('\n');
        }
        Ok(out.into_bytes())
    }
}

/// `pc.info`：环境摘要（os/host/cwd/pid/llm/gpio）。
struct PcInfoTool {
    llm: &'static str,
    gpio: &'static str,
}

impl ToolHandler for PcInfoTool {
    fn call(&mut self, _ctx: &mut ToolContext<'_>) -> Result<Vec<u8>, CommandError> {
        let host = std::env::var("COMPUTERNAME")
            .or_else(|_| std::env::var("HOSTNAME"))
            .unwrap_or_else(|_| String::from("unknown"));
        let cwd = std::env::current_dir()
            .map(|path| path.to_string_lossy().into_owned())
            .unwrap_or_else(|_| String::from("unknown"));
        let text = format!(
            "os={}\nhost={}\ncwd={}\npid={}\nllm={}\ngpio={}\n",
            std::env::consts::OS,
            host,
            cwd,
            std::process::id(),
            self.llm,
            self.gpio,
        );
        Ok(text.into_bytes())
    }
}

// ---------------------------------------------------------------------------
// 任务
// ---------------------------------------------------------------------------

/// `echo` 任务：一个 poll 周期内原样返回参数。
struct EchoTask {
    args: Vec<u8>,
}

impl TaskHandler for EchoTask {
    fn poll(&mut self, ctx: &mut TaskContext<'_>) -> TaskPoll {
        if ctx.is_expired() {
            return TaskPoll::Failed(CommandError::from(ErrorCode::Timeout));
        }
        TaskPoll::Done(self.args.clone())
    }
}

/// `sleep` 任务：协作式等待指定毫秒后完成，验证 Pending→Done 的多周期推进。
struct SleepTask {
    until_ms: Option<u64>,
    duration_ms: u64,
}

impl TaskHandler for SleepTask {
    fn poll(&mut self, ctx: &mut TaskContext<'_>) -> TaskPoll {
        let until = *self
            .until_ms
            .get_or_insert_with(|| ctx.now_mono_ms + self.duration_ms);
        if ctx.now_mono_ms >= until {
            TaskPoll::Done(Vec::new())
        } else {
            TaskPoll::Pending {
                wake_after_ms: (until - ctx.now_mono_ms).min(u32::MAX as u64) as u32,
            }
        }
    }
}

/// `pc.job` 任务：子进程命令，`try_wait` 非阻塞推进；DDL 到期杀进程。
///
/// 输出只保留尾部（≤ `MAX_PAYLOAD`），完成结果 `exit=<code>\n` + 输出尾部。
struct JobTask {
    child: Option<Child>,
    sink: Arc<OutputSink>,
}

impl JobTask {
    /// 杀掉并收割子进程（幂等）。
    fn kill_child(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl Drop for JobTask {
    fn drop(&mut self) {
        // 任务被运行时丢弃（DDL/取消）时的兜底：不留孤儿进程。
        self.kill_child();
    }
}

impl TaskHandler for JobTask {
    fn poll(&mut self, ctx: &mut TaskContext<'_>) -> TaskPoll {
        if let Some(child) = self.child.as_mut() {
            match child.try_wait() {
                Ok(Some(status)) => {
                    let code = status.code().unwrap_or(-1);
                    self.child = None; // try_wait 已收割，仅放弃句柄。
                    // 等管道吸干（有界）；被杀的孙进程不无限等。
                    self.sink
                        .wait_pumps(2, Duration::from_millis(PUMP_GRACE_MS));
                    let (body, truncated) = self.sink.take();
                    return TaskPoll::Done(assemble_bounded(
                        &format!("exit={code}\n"),
                        &body,
                        truncated,
                        None,
                        MAX_PAYLOAD,
                    ));
                }
                Ok(None) => {}
                Err(error) => {
                    self.kill_child();
                    return TaskPoll::Failed(CommandError::new(
                        ErrorCode::Internal,
                        format!("pc.job 等待命令失败：{error}"),
                    ));
                }
            }
        }
        if ctx.is_expired() {
            self.kill_child();
            return TaskPoll::Failed(CommandError::new(
                ErrorCode::Timeout,
                "pc.job 超过 DDL：命令进程已被终止",
            ));
        }
        TaskPoll::Pending {
            wake_after_ms: JOB_POLL_MS,
        }
    }
}

/// `pc.ask` 任务：经 prompt 通道取 LLM 回答；阻塞 ask 在独立线程，poll 只收结果。
struct AskTask {
    receiver: mpsc::Receiver<Result<String, PromptError>>,
    started: Instant,
    timeout_ms: u64,
}

impl TaskHandler for AskTask {
    fn poll(&mut self, ctx: &mut TaskContext<'_>) -> TaskPoll {
        // DDL 兜底（运行时本身也会在 poll 之间做 DDL 检查）。
        if ctx.is_expired() {
            return TaskPoll::Failed(CommandError::from(ErrorCode::Timeout));
        }
        // 内部硬超时（通道自身也有同预算超时，这里是任务侧兜底）。
        if self.started.elapsed() >= Duration::from_millis(self.timeout_ms) {
            return TaskPoll::Failed(CommandError::new(
                ErrorCode::Timeout,
                format!("pc.ask 等待回答超过内部上限（{}ms）", self.timeout_ms),
            ));
        }
        match self.receiver.try_recv() {
            Ok(Ok(answer)) => TaskPoll::Done(assemble_bounded(
                "",
                answer.as_bytes(),
                false,
                None,
                MAX_PAYLOAD,
            )),
            Ok(Err(error)) => TaskPoll::Failed(prompt_error_to_command(error)),
            Err(mpsc::TryRecvError::Empty) => TaskPoll::Pending {
                wake_after_ms: ASK_POLL_MS,
            },
            Err(mpsc::TryRecvError::Disconnected) => TaskPoll::Failed(CommandError::new(
                ErrorCode::Internal,
                "pc.ask 内部通道异常关闭（工作线程未返回结果）",
            )),
        }
    }
}

/// 通道错误 → 命令错误：`timeout` → Timeout，其余 → InvalidArgument；
/// 消息保留原始 code/message 并带 `--llm` 可操作提示。
fn prompt_error_to_command(error: PromptError) -> CommandError {
    if error.code == "timeout" {
        CommandError::new(
            ErrorCode::Timeout,
            format!(
                "pc.ask 等待回答超时：{}（可检查 Router prompt 通道或 DSH 插件是否可达）",
                error.message
            ),
        )
    } else {
        CommandError::new(
            ErrorCode::InvalidArgument,
            format!(
                "pc.ask 通道失败（{}）：{}（用 --llm router|dsh 启用 prompt 通道后重试）",
                error.code, error.message
            ),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sekai::sekai_tool::{ToolContext, ToolRegistry};
    use std::path::{Path, PathBuf};
    use std::sync::atomic::AtomicU64;

    /// 任务工厂应返回 Err（Box<dyn TaskHandler> 无 Debug，不能 unwrap_err）。
    fn expect_factory_err(result: Result<Box<dyn TaskHandler>, CommandError>) -> CommandError {
        match result {
            Ok(_) => panic!("任务工厂应返回 Err"),
            Err(error) => error,
        }
    }

    fn call_tool(registry: &mut ToolRegistry, name: &str, args: &[u8]) -> Result<Vec<u8>, String> {
        let tool = registry
            .get_mut(name)
            .ok_or_else(|| format!("工具 {name} 未注册"))?;
        let workers = sekai::sekai_device_list::DeviceList::new();
        let mut context = ToolContext {
            args,
            now_mono_ms: 0,
            now_unix_ms: Some(0),
            workers: &workers,
        };
        tool.call(&mut context).map_err(|error| error.to_string())
    }

    /// 测试专用临时目录（`std::env::temp_dir` + 唯一子目录；Drop 自清）。
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "sekai-worker-test-{tag}-{}-{unique}",
                std::process::id()
            ));
            std::fs::create_dir_all(&path).expect("创建临时目录失败");
            TempDir(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }

        fn join(&self, name: &str) -> PathBuf {
            self.0.join(name)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// 假 prompt 通道：立即返回预置结果。
    struct FakeTransport {
        answer: Result<String, PromptError>,
    }

    impl PromptTransport for FakeTransport {
        fn ask(&self, _prompt: &str) -> Result<String, PromptError> {
            self.answer.clone()
        }
    }

    // ---- 系统时钟（回归） ----

    #[test]
    fn system_clock_is_monotonic_and_calibratable() {
        let clock = SystemClock::new();
        let first = clock.monotonic_ms();
        assert!(clock.unix_ms().is_some());
        clock.calibrate_unix_ms(1_700_000_000_000);
        let calibrated = clock.unix_ms().unwrap();
        assert!(
            (1_700_000_000_000..=1_700_000_000_120).contains(&calibrated),
            "校准后墙上时间应接近校准值：{calibrated}"
        );
        assert!(clock.monotonic_ms() >= first);
    }

    // ---- agent 能力注册 ----

    #[test]
    fn agent_registries_register_pc_tools_and_tasks() {
        let (tasks, tools) = agent_registries(None).unwrap();
        for name in ["echo", "sleep", "pc.job", "pc.ask"] {
            assert!(tasks.contains(name), "缺少任务 {name}");
        }
        for name in ["pc.exec", "pc.read", "pc.write", "pc.ls", "pc.info"] {
            assert!(tools.contains(name), "缺少工具 {name}");
        }
        // 默认不注册 gpio。
        for name in sekai_gpio::GPIO_TOOL_NAMES {
            assert!(!tools.contains(name), "默认不应注册 {name}");
        }
    }

    #[test]
    fn pc_info_reports_all_fields() {
        let (_, mut tools) = agent_registries(None).unwrap();
        let out = call_tool(&mut tools, "pc.info", b"").unwrap();
        let text = String::from_utf8(out).unwrap();
        for prefix in ["os=", "host=", "cwd=", "pid=", "llm=", "gpio="] {
            assert!(text.contains(prefix), "缺少字段 {prefix}：{text}");
        }
        assert!(
            text.contains(&format!("os={}\n", std::env::consts::OS)),
            "{text}"
        );
        assert!(text.contains("host="), "{text}");
        assert!(text.contains("llm=off\n"), "{text}");
        assert!(text.contains("gpio=none\n"), "{text}");

        // 模式标签与 gpio opt-in 走 agent_registries_for。
        let (_, mut tools) =
            agent_registries_for(None, LlmMode::Dsh, Some(ChipFamily::Esp32)).unwrap();
        let out = call_tool(&mut tools, "pc.info", b"").unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("llm=dsh\n"), "{text}");
        assert!(text.contains("gpio=esp32\n"), "{text}");
        for name in sekai_gpio::GPIO_TOOL_NAMES {
            assert!(tools.contains(name), "缺少 gpio 工具 {name}");
        }
    }

    // ---- pc.write / pc.read 往返 ----

    #[test]
    fn pc_write_then_read_roundtrip() {
        let dir = TempDir::new("rw");
        let file = dir.join("hello.txt");
        let path = file.to_string_lossy().into_owned();
        let content = "第一行\nsecond line\n";
        let (_, mut tools) = agent_registries(None).unwrap();

        let out = call_tool(
            &mut tools,
            "pc.write",
            format!("{path}\n{content}").as_bytes(),
        )
        .unwrap();
        assert_eq!(out, format!("written={}", content.len()).into_bytes());

        let out = call_tool(&mut tools, "pc.read", format!("{path}\n").as_bytes()).unwrap();
        assert_eq!(out, content.as_bytes());

        // 路径为空 / 文件不存在 → InvalidArgument（中文消息）。
        let error = call_tool(&mut tools, "pc.read", b"\nbody").unwrap_err();
        assert!(error.contains("路径为空"), "{error}");
        let missing = dir.join("no-such-file");
        let error = call_tool(
            &mut tools,
            "pc.read",
            format!("{}\n", missing.to_string_lossy()).as_bytes(),
        )
        .unwrap_err();
        assert!(error.contains("pc.read"), "{error}");
    }

    #[test]
    fn pc_write_rejects_empty_path_and_missing_parent() {
        let (_, mut tools) = agent_registries(None).unwrap();
        let error = call_tool(&mut tools, "pc.write", b"\ncontent").unwrap_err();
        assert!(error.contains("路径为空"), "{error}");

        // 不建父目录。
        let nested = std::env::temp_dir()
            .join("sekai-worker-test-missing-parent")
            .join("x.txt");
        let error = call_tool(
            &mut tools,
            "pc.write",
            format!("{}\n", nested.to_string_lossy()).as_bytes(),
        )
        .unwrap_err();
        assert!(error.contains("pc.write"), "{error}");
        assert!(!nested.exists());
    }

    #[test]
    fn pc_read_truncates_at_4000_bytes() {
        let dir = TempDir::new("big");
        let file = dir.join("big.txt");
        std::fs::write(&file, vec![b'x'; 5_000]).unwrap();
        let (_, mut tools) = agent_registries(None).unwrap();
        let out = call_tool(
            &mut tools,
            "pc.read",
            format!("{}\n", file.to_string_lossy()).as_bytes(),
        )
        .unwrap();
        assert!(
            out.len() <= MAX_READ_BYTES + 1 + MARKER_TRUNCATED.len(),
            "{}",
            out.len()
        );
        assert!(
            out.ends_with(MARKER_TRUNCATED.as_bytes()),
            "{:?}",
            String::from_utf8_lossy(&out)
        );
        assert_eq!(&out[..MAX_READ_BYTES], &vec![b'x'; MAX_READ_BYTES][..]);
    }

    // ---- pc.ls ----

    #[test]
    fn pc_ls_lists_tempdir_sorted() {
        let dir = TempDir::new("ls");
        std::fs::write(dir.join("b.txt"), b"1").unwrap();
        std::fs::write(dir.join("a.txt"), b"1").unwrap();
        std::fs::create_dir(dir.join("sub")).unwrap();
        let (_, mut tools) = agent_registries(None).unwrap();

        let out = call_tool(
            &mut tools,
            "pc.ls",
            format!("{}\n", dir.path().to_string_lossy()).as_bytes(),
        )
        .unwrap();
        let text = String::from_utf8(out).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines, vec!["f a.txt", "f b.txt", "d sub"], "{text}");

        // 不存在的目录。
        let error = call_tool(
            &mut tools,
            "pc.ls",
            format!("{}\n", dir.join("nope").to_string_lossy()).as_bytes(),
        )
        .unwrap_err();
        assert!(error.contains("pc.ls"), "{error}");
    }

    #[test]
    fn pc_ls_caps_at_200_lines() {
        let dir = TempDir::new("many");
        for index in 0..(MAX_LS_LINES + 5) {
            std::fs::write(dir.join(&format!("f{index:03}.txt")), b"1").unwrap();
        }
        let (_, mut tools) = agent_registries(None).unwrap();
        let out = call_tool(
            &mut tools,
            "pc.ls",
            format!("{}\n", dir.path().to_string_lossy()).as_bytes(),
        )
        .unwrap();
        let text = String::from_utf8(out).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), MAX_LS_LINES + 1, "{text}");
        assert_eq!(lines[0], "f f000.txt");
        assert_eq!(lines[MAX_LS_LINES - 1], "f f199.txt");
        assert_eq!(lines[MAX_LS_LINES], MARKER_TRUNCATED);
    }

    // ---- pc.exec ----

    #[test]
    fn pc_exec_runs_echo_and_reports_exit_code() {
        let (_, mut tools) = agent_registries(None).unwrap();
        let out = call_tool(&mut tools, "pc.exec", b"  echo hello-sekai  ").unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.starts_with("exit=0\n"), "{text}");
        assert!(text.contains("hello-sekai"), "{text}");

        // 非零退出码。
        let command = if cfg!(windows) { "exit /B 3" } else { "exit 3" };
        let out = call_tool(&mut tools, "pc.exec", command.as_bytes()).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.starts_with("exit=3\n"), "{text}");
    }

    #[test]
    fn pc_exec_rejects_empty_command() {
        let (_, mut tools) = agent_registries(None).unwrap();
        let error = call_tool(&mut tools, "pc.exec", b"   ").unwrap_err();
        assert!(error.contains("pc.exec"), "{error}");
    }

    #[test]
    fn pc_exec_truncates_big_output() {
        let dir = TempDir::new("bigout");
        let file = dir.join("big.bin");
        std::fs::write(&file, vec![b'a'; 8_192]).unwrap();
        let command = if cfg!(windows) {
            format!("type {}", file.to_string_lossy())
        } else {
            format!("cat {}", file.to_string_lossy())
        };
        let (_, mut tools) = agent_registries(None).unwrap();
        let out = call_tool(&mut tools, "pc.exec", command.as_bytes()).unwrap();
        assert!(out.len() <= MAX_PAYLOAD, "{}", out.len());
        let text = String::from_utf8(out).unwrap();
        assert!(text.starts_with("exit=0\n"), "{text}");
        assert!(text.contains(MARKER_TRUNCATED), "{text}");
    }

    #[test]
    fn pc_exec_timeout_marker_is_wellformed() {
        // 直接单测组装函数：未截断时只有超时标记行，不额外加 [[truncated]]。
        let out = assemble_bounded(
            "exit=-1\n",
            b"partial output",
            false,
            Some(10_000),
            MAX_PAYLOAD,
        );
        let text = String::from_utf8(out).unwrap();
        assert!(
            text.starts_with("exit=-1\npartial output\n[[timeout after 10000ms]]"),
            "{text}"
        );
        assert!(!text.contains(MARKER_TRUNCATED), "{text}");
    }

    // ---- 组装辅助 ----

    #[test]
    fn truncate_utf8_respects_char_boundary() {
        // "a你b"：a(1) + 你(3) + b(1)。
        let bytes = "a你b".as_bytes();
        let (kept, truncated) = truncate_utf8(bytes, 4);
        assert_eq!(kept, "a你".as_bytes());
        assert!(truncated);
        let (kept, truncated) = truncate_utf8(bytes, 5);
        assert_eq!(kept, bytes);
        assert!(!truncated);
    }

    #[test]
    fn assemble_bounded_fits_limit_with_multibyte_body() {
        let body: Vec<u8> = "世界".repeat(4_000).into_bytes();
        let out = assemble_bounded("exit=0\n", &body, false, None, MAX_PAYLOAD);
        assert!(out.len() <= MAX_PAYLOAD, "{}", out.len());
        // 输出仍是合法 UTF-8 且带截断标记。
        let text = String::from_utf8(out.clone()).expect("应按字符边界截断");
        assert!(text.contains(MARKER_TRUNCATED), "{text}");
        assert!(text.starts_with("exit=0\n"), "{text}");
        assert!(
            out.len() > MAX_PAYLOAD - 64,
            "应尽量用满预算：{}",
            out.len()
        );
    }

    #[test]
    fn assemble_bounded_marks_prior_truncation() {
        // body 本身已丢过字节（尾部滑窗）时必须带标记，即使没再截。
        let body = vec![b'x'; 16];
        let out = assemble_bounded("exit=0\n", &body, true, None, MAX_PAYLOAD);
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains(MARKER_TRUNCATED), "{text}");
        assert!(
            text.ends_with(&format!("exit=0\nxxxxxxxxxxxxxxxx\n{MARKER_TRUNCATED}")),
            "{text}"
        );
    }

    // ---- echo / sleep 回归 ----

    #[test]
    fn echo_and_sleep_tasks_advance() {
        let (tasks, _) = host_registries(ChipFamily::Esp32).unwrap();
        let workers = sekai::sekai_device_list::DeviceList::new();

        let factory = tasks.get("echo").unwrap();
        let mut handler = factory(b"payload").unwrap();
        let mut context = TaskContext {
            now_mono_ms: 0,
            deadline_mono_ms: 500,
            now_unix_ms: None,
            workers: &workers,
        };
        match handler.poll(&mut context) {
            TaskPoll::Done(result) => assert_eq!(result, b"payload"),
            other => panic!("echo 应立即完成：{other:?}"),
        }

        let factory = tasks.get("sleep").unwrap();
        let mut handler = factory(b"100").unwrap();
        let mut context = TaskContext {
            now_mono_ms: 0,
            deadline_mono_ms: 500,
            now_unix_ms: None,
            workers: &workers,
        };
        assert!(matches!(
            handler.poll(&mut context),
            TaskPoll::Pending { .. }
        ));
        context.now_mono_ms = 100;
        match handler.poll(&mut context) {
            TaskPoll::Done(result) => assert!(result.is_empty()),
            other => panic!("sleep 到期应完成：{other:?}"),
        }
    }

    #[test]
    fn host_registries_register_gpio_and_tasks() {
        let (tasks, mut tools) = host_registries(ChipFamily::Esp32).unwrap();
        assert!(tasks.contains("echo"));
        assert!(tasks.contains("sleep"));
        for name in sekai_gpio::GPIO_TOOL_NAMES {
            assert!(tools.contains(name), "缺少工具 {name}");
        }
        // virtual gpio 真实可用：配置 IO4 为输入后读到低电平。
        call_tool(&mut tools, "gpio.config", b"IO4 in").unwrap();
        let result = call_tool(&mut tools, "gpio.read", b"IO4").unwrap();
        let text = String::from_utf8(result).unwrap();
        assert!(text.contains("pin=IO4"), "{text}");
        assert!(text.contains("level=0"), "{text}");
    }

    // ---- pc.job ----

    /// 用真实时间驱动任务 poll 直到终结（或超时失败）。
    fn drive_task(handler: &mut dyn TaskHandler, deadline_ms: u64) -> TaskPoll {
        let workers = sekai::sekai_device_list::DeviceList::new();
        let started = Instant::now();
        loop {
            let now = started.elapsed().as_millis() as u64;
            let mut context = TaskContext {
                now_mono_ms: now,
                deadline_mono_ms: now + deadline_ms,
                now_unix_ms: None,
                workers: &workers,
            };
            match handler.poll(&mut context) {
                TaskPoll::Pending { .. } => thread::sleep(Duration::from_millis(10)),
                final_poll => return final_poll,
            }
        }
    }

    #[test]
    fn pc_job_short_command_completes_with_exit_zero() {
        let (tasks, _) = agent_registries(None).unwrap();
        let factory = tasks.get("pc.job").unwrap();
        let mut handler = factory(b"echo hello-job").unwrap();
        match drive_task(handler.as_mut(), 15_000) {
            TaskPoll::Done(payload) => {
                let text = String::from_utf8(payload).unwrap();
                assert!(text.starts_with("exit=0\n"), "{text}");
                assert!(text.contains("hello-job"), "{text}");
            }
            other => panic!("pc.job 应正常完成：{other:?}"),
        }
    }

    #[test]
    fn pc_job_empty_command_is_rejected() {
        let (tasks, _) = agent_registries(None).unwrap();
        let factory = tasks.get("pc.job").unwrap();
        let error = expect_factory_err(factory(b"   "));
        assert_eq!(error.code, ErrorCode::InvalidArgument);
        assert!(error.message.contains("pc.job"), "{}", error.message);
    }

    #[test]
    fn pc_job_long_command_is_killed_at_ddl() {
        let (tasks, _) = agent_registries(None).unwrap();
        let factory = tasks.get("pc.job").unwrap();
        // 长命令：DDL 到期必须杀进程并 Failed(Timeout)。
        let command = if cfg!(windows) {
            "ping -n 30 127.0.0.1"
        } else {
            "sleep 30"
        };
        let mut handler = factory(command.as_bytes()).unwrap();
        let workers = sekai::sekai_device_list::DeviceList::new();
        // 首个 poll：未到期 → Pending。
        let mut context = TaskContext {
            now_mono_ms: 0,
            deadline_mono_ms: 50,
            now_unix_ms: None,
            workers: &workers,
        };
        assert!(matches!(
            handler.poll(&mut context),
            TaskPoll::Pending { .. }
        ));
        // DDL 已过 → 杀进程 + Failed(Timeout)。
        context.now_mono_ms = 100;
        match handler.poll(&mut context) {
            TaskPoll::Failed(error) => assert_eq!(error.code, ErrorCode::Timeout),
            other => panic!("DDL 到期应 Failed(Timeout)：{other:?}"),
        }
        drop(handler); // Drop 兜底杀进程。
    }

    #[test]
    fn pc_job_keeps_tail_of_big_output() {
        let (tasks, _) = agent_registries(None).unwrap();
        let dir = TempDir::new("jobtail");
        let file = dir.join("big.bin");
        let mut body = vec![b'a'; 6_000];
        body.extend_from_slice(b"--tail-marker--");
        std::fs::write(&file, &body).unwrap();
        let command = if cfg!(windows) {
            format!("type {}", file.to_string_lossy())
        } else {
            format!("cat {}", file.to_string_lossy())
        };
        let factory = tasks.get("pc.job").unwrap();
        let mut handler = factory(command.as_bytes()).unwrap();
        match drive_task(handler.as_mut(), 15_000) {
            TaskPoll::Done(payload) => {
                assert!(payload.len() <= MAX_PAYLOAD, "{}", payload.len());
                let text = String::from_utf8(payload).unwrap();
                assert!(text.starts_with("exit=0\n"), "{text}");
                // 尾部保留：结尾可见；头部 a 已被滑窗丢弃（6000 → ~4000）。
                assert!(text.contains("tail-marker"), "{text}");
                assert!(text.contains(MARKER_TRUNCATED), "{text}");
                assert!(
                    text.matches('a').count() < 6_000,
                    "头部应被丢弃：{}",
                    text.matches('a').count()
                );
            }
            other => panic!("pc.job 应正常完成：{other:?}"),
        }
    }

    // ---- pc.ask ----

    #[test]
    fn pc_ask_without_channel_fails_actionably() {
        let (tasks, _) = agent_registries(None).unwrap();
        let factory = tasks.get("pc.ask").unwrap();
        let error = expect_factory_err(factory(" 你好 ".as_bytes()));
        assert_eq!(error.code, ErrorCode::InvalidArgument);
        assert!(error.message.contains("--llm"), "{}", error.message);

        let error = expect_factory_err(factory(b"   "));
        assert!(error.message.contains("为空"), "{}", error.message);
    }

    #[test]
    fn pc_ask_returns_answer_from_transport() {
        let llm: Arc<dyn PromptTransport> = Arc::new(FakeTransport {
            answer: Ok(String::from("这是回答")),
        });
        let (tasks, _) = agent_registries(Some(llm)).unwrap();
        let factory = tasks.get("pc.ask").unwrap();
        let mut handler = factory(" 问题 ".as_bytes()).unwrap();
        match drive_task(handler.as_mut(), 5_000) {
            TaskPoll::Done(payload) => assert_eq!(payload, "这是回答".as_bytes()),
            other => panic!("pc.ask 应完成：{other:?}"),
        }
    }

    #[test]
    fn pc_ask_maps_transport_errors() {
        // 非 timeout 错误 → InvalidArgument；timeout → Timeout。
        for (answer, expected_code) in [
            (
                Err(PromptError::new("unreachable", "连不上")),
                ErrorCode::InvalidArgument,
            ),
            (
                Err(PromptError::new("timeout", "上游超时")),
                ErrorCode::Timeout,
            ),
        ] {
            let llm: Arc<dyn PromptTransport> = Arc::new(FakeTransport { answer });
            let (tasks, _) = agent_registries(Some(llm)).unwrap();
            let factory = tasks.get("pc.ask").unwrap();
            let mut handler = factory("问".as_bytes()).unwrap();
            match drive_task(handler.as_mut(), 5_000) {
                TaskPoll::Failed(error) => {
                    assert_eq!(error.code, expected_code, "{}", error.message);
                    assert!(error.message.contains("pc.ask"), "{}", error.message);
                }
                other => panic!("pc.ask 应失败：{other:?}"),
            }
        }
    }

    #[test]
    fn pc_ask_answer_is_bounded() {
        let llm: Arc<dyn PromptTransport> = Arc::new(FakeTransport {
            answer: Ok("答".repeat(9_999)),
        });
        let (tasks, _) = agent_registries(Some(llm)).unwrap();
        let factory = tasks.get("pc.ask").unwrap();
        let mut handler = factory("问".as_bytes()).unwrap();
        match drive_task(handler.as_mut(), 5_000) {
            TaskPoll::Done(payload) => {
                assert!(payload.len() <= MAX_PAYLOAD, "{}", payload.len());
                let text = String::from_utf8(payload).unwrap();
                assert!(text.contains(MARKER_TRUNCATED), "{text}");
            }
            other => panic!("pc.ask 应完成：{other:?}"),
        }
    }
}
