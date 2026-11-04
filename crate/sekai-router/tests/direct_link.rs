//! Router 直连端点集成测试：真实 WorkerHost（sekai-worker）+ 脚本化对端。
//!
//! 覆盖：
//!
//! - 内存链路端到端：Hello 注册 → 派 echo Task → Completed（真实双方状态机）；
//! - 内存链路 Tool 调用（virtual GPIO）；
//! - 单帧 Failed（Worker 预校验失败，无 Accepted）映射为 worker_failed；
//! - 对端关闭链路 → 在途命令按 link_lost 失败；
//! - Worker 重启（新 boot_id Hello）→ 在途命令按 worker_rebooted 失败；
//! - TCP listen 模式：真实 WorkerHost 经 `sekai_worker::TcpLink` 接入；
//! - stdio 子进程模式（默认忽略，设 `SEKAI_WORKER_BIN` 后运行）。

use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use sekai::sekai_device_list::{DeviceList, Endpoint};
use sekai::sekai_runtime::RuntimeConfig;
use sekai::sekai_stdin::Stdin;
use sekai::sekai_stdout::{ErrorPayload, Hello, Phase, Stdout};
use sekai::sekai_wire::ErrorCode;
use sekai_gpio::ChipFamily;
use sekai_router::control::{CommandRequest, CommandResult, ControlEvent, ControlPlaneConfig};
use sekai_router::direct::{DirectListener, DirectWorkerEndpoint, StdioWorkerLink};
use sekai_router::topic::TopicRoot;
use sekai_router::worker::WorkerInfo;
use sekai_worker::capability::{SystemClock, host_registries};
use sekai_worker::host::WorkerHost;
use sekai_worker::link::{Link, LinkError, MemoryLink, TcpLink};

const FAMILY: ChipFamily = ChipFamily::Esp32;
/// 单测试整体超时。
const DRIVE_TIMEOUT: Duration = Duration::from_secs(20);

fn unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

fn endpoint_config(tag: &str) -> ControlPlaneConfig {
    ControlPlaneConfig::new(
        format!("router-direct-{tag}"),
        TopicRoot::new("sekai/v1").expect("测试主题根合法"),
    )
}

fn direct_endpoint() -> Endpoint {
    Endpoint::new("direct", 1).expect("端点合法")
}

// ---- 帧构造 helper ----

fn hello_frame(boot_id: u64, seq: u32, worker_id: &str, firmware: &str) -> Vec<u8> {
    let hello = Hello {
        worker_id: String::from(worker_id),
        firmware: String::from(firmware),
        uptime_ms: 0,
        unix_ms: 0,
        tools: vec![String::from("gpio.info"), String::from("gpio.read")],
        tasks: vec![String::from("echo"), String::from("sleep")],
        workers: DeviceList::new(),
        active_tasks: Vec::new(),
    };
    Stdout::new(boot_id, 0, seq, Phase::Hello, 0, hello.encode().unwrap())
        .unwrap()
        .encode()
        .unwrap()
}

fn failed_frame(boot_id: u64, cmd_id: u64, seq: u32, code: ErrorCode, message: &str) -> Vec<u8> {
    let payload = ErrorPayload::new(code, message).encode().unwrap();
    Stdout::new(boot_id, cmd_id, seq, Phase::Failed, 0, payload)
        .unwrap()
        .encode()
        .unwrap()
}

// ---- 真实 WorkerHost 驱动（对端线程） ----

/// 在独立线程里用真实 `WorkerHost` 驱动链路对端，直到链路关闭。
fn spawn_real_worker(
    mut link: impl Link + Send + 'static,
    worker_id: &str,
    boot_id: u64,
) -> JoinHandle<()> {
    let worker_id = String::from(worker_id);
    thread::spawn(move || {
        let (tasks, tools) = host_registries(FAMILY).expect("宿主能力注册失败");
        let mut config = RuntimeConfig::new(worker_id, boot_id);
        config.firmware = String::from("direct-test");
        let mut host = WorkerHost::new(config, SystemClock::new(), DeviceList::new(), tasks, tools)
            .expect("WorkerHost 构造失败");
        for frame in host.on_link_up(direct_endpoint()).expect("link_up 失败") {
            link.write_frame(&frame).expect("写出 Hello 失败");
        }
        loop {
            match link.read_frame() {
                Ok(Some(frame)) => {
                    for out in host.on_frame(&frame).expect("处理帧失败") {
                        link.write_frame(&out).expect("写出帧失败");
                    }
                }
                Ok(None) => {
                    host.on_link_lost();
                    return;
                }
                Err(LinkError::Timeout) => {
                    for out in host.on_poll().expect("poll 失败") {
                        link.write_frame(&out).expect("写出帧失败");
                    }
                }
                Err(_) => {
                    host.on_link_lost();
                    return;
                }
            }
        }
    })
}

// ---- 脚本化对端 ----

enum FakeReply {
    /// 回这些帧后继续。
    Frames(Vec<Vec<u8>>),
    /// 关闭链路。
    Close,
}

/// 脚本化 Worker：先发初始帧（Hello 等），之后对每条 Stdin 调 `on_stdin`
/// 决定回帧或关闭。`on_stdin` 返回的帧原样写链路（手工构造 Stdout）。
fn spawn_fake_worker<F>(
    mut link: MemoryLink,
    initial: Vec<Vec<u8>>,
    mut on_stdin: F,
) -> JoinHandle<()>
where
    F: FnMut(u64) -> FakeReply + Send + 'static,
{
    thread::spawn(move || {
        for frame in initial {
            link.write_frame(&frame).expect("脚本 Worker 写初始帧失败");
        }
        loop {
            match link.read_frame() {
                Ok(Some(frame)) => {
                    let cmd_id = Stdin::decode(&frame)
                        .map(|stdin| stdin.id)
                        .unwrap_or_default();
                    match on_stdin(cmd_id) {
                        FakeReply::Frames(frames) => {
                            for out in frames {
                                link.write_frame(&out).expect("脚本 Worker 写帧失败");
                            }
                        }
                        FakeReply::Close => {
                            link.close();
                            return;
                        }
                    }
                }
                Ok(None) => return,
                Err(LinkError::Timeout) => continue,
                Err(_) => return,
            }
        }
    })
}

// ---- Router 侧驱动 ----

fn pump_and_collect(endpoint: &mut DirectWorkerEndpoint<impl Link>) -> Vec<ControlEvent> {
    endpoint.pump(unix_ms(), unix_ms()).expect("端点泵失败");
    endpoint.take_events()
}

fn wait_online(endpoint: &mut DirectWorkerEndpoint<impl Link>, worker_id: &str) -> WorkerInfo {
    let deadline = Instant::now() + DRIVE_TIMEOUT;
    loop {
        for event in pump_and_collect(endpoint) {
            let _ = event;
        }
        if let Some(info) = endpoint.worker(worker_id)
            && info.online
        {
            return info;
        }
        assert!(Instant::now() < deadline, "等待 {worker_id} 上线超时");
        thread::sleep(Duration::from_millis(5));
    }
}

fn drive_until_finished(
    endpoint: &mut DirectWorkerEndpoint<impl Link>,
    cmd_id: u64,
) -> CommandResult {
    let deadline = Instant::now() + DRIVE_TIMEOUT;
    loop {
        for event in pump_and_collect(endpoint) {
            if let ControlEvent::CommandFinished(result) = event
                && result.cmd_id == cmd_id
            {
                return result;
            }
        }
        assert!(Instant::now() < deadline, "等待 cmd {cmd_id} 结束超时");
        thread::sleep(Duration::from_millis(5));
    }
}

// ---- 内存链路端到端（真实 WorkerHost） ----

#[test]
fn hello_then_task_echo_completes_over_memory_link() {
    let (router_link, worker_link) = sekai_worker::link::memory_pair();
    let worker = spawn_real_worker(worker_link, "pc-echo", 11);
    let mut endpoint = DirectWorkerEndpoint::new(
        endpoint_config("echo"),
        router_link,
        "pc-echo",
        direct_endpoint(),
        unix_ms(),
    )
    .expect("端点构造失败");
    assert!(endpoint.is_ready());

    let info = wait_online(&mut endpoint, "pc-echo");
    assert_eq!(info.boot_id, 11);
    assert_eq!(info.firmware, "direct-test");
    assert!(info.has_task("echo"), "echo 任务应通告：{:?}", info.tasks);
    assert!(info.has_tool("gpio.info"), "gpio.info 工具应通告");

    let cmd_id = endpoint
        .submit(
            CommandRequest::task("pc-echo", "echo", b"direct-hello".to_vec()),
            unix_ms(),
            unix_ms(),
        )
        .expect("提交失败");
    let result = drive_until_finished(&mut endpoint, cmd_id);
    assert!(result.ok(), "echo 应成功：{:?}", result.failure);
    assert_eq!(result.payload, b"direct-hello".to_vec());
    assert_eq!(
        result.phase_names(),
        vec!["Accepted", "Started", "Completed"]
    );
    assert!(endpoint.is_open());
    endpoint.close(unix_ms());
    worker.join().expect("Worker 线程 panic");
}

#[test]
fn tool_gpio_info_runs_over_memory_link() {
    let (router_link, worker_link) = sekai_worker::link::memory_pair();
    let worker = spawn_real_worker(worker_link, "pc-gpio", 12);
    let mut endpoint = DirectWorkerEndpoint::new(
        endpoint_config("gpio"),
        router_link,
        "pc-gpio",
        direct_endpoint(),
        unix_ms(),
    )
    .expect("端点构造失败");
    wait_online(&mut endpoint, "pc-gpio");

    let cmd_id = endpoint
        .submit(
            CommandRequest::tool("pc-gpio", "gpio.info", "IO4"),
            unix_ms(),
            unix_ms(),
        )
        .expect("提交失败");
    let result = drive_until_finished(&mut endpoint, cmd_id);
    assert!(result.ok(), "gpio.info 应成功：{:?}", result.failure);
    assert!(!result.payload.is_empty(), "gpio.info 应返回信息载荷");
    assert!(endpoint.frames_tx() >= 1, "至少写出一帧 Stdin");
    assert!(endpoint.frames_rx() >= 2, "至少收到 Hello 与事件帧");
    endpoint.close(unix_ms());
    worker.join().expect("Worker 线程 panic");
}

// ---- 脚本化对端：Router 侧生命周期语义 ----

/// Worker 预校验失败只回单帧 Failed（无 Accepted）：Router 侧 phases 不含
/// Accepted，failure 映射为 worker_failed + Worker 错误码。
#[test]
fn single_frame_failed_maps_to_worker_failed_without_accepted() {
    let (router_link, worker_link) = sekai_worker::link::memory_pair();
    let worker = spawn_fake_worker(
        worker_link,
        vec![hello_frame(1, 1, "pc-fail", "t")],
        |cmd_id| {
            FakeReply::Frames(vec![failed_frame(
                1,
                cmd_id,
                2,
                ErrorCode::InvalidArgument,
                "工具未注册",
            )])
        },
    );
    let mut endpoint = DirectWorkerEndpoint::new(
        endpoint_config("fail"),
        router_link,
        "pc-fail",
        direct_endpoint(),
        unix_ms(),
    )
    .expect("端点构造失败");
    wait_online(&mut endpoint, "pc-fail");

    let cmd_id = endpoint
        .submit(
            CommandRequest::task("pc-fail", "sleep", b"1".to_vec()),
            unix_ms(),
            unix_ms(),
        )
        .expect("提交失败");
    let result = drive_until_finished(&mut endpoint, cmd_id);
    assert!(!result.ok());
    assert_eq!(
        result.phase_names(),
        vec!["Failed"],
        "单帧 Failed 不应产生 Accepted 阶段"
    );
    let failure = result.failure.expect("应有失败信息");
    assert_eq!(failure.code, "worker_failed");
    assert_eq!(failure.worker_code, Some(ErrorCode::InvalidArgument));
    endpoint.close(unix_ms());
    worker.join().expect("脚本 Worker 线程 panic");
}

/// 对端在命令在途时关闭链路：命令按 link_lost 失败。
#[test]
fn worker_closing_link_fails_pending_as_link_lost() {
    let (router_link, worker_link) = sekai_worker::link::memory_pair();
    let worker = spawn_fake_worker(
        worker_link,
        vec![hello_frame(5, 1, "pc-vanish", "t")],
        |_| FakeReply::Close,
    );
    let mut endpoint = DirectWorkerEndpoint::new(
        endpoint_config("vanish"),
        router_link,
        "pc-vanish",
        direct_endpoint(),
        unix_ms(),
    )
    .expect("端点构造失败");
    wait_online(&mut endpoint, "pc-vanish");

    let cmd_id = endpoint
        .submit(
            CommandRequest::task("pc-vanish", "sleep", b"60000".to_vec()),
            unix_ms(),
            unix_ms(),
        )
        .expect("提交失败");
    let result = drive_until_finished(&mut endpoint, cmd_id);
    let failure = result.failure.expect("应有失败信息");
    assert_eq!(failure.code, "link_lost");
    assert!(!endpoint.is_open(), "链路应已关闭");
    let info = endpoint.worker("pc-vanish").expect("快照保留");
    assert!(!info.online, "断链后 Worker 应离线");
    worker.join().expect("脚本 Worker 线程 panic");
}

/// 命令在途时 Worker 重启（新 boot_id Hello）：命令按 worker_rebooted 失败，
/// 注册表更新为新 boot_id。
#[test]
fn worker_reboot_fails_pending_and_updates_boot_id() {
    let (router_link, worker_link) = sekai_worker::link::memory_pair();
    let worker = spawn_fake_worker(
        worker_link,
        vec![hello_frame(1, 1, "pc-reboot", "t")],
        |cmd_id| {
            FakeReply::Frames(vec![
                hello_frame(2, 1, "pc-reboot", "t"),
                failed_frame(2, cmd_id, 2, ErrorCode::UnknownTask, "不应到达"),
            ])
        },
    );
    let mut endpoint = DirectWorkerEndpoint::new(
        endpoint_config("reboot"),
        router_link,
        "pc-reboot",
        direct_endpoint(),
        unix_ms(),
    )
    .expect("端点构造失败");
    wait_online(&mut endpoint, "pc-reboot");

    let cmd_id = endpoint
        .submit(
            CommandRequest::task("pc-reboot", "sleep", b"60000".to_vec()),
            unix_ms(),
            unix_ms(),
        )
        .expect("提交失败");
    let result = drive_until_finished(&mut endpoint, cmd_id);
    let failure = result.failure.expect("应有失败信息");
    assert_eq!(
        failure.code, "worker_rebooted",
        "boot_id 变化应失败在途命令：{failure:?}"
    );
    let info = endpoint.worker("pc-reboot").expect("快照保留");
    assert_eq!(info.boot_id, 2, "注册表应更新为新 boot_id");
    assert!(info.online);
    endpoint.close(unix_ms());
    worker.join().expect("脚本 Worker 线程 panic");
}

// ---- TCP listen 模式 ----

#[test]
fn tcp_listener_accepts_real_worker_host() {
    let listener = DirectListener::bind("127.0.0.1:0").expect("监听绑定失败");
    let address = listener.local_addr().expect("监听地址失败");

    let worker = thread::spawn(move || {
        let mut link = TcpLink::new();
        link.connect(&address.to_string()).expect("Worker 连接失败");
        let mut link = link;
        let (tasks, tools) = host_registries(FAMILY).expect("宿主能力注册失败");
        let mut config = RuntimeConfig::new(String::from("pc-tcp"), 33);
        config.firmware = String::from("tcp-test");
        let mut host = WorkerHost::new(config, SystemClock::new(), DeviceList::new(), tasks, tools)
            .expect("WorkerHost 构造失败");
        for frame in host.on_link_up(direct_endpoint()).expect("link_up 失败") {
            link.write_frame(&frame).expect("写出 Hello 失败");
        }
        loop {
            match link.read_frame() {
                Ok(Some(frame)) => {
                    for out in host.on_frame(&frame).expect("处理帧失败") {
                        link.write_frame(&out).expect("写出帧失败");
                    }
                }
                Ok(None) => {
                    host.on_link_lost();
                    return;
                }
                Err(LinkError::Timeout) => {
                    for out in host.on_poll().expect("poll 失败") {
                        link.write_frame(&out).expect("写出帧失败");
                    }
                }
                Err(_) => {
                    host.on_link_lost();
                    return;
                }
            }
        }
    });

    let mut endpoint = listener
        .accept_endpoint(
            endpoint_config("tcp"),
            "pc-tcp",
            direct_endpoint(),
            unix_ms(),
        )
        .expect("接受连接失败");
    let info = wait_online(&mut endpoint, "pc-tcp");
    assert_eq!(info.boot_id, 33);
    assert_eq!(info.firmware, "tcp-test");

    let cmd_id = endpoint
        .submit(
            CommandRequest::task("pc-tcp", "echo", b"over-tcp".to_vec()),
            unix_ms(),
            unix_ms(),
        )
        .expect("提交失败");
    let result = drive_until_finished(&mut endpoint, cmd_id);
    assert!(result.ok(), "TCP echo 应成功：{:?}", result.failure);
    assert_eq!(result.payload, b"over-tcp".to_vec());
    endpoint.close(unix_ms());
    worker.join().expect("Worker 线程 panic");
}

// ---- stdio 子进程模式（默认忽略） ----

/// 真实子进程冒烟：需要先 `cargo build -p sekai-worker`，然后设置
/// `SEKAI_WORKER_BIN=<target/debug/sekai-worker(.exe)>` 并
/// `cargo test -p sekai-router --test direct_link -- --ignored` 运行。
#[test]
#[ignore = "需要 SEKAI_WORKER_BIN 指向已构建的 sekai-worker 二进制"]
fn stdio_subprocess_hello_and_echo() {
    let binary = std::env::var("SEKAI_WORKER_BIN")
        .ok()
        .filter(|value| !value.is_empty())
        .expect("设置 SEKAI_WORKER_BIN 后运行");
    let link = StdioWorkerLink::spawn(
        &binary,
        &[
            "--mode",
            "stdio",
            "--worker-id",
            "pc-spawn",
            "--boot-id",
            "42",
        ],
    )
    .expect("拉起子进程失败");

    let mut endpoint = DirectWorkerEndpoint::new(
        endpoint_config("stdio"),
        link,
        "pc-spawn",
        direct_endpoint(),
        unix_ms(),
    )
    .expect("端点构造失败");
    let info = wait_online(&mut endpoint, "pc-spawn");
    assert_eq!(info.boot_id, 42);
    assert!(info.has_task("echo"));

    let cmd_id = endpoint
        .submit(
            CommandRequest::task("pc-spawn", "echo", b"sub-process".to_vec()),
            unix_ms(),
            unix_ms(),
        )
        .expect("提交失败");
    let result = drive_until_finished(&mut endpoint, cmd_id);
    assert!(result.ok(), "子进程 echo 应成功：{:?}", result.failure);
    assert_eq!(result.payload, b"sub-process".to_vec());
    endpoint.close(unix_ms());
}
