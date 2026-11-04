//! DirectGateway 网关级测试：真实 WorkerHost（sekai-worker）经内存链路 /
//! TCP 连入 / stdio 子进程接入线程化网关，走 WorkerGateway trait 的完整
//! wait_ready → list_workers → call 回路。
//!
//! 覆盖：
//!
//! - 内存链路：echo Task 端到端 Completed；
//! - 链路关闭后 call 以稳定错误码 `link_lost` 拒绝；
//! - TCP listen：真实 WorkerHost 经 `sekai_worker::TcpLink` 连入 → 发现 →
//!   gpio.info Tool 调用成功；
//! - TCP 断链重连：Worker 二次连入后重新注册；
//! - stdio 子进程（默认忽略，设 `SEKAI_WORKER_BIN` 后运行）。

use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use sekai::sekai_device_list::{DeviceList, Endpoint};
use sekai::sekai_runtime::RuntimeConfig;
use sekai_gpio::ChipFamily;
use sekai_router::control::{CommandRequest, ControlPlaneConfig};
use sekai_router::direct_gateway::{DirectGateway, DirectGatewayConfig, DirectMode};
use sekai_router::gateway::WorkerGateway;
use sekai_router::topic::TopicRoot;
use sekai_worker::capability::{SystemClock, host_registries};
use sekai_worker::host::WorkerHost;
use sekai_worker::link::{Link, LinkError, MemoryLink, TcpLink};

const FAMILY: ChipFamily = ChipFamily::Esp32;
/// 单测试整体超时。
const DRIVE_TIMEOUT: Duration = Duration::from_secs(20);
/// 轮询步长。
const POLL: Duration = Duration::from_millis(20);

fn gateway_config(tag: &str) -> DirectGatewayConfig {
    let mut plane = ControlPlaneConfig::new(
        format!("router-dg-{tag}"),
        TopicRoot::new("sekai/v1").unwrap(),
    );
    plane.default_ddl_ms = 500;
    DirectGatewayConfig {
        plane,
        worker_id_hint: String::from("pc-worker"),
        default_ddl_ms: 500,
        tool_timeout_ms: Some(5_000),
        task_timeout_ms: Some(5_000),
    }
}

/// 在独立线程里用真实 `WorkerHost` 驱动一条链路对端，直到链路关闭。
fn spawn_real_worker(
    mut link: impl Link + Send + 'static,
    worker_id: &str,
    boot_id: u64,
) -> JoinHandle<()> {
    let worker_id = String::from(worker_id);
    thread::spawn(move || {
        let (tasks, tools) = host_registries(FAMILY).expect("宿主能力注册失败");
        let mut config = RuntimeConfig::new(worker_id, boot_id);
        config.firmware = String::from("gateway-test");
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

/// TCP Worker：连接 `addr` 并驱动 WorkerHost；每会话 `close_after` 后主动
/// 关闭链路（或等 Router 关闭），共 `sessions` 个会话。
fn spawn_tcp_worker(
    addr: String,
    worker_id: &str,
    boot_id: u64,
    sessions: usize,
    close_after: Duration,
) -> JoinHandle<()> {
    let worker_id = String::from(worker_id);
    thread::spawn(move || {
        for _ in 0..sessions {
            // 连接带重试（Router 可能尚未回到 Accepting）。
            let mut link = TcpLink::new();
            let connected = {
                let deadline = Instant::now() + Duration::from_secs(10);
                loop {
                    if link.connect(&addr).is_ok() {
                        break true;
                    }
                    if Instant::now() >= deadline {
                        break false;
                    }
                    thread::sleep(POLL);
                }
            };
            if !connected {
                return;
            }
            let (tasks, tools) = host_registries(FAMILY).expect("宿主能力注册失败");
            let mut config = RuntimeConfig::new(worker_id.clone(), boot_id);
            config.firmware = String::from("gateway-test");
            let mut host =
                WorkerHost::new(config, SystemClock::new(), DeviceList::new(), tasks, tools)
                    .expect("WorkerHost 构造失败");
            let endpoint = Endpoint::parse(&addr).expect("地址合法");
            for frame in host.on_link_up(endpoint).expect("link_up 失败") {
                link.write_frame(&frame).expect("写出 Hello 失败");
            }
            let started = Instant::now();
            loop {
                match link.read_frame() {
                    Ok(Some(frame)) => {
                        for out in host.on_frame(&frame).expect("处理帧失败") {
                            link.write_frame(&out).expect("写出帧失败");
                        }
                    }
                    Ok(None) => {
                        host.on_link_lost();
                        break;
                    }
                    Err(LinkError::Timeout) => {
                        for out in host.on_poll().expect("poll 失败") {
                            link.write_frame(&out).expect("写出帧失败");
                        }
                    }
                    Err(_) => {
                        host.on_link_lost();
                        break;
                    }
                }
                if started.elapsed() >= close_after {
                    // Worker 主动关闭本会话。
                    link.close();
                    host.on_link_lost();
                    break;
                }
            }
            // 会话间隔，给 Router 侧一点时间回到 Accepting。
            thread::sleep(Duration::from_millis(100));
        }
    })
}

fn direct_endpoint() -> Endpoint {
    Endpoint::new("direct", 1).expect("端点合法")
}

/// 轮询直到网关发现指定 Worker（或超时 panic）。
fn wait_for_worker(gateway: &mut DirectGateway, worker_id: &str) {
    let deadline = Instant::now() + DRIVE_TIMEOUT;
    loop {
        let found = gateway
            .list_workers()
            .iter()
            .any(|worker| worker.worker_id == worker_id && worker.online);
        if found {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "等待 Worker {worker_id} 上线超时"
        );
        thread::sleep(POLL);
    }
}

// ---- 内存链路 ----

/// 内存链路端到端：wait_ready → Hello 注册 → echo Task → Completed。
#[test]
fn memory_link_gateway_runs_echo_task() {
    let (router_link, worker_link): (MemoryLink, MemoryLink) = sekai_worker::link::memory_pair();
    let worker = spawn_real_worker(worker_link, "pc-mem", 21);
    let mut gateway =
        DirectGateway::over_link(Box::new(router_link), gateway_config("mem")).unwrap();
    gateway.wait_ready(2_000).expect("应立即可就绪");
    wait_for_worker(&mut gateway, "pc-mem");

    let result = gateway
        .call(CommandRequest::task("pc-mem", "echo", b"gw-hello".to_vec()))
        .expect("echo 应成功");
    assert!(result.ok(), "echo 应成功：{:?}", result.failure);
    assert_eq!(result.payload, b"gw-hello".to_vec());
    assert_eq!(
        result.phase_names(),
        vec!["Accepted", "Started", "Completed"]
    );
    drop(gateway);
    worker.join().expect("Worker 线程 panic");
}

/// 对端关闭后：网关存活但 call 以 link_lost 拒绝。
///
/// 对端线程发完 Hello 后立即关闭链路。瞬逝对端的「在线」窗口只存在于
/// 单次泵内、不会发布到快照，因此不能等快照翻转；改为给 IO 线程
/// 足够周期（300ms ≫ 20ms 泵节奏）观察关闭，之后断言 Closed 拒绝语义。
#[test]
fn call_after_link_close_is_rejected_with_link_lost() {
    let (router_link, worker_link): (MemoryLink, MemoryLink) = sekai_worker::link::memory_pair();
    // Hello 后立即关闭的对端。
    thread::spawn(move || {
        let mut worker_link = worker_link;
        let (tasks, tools) = host_registries(FAMILY).expect("宿主能力注册失败");
        let mut config = RuntimeConfig::new("pc-gone", 22);
        config.firmware = String::from("gateway-test");
        let mut host = WorkerHost::new(config, SystemClock::new(), DeviceList::new(), tasks, tools)
            .expect("WorkerHost 构造失败");
        for frame in host.on_link_up(direct_endpoint()).expect("link_up 失败") {
            worker_link.write_frame(&frame).expect("写出 Hello 失败");
        }
        worker_link.close();
    });
    let mut gateway =
        DirectGateway::over_link(Box::new(router_link), gateway_config("gone")).unwrap();

    // IO 线程必然已观察到关闭并进入 Closed 终态。
    thread::sleep(Duration::from_millis(300));
    assert!(!gateway.is_ready());

    let error = gateway
        .call(CommandRequest::task("pc-gone", "echo", b"x".to_vec()))
        .expect_err("链路关闭后 call 应被拒绝");
    assert_eq!(error.code, "link_lost", "错误应为 link_lost：{error}");
    drop(gateway);
}

// ---- TCP listen ----

/// TCP listen：Worker 主动连入 → 发现 → gpio.info Tool 成功。
#[test]
fn tcp_listen_accepts_real_worker() {
    let mut gateway = DirectGateway::listen_tcp("127.0.0.1:0", gateway_config("tcp")).unwrap();
    let addr = match gateway.mode() {
        DirectMode::TcpListen { addr } => addr.clone(),
        other => panic!("模式应为 TcpListen：{other:?}"),
    };
    // 会话寿命长于测试，Router drop 时自然断开。
    let worker = spawn_tcp_worker(addr, "pc-tcp", 31, 1, Duration::from_secs(60));
    gateway.wait_ready(5_000).expect("应接受连接");
    wait_for_worker(&mut gateway, "pc-tcp");

    let result = gateway
        .call(CommandRequest::tool("pc-tcp", "gpio.info", "IO4"))
        .expect("gpio.info 应成功");
    assert!(result.ok(), "gpio.info 应成功：{:?}", result.failure);
    assert!(!result.payload.is_empty());
    drop(gateway);
    worker.join().expect("Worker 线程 panic");
}

/// TCP 断链重连：Worker 主动关闭会话一后再次连入，重新注册。
#[test]
fn tcp_reconnect_reregisters_worker() {
    let mut gateway = DirectGateway::listen_tcp("127.0.0.1:0", gateway_config("recon")).unwrap();
    let addr = match gateway.mode() {
        DirectMode::TcpListen { addr } => addr.clone(),
        other => panic!("模式应为 TcpListen：{other:?}"),
    };
    // 每会话 300ms 后 Worker 主动关闭，共两幕。
    let worker = spawn_tcp_worker(addr, "pc-recon", 32, 2, Duration::from_millis(300));
    gateway.wait_ready(5_000).expect("应接受首条连接");
    wait_for_worker(&mut gateway, "pc-recon");

    // 第一会话结束：Router 回到 Accepting，注册表清空。
    let deadline = Instant::now() + DRIVE_TIMEOUT;
    loop {
        let empty = gateway.list_workers().is_empty();
        if empty {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "断链后注册表应清空（workers={:?}）",
            gateway.list_workers()
        );
        thread::sleep(POLL);
    }

    // 第二会话连入：重新注册上线。
    let deadline = Instant::now() + DRIVE_TIMEOUT;
    loop {
        let online = gateway
            .list_workers()
            .iter()
            .any(|worker| worker.worker_id == "pc-recon" && worker.online);
        if online {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "等待重连注册超时（workers={:?}）",
            gateway.list_workers()
        );
        thread::sleep(POLL);
    }
    drop(gateway);
    worker.join().expect("Worker 线程 panic");
}

/// 对端连入后立即消失（无 Hello）：网关回到 Accepting，call 以稳定
/// 错误码 link_lost 拒绝，不挂起。无论 call 落在 Accepting（尚未接受）
/// 还是断链后，错误码一致。
#[test]
fn tcp_vanishing_peer_rejects_calls_with_stable_code() {
    let mut gateway = DirectGateway::listen_tcp("127.0.0.1:0", gateway_config("cut")).unwrap();
    let addr = match gateway.mode() {
        DirectMode::TcpListen { addr } => addr.clone(),
        other => panic!("模式应为 TcpListen：{other:?}"),
    };
    // 原始 TCP 对端：连上立即关闭，不经 WorkerHost。
    {
        let stream = std::net::TcpStream::connect(&addr).expect("应能连入");
        drop(stream);
    }
    // 给 IO 线程一点时间接受并观察到关闭。
    thread::sleep(Duration::from_millis(200));

    let error = gateway
        .call(CommandRequest::task("pc-cut", "echo", b"x".to_vec()))
        .expect_err("断链后 call 应被拒绝");
    assert_eq!(error.code, "link_lost", "错误码应稳定：{error}");
    drop(gateway);
}

// ---- stdio 子进程（需真实二进制） ----

/// `SEKAI_WORKER_BIN=<target/debug/sekai-worker(.exe)>` 后运行：
/// spawn_stdio 拉起真实 Worker 子进程并完成 echo Task。
#[test]
#[ignore = "需要 SEKAI_WORKER_BIN 指向已构建的 sekai-worker 二进制"]
fn stdio_spawn_runs_real_worker_binary() {
    let binary = std::env::var("SEKAI_WORKER_BIN").expect("设置 SEKAI_WORKER_BIN 后运行");
    let args = vec![
        String::from("--mode"),
        String::from("stdio"),
        String::from("--worker-id"),
        String::from("stdio-gw"),
        String::from("--boot-id"),
        String::from("77"),
    ];
    let mut gateway = DirectGateway::spawn_stdio(&binary, &args, gateway_config("stdio")).unwrap();
    gateway.wait_ready(5_000).expect("子进程应立即就绪");
    wait_for_worker(&mut gateway, "stdio-gw");
    let result = gateway
        .call(CommandRequest::task(
            "stdio-gw",
            "echo",
            b"from-spawn".to_vec(),
        ))
        .expect("echo 应成功");
    assert!(result.ok(), "echo 应成功：{:?}", result.failure);
    assert_eq!(result.payload, b"from-spawn".to_vec());
    drop(gateway);
}
