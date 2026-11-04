//! 内存链路集成测试：直连承载下的 V1 控制平面全流程。
//!
//! 链路两端：`Loop::router` 扮演 Router（直接读写 V1 帧），Worker 端由
//! `Loop::pump_once` 驱动真实 `Runtime`。覆盖：
//!
//! - Hello → Task Stdin → Accepted/Started/Completed 全流程；
//! - ToolCall 流程（PC agent pc.* 工具、gpio opt-in 与内置工具）；
//! - 非法帧拒绝（解码层静默丢弃 + 可关联 id 的 Failed 回执 + 链路层拒绝）；
//! - 断线重连后的 retained-Hello 语义与 boot_id/seq 连续性；
//! - Stdin 去重（重放最终事件）与 sleep 任务的多周期推进。

mod support;

use std::thread;
use std::time::{Duration, Instant};

use sekai::sekai_stdin::{Kind, Stdin, TaskBody, ToolBody};
use sekai::sekai_stdout::{ErrorPayload, Hello, Phase, Stdout};
use sekai::sekai_wire::ErrorCode;
use sekai_gpio::GPIO_TOOL_NAMES;
use sekai_worker::link::Link;

use support::{BASE_UNIX_MS, Loop};

// ---- 帧构造（Router 侧视角） ----

fn stdin_bytes(id: u64, kind: Kind, payload: Vec<u8>, ddl_ms: u32) -> Vec<u8> {
    Stdin::new(id, kind, BASE_UNIX_MS, 0, ddl_ms, payload)
        .expect("Stdin 构造失败")
        .encode()
        .expect("Stdin 编码失败")
}

fn task_stdin(id: u64, name: &str, args: &[u8]) -> Vec<u8> {
    task_stdin_with_ddl(id, name, args, 500)
}

fn task_stdin_with_ddl(id: u64, name: &str, args: &[u8], ddl_ms: u32) -> Vec<u8> {
    let body = TaskBody::new(name, args.to_vec()).expect("任务载荷构造失败");
    stdin_bytes(
        id,
        Kind::Task,
        body.encode().expect("任务载荷编码失败"),
        ddl_ms,
    )
}

fn tool_stdin(id: u64, name: &str, args: &[u8]) -> Vec<u8> {
    let body = ToolBody::new(name, args.to_vec()).expect("工具载荷构造失败");
    stdin_bytes(
        id,
        Kind::ToolCall,
        body.encode().expect("工具载荷编码失败"),
        0,
    )
}

fn decode_stdout(frame: &[u8]) -> Stdout {
    Stdout::decode(frame).expect("Stdout 解码失败")
}

fn phases(frames: &[Vec<u8>]) -> Vec<Phase> {
    frames.iter().map(|f| decode_stdout(f).phase).collect()
}

/// 等待指定 `cmd_id` 的下一帧：期间 pump Worker、推进手动时钟并真实等待
/// （`pc.exec` 等 Task 的完成依赖真实子进程退出，而 `ManualClock` 只能手动
/// 推进「下次 poll 时刻」）。
fn wait_for_cmd_frame(lp: &mut Loop, cmd_id: u64, max_ms: u64) -> Stdout {
    let deadline = Instant::now() + Duration::from_millis(max_ms);
    loop {
        lp.pump_until_idle(2);
        loop {
            match lp.router.read_frame() {
                Ok(Some(frame)) => {
                    let out = decode_stdout(&frame);
                    if out.cmd_id == cmd_id {
                        return out;
                    }
                }
                Ok(None) => panic!("Router 端链路被关闭"),
                Err(_) => break, // 当前没有可读帧
            }
        }
        assert!(
            Instant::now() < deadline,
            "等待命令 {cmd_id} 的帧超时（{max_ms}ms）"
        );
        thread::sleep(Duration::from_millis(10));
        lp.clock.advance(10);
    }
}

// ---- Hello → Task → Accepted/Started/Completed ----

#[test]
fn hello_then_task_full_flow() {
    let mut lp = Loop::new("pc-1", 7);

    // 链路建立后首帧必须是 Hello（对应 MQTT retained state 语义）。
    let hello_frame = lp.router_read();
    let hello_out = decode_stdout(&hello_frame);
    assert_eq!(hello_out.phase, Phase::Hello);
    assert_eq!(hello_out.boot_id, 7, "boot_id 必须与配置一致");
    assert_eq!(hello_out.cmd_id, 0, "Hello 的 cmd_id 必须为 0");
    assert_eq!(hello_out.seq, 1, "本 boot 内首个 Stdout 的 seq 为 1");
    let hello = Hello::decode(&hello_out.payload).expect("Hello 载荷解码失败");
    assert_eq!(hello.worker_id, "pc-1");
    assert_eq!(hello.firmware, "0.1.0-test");
    assert!(
        hello.tasks.iter().any(|name| name == "echo"),
        "{:?}",
        hello.tasks
    );
    assert!(hello.tasks.iter().any(|name| name == "sleep"));
    assert!(hello.tasks.iter().any(|name| name == "pc.job"));
    assert!(hello.tasks.iter().any(|name| name == "pc.ask"));
    // pc.exec 是 Task（B4.2/P3）：Hello 必须把它放在 tasks 列表里，
    // Router 会按"名字→线类型"自动路由同名调用。
    assert!(
        hello.tasks.iter().any(|name| name == "pc.exec"),
        "{:?}",
        hello.tasks
    );
    assert!(
        !hello.tools.iter().any(|tool| tool == "pc.exec"),
        "pc.exec 不应再出现在 tools 列表"
    );
    // 能力声明：PC agent 工具 + 内置工具；默认（无 --gpio）不含 gpio.*。
    for name in ["pc.read", "pc.write", "pc.ls", "pc.info"] {
        assert!(
            hello.tools.iter().any(|tool| tool == name),
            "Hello 缺少工具 {name}"
        );
    }
    assert!(hello.tools.iter().any(|tool| tool == "sekai.time"));
    for name in GPIO_TOOL_NAMES {
        assert!(
            !hello.tools.iter().any(|tool| tool == name),
            "默认不应注册 gpio 工具 {name}"
        );
    }
    lp.assert_router_idle();

    // Router 下发 Task Stdin（echo 任务）。
    lp.router
        .write_frame(&task_stdin(7, "echo", b"hello-sekai"))
        .expect("写链路失败");
    let frames = lp.router_read_n(3);
    assert_eq!(
        phases(&frames),
        [Phase::Accepted, Phase::Started, Phase::Completed],
        "echo 任务应产生三段命令事件"
    );

    let accepted = decode_stdout(&frames[0]);
    assert_eq!(accepted.cmd_id, 7);
    assert_eq!(accepted.boot_id, 7);
    let started = decode_stdout(&frames[1]);
    assert_eq!(started.cmd_id, 7);
    let completed = decode_stdout(&frames[2]);
    assert_eq!(completed.cmd_id, 7);
    assert_eq!(completed.payload, b"hello-sekai", "echo 应原样返回参数");
    assert_eq!((accepted.seq, started.seq, completed.seq), (2, 3, 4));
    lp.assert_router_idle();
}

// ---- ToolCall 流程（gpio opt-in：--gpio 后含 gpio.*；PC agent 工具见下） ----

#[test]
fn tool_call_flow_with_virtual_gpio() {
    // gpio 需显式 opt-in（等价 CLI --gpio esp32）。
    let mut lp = Loop::new_with_gpio("pc-tools", 11);
    let hello_frame = lp.router_read();
    let hello_out = decode_stdout(&hello_frame);
    let hello = Hello::decode(&hello_out.payload).expect("Hello 载荷解码失败");
    for name in GPIO_TOOL_NAMES {
        assert!(
            hello.tools.iter().any(|tool| tool == name),
            "--gpio 后应含工具 {name}"
        );
    }
    assert!(
        hello.tasks.iter().any(|name| name == "pc.exec"),
        "pc.exec 应在任务列表（Task 化后）"
    );

    // gpio.config：把 IO4 配置为输入。
    lp.router
        .write_frame(&tool_stdin(21, "gpio.config", b"IO4 in"))
        .expect("写链路失败");
    let frames = lp.router_read_n(2);
    assert_eq!(phases(&frames), [Phase::Accepted, Phase::Completed]);
    let completed = decode_stdout(&frames[1]);
    assert_eq!(completed.cmd_id, 21);
    let text = String::from_utf8(completed.payload.clone()).expect("config 结果应为 UTF-8");
    assert!(text.contains("pin=IO4"), "{text}");
    assert!(text.contains("mode=in"), "{text}");

    // gpio.read：virtual 后端默认读到低电平。
    lp.router
        .write_frame(&tool_stdin(22, "gpio.read", b"IO4"))
        .expect("写链路失败");
    let frames = lp.router_read_n(2);
    assert_eq!(phases(&frames), [Phase::Accepted, Phase::Completed]);
    let completed = decode_stdout(&frames[1]);
    assert_eq!(completed.cmd_id, 22);
    let text = String::from_utf8(completed.payload.clone()).expect("read 结果应为 UTF-8");
    assert!(text.contains("level=0"), "{text}");

    // 未注册工具名 → 预校验拒绝：只回一帧 Failed(UnknownTool)，不发 Accepted
    // （Accepted 仅在校验通过并入队后发出；已注册工具的运行期失败才是
    //   Accepted+Failed 两帧，见 runtime 的 remember_fail 路径）。
    lp.router
        .write_frame(&tool_stdin(23, "gpio.nope", b""))
        .expect("写链路失败");
    let frames = lp.router_read_n(1);
    let failed = decode_stdout(&frames[0]);
    assert_eq!(failed.phase, Phase::Failed);
    assert_eq!(failed.cmd_id, 23);
    let error = ErrorPayload::decode(&failed.payload).expect("错误载荷解码失败");
    assert_eq!(error.code, ErrorCode::UnknownTool);

    // 内置工具 sekai.time 也在直连链路上可用（16 字节结果）。
    lp.router
        .write_frame(&tool_stdin(24, "sekai.time", b""))
        .expect("写链路失败");
    let frames = lp.router_read_n(2);
    assert_eq!(phases(&frames), [Phase::Accepted, Phase::Completed]);
    let completed = decode_stdout(&frames[1]);
    assert_eq!(
        completed.payload.len(),
        16,
        "sekai.time = unix_ms + mono_ms"
    );

    // seq 在整条链路生命周期内严格递增：Hello(1) + config(2,3) + read(4,5)
    // + nope 的 Failed(6) + time(7,8)。
    let last = decode_stdout(&frames[1]);
    assert_eq!(last.seq, 8);
    lp.assert_router_idle();
}

// ---- PC agent 能力经直连链路（pc.exec 任务 / pc.write → pc.read 工具） ----

#[test]
fn pc_agent_tools_flow_over_link() {
    let mut lp = Loop::new("pc-agent", 21);
    let _hello = lp.router_read();

    // pc.exec 是 Task：Accepted → Started → Completed 三段。
    lp.router
        .write_frame(&task_stdin(31, "pc.exec", b"echo hello-pc"))
        .expect("写链路失败");
    let frames = lp.router_read_n(3);
    assert_eq!(
        phases(&frames),
        [Phase::Accepted, Phase::Started, Phase::Completed]
    );
    let completed = decode_stdout(&frames[2]);
    assert_eq!(completed.cmd_id, 31);
    let text = String::from_utf8(completed.payload.clone()).expect("exec 结果应为 UTF-8");
    assert!(text.starts_with("exit=0\n"), "{text}");
    assert!(text.contains("hello-pc"), "{text}");

    // pc.write → pc.read 往返（tempdir 测试自建自清）。
    let dir = std::env::temp_dir().join(format!("sekai-worker-flow-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("创建临时目录失败");
    let file = dir.join("note.txt");
    let path = file.to_string_lossy().into_owned();

    lp.router
        .write_frame(&tool_stdin(
            32,
            "pc.write",
            format!("{path}\nhello-file\n").as_bytes(),
        ))
        .expect("写链路失败");
    let frames = lp.router_read_n(2);
    assert_eq!(phases(&frames), [Phase::Accepted, Phase::Completed]);
    let completed = decode_stdout(&frames[1]);
    assert_eq!(completed.payload, b"written=11", "hello-file\\n 共 11 字节");

    lp.router
        .write_frame(&tool_stdin(33, "pc.read", format!("{path}\n").as_bytes()))
        .expect("写链路失败");
    let frames = lp.router_read_n(2);
    assert_eq!(phases(&frames), [Phase::Accepted, Phase::Completed]);
    let completed = decode_stdout(&frames[1]);
    assert_eq!(completed.payload, b"hello-file\n");
    let _ = std::fs::remove_dir_all(&dir);

    // seq 严格递增：Hello(1) + exec(2,3,4) + write(5,6) + read(7,8)。
    assert_eq!(completed.seq, 8);
    lp.assert_router_idle();
}

// ---- 非法帧拒绝 ----

#[test]
fn invalid_frames_are_rejected() {
    let mut lp = Loop::new("pc-bad", 13);
    let hello = decode_stdout(&lp.router_read());
    assert_eq!(hello.seq, 1);

    // (a) kind=9 的未知命令类型：头部 id 不可解析 → 静默丢弃并计数。
    let mut raw = vec![1u8, 9];
    raw.extend_from_slice(&42u64.to_be_bytes()); // id
    raw.extend_from_slice(&BASE_UNIX_MS.to_be_bytes()); // timestamp
    raw.extend_from_slice(&0u32.to_be_bytes()); // delay_ms
    raw.extend_from_slice(&500u32.to_be_bytes()); // ddl_ms
    raw.extend_from_slice(&1u32.to_be_bytes()); // payload_len
    raw.push(b'x');
    lp.router.write_frame(&raw).expect("写链路失败");
    lp.pump_until_idle(2);
    lp.assert_router_idle();
    assert_eq!(lp.host.status().rejected_commands, 1);

    // (b) 版本字节错误：同样静默丢弃。
    let mut raw = task_stdin(43, "echo", b"v?");
    raw[0] = 2;
    lp.router.write_frame(&raw).expect("写链路失败");
    lp.pump_until_idle(2);
    lp.assert_router_idle();
    assert_eq!(lp.host.status().rejected_commands, 2);

    // (c) 信封合法但载荷非法（工具名不是合法 UTF-8）：可关联 id → Failed 回执。
    lp.router
        .write_frame(&stdin_bytes(5, Kind::ToolCall, vec![0xFF, 0xFF, 0x01], 0))
        .expect("写链路失败");
    let frames = lp.router_read_n(1);
    let failed = decode_stdout(&frames[0]);
    assert_eq!(failed.phase, Phase::Failed);
    assert_eq!(failed.cmd_id, 5, "载荷解析失败必须回执到具体命令");
    let error = ErrorPayload::decode(&failed.payload).expect("错误载荷解码失败");
    assert_eq!(error.code, ErrorCode::InvalidArgument);

    // (d) 空帧在链路层直接拒绝，不进入 Worker。
    assert!(lp.router.write_frame(&[]).is_err());

    // (e) 拒绝之后链路依然健康：合法命令照常完成，seq 连续（Hello=1, Failed=2）。
    lp.router
        .write_frame(&task_stdin(44, "echo", b"still-alive"))
        .expect("写链路失败");
    let frames = lp.router_read_n(3);
    assert_eq!(
        phases(&frames),
        [Phase::Accepted, Phase::Started, Phase::Completed]
    );
    let completed = decode_stdout(&frames[2]);
    assert_eq!(completed.cmd_id, 44);
    assert_eq!(completed.seq, 5);
    assert_eq!(completed.payload, b"still-alive");
    lp.assert_router_idle();
}

// ---- 重连：retained Hello 语义 + boot_id/seq 连续性 ----

#[test]
fn reconnect_reemits_hello_and_continues_seq() {
    let mut lp = Loop::new("pc-re", 9);
    let hello = decode_stdout(&lp.router_read());
    assert_eq!((hello.phase, hello.seq), (Phase::Hello, 1));

    // 跑一个任务消耗 seq：Accepted(2) Started(3) Completed(4)。
    lp.router
        .write_frame(&task_stdin(3, "echo", b"x"))
        .expect("写链路失败");
    let _ = lp.router_read_n(3);
    lp.assert_router_idle();

    // 意外断链 → 重连：同 boot_id 重新发布 Hello，seq 接续为 5。
    lp.host.on_link_lost();
    let frames = lp
        .host
        .on_link_up(sekai::sekai_device_list::Endpoint::new("router", 7421).unwrap())
        .expect("重连失败");
    assert_eq!(frames.len(), 1, "重连后应恰好产出一帧 Hello");
    let hello2 = decode_stdout(&frames[0]);
    assert_eq!(hello2.phase, Phase::Hello);
    assert_eq!(hello2.boot_id, 9, "同一启动周期内 boot_id 不变");
    assert_eq!(hello2.seq, 5, "seq 跨重连单调递增");

    // 重连后链路继续可用。
    lp.router
        .write_frame(&tool_stdin(4, "sekai.time", b""))
        .expect("写链路失败");
    let frames = lp.router_read_n(2);
    assert_eq!(phases(&frames), [Phase::Accepted, Phase::Completed]);
    assert_eq!(decode_stdout(&frames[1]).seq, 7);
}

// ---- Stdin 去重：重放最终事件 ----

#[test]
fn duplicate_stdin_replays_final_event() {
    let mut lp = Loop::new("pc-dedup", 3);
    let _hello = lp.router_read();

    let frame = task_stdin(7, "echo", b"once");
    lp.router.write_frame(&frame).expect("写链路失败");
    let first = lp.router_read_n(3);
    assert_eq!(
        phases(&first),
        [Phase::Accepted, Phase::Started, Phase::Completed]
    );

    // 同一 cmd_id 重发：只重放最终事件，不重复执行。
    lp.router.write_frame(&frame).expect("写链路失败");
    let replay = lp.router_read_n(1);
    let replayed = decode_stdout(&replay[0]);
    assert_eq!(replayed.phase, Phase::Completed);
    assert_eq!(replayed.cmd_id, 7);
    assert_eq!(replayed.payload, b"once");
    lp.assert_router_idle();
}

// ---- 协作式任务多周期推进（sleep） ----

#[test]
fn sleep_task_advances_across_polls() {
    let mut lp = Loop::new("pc-sleep", 5);
    let _hello = lp.router_read();

    lp.router
        .write_frame(&task_stdin(8, "sleep", b"30"))
        .expect("写链路失败");
    let frames = lp.router_read_n(2);
    assert_eq!(phases(&frames), [Phase::Accepted, Phase::Started]);
    // 时间未推进：任务保持 Pending，不产生新帧。
    lp.assert_router_idle();

    // 时间推进 31ms：任务在后续 poll 中完成。
    lp.clock.advance(31);
    let frames = lp.router_read_n(1);
    let completed = decode_stdout(&frames[0]);
    assert_eq!(completed.phase, Phase::Completed);
    assert_eq!(completed.cmd_id, 8);
    assert!(completed.payload.is_empty());
    lp.assert_router_idle();
}

// ---- 超时任务按 DDL 上报 Failed(Timeout) ----

#[test]
fn task_exceeding_ddl_fails_with_timeout() {
    let mut lp = Loop::new("pc-ddl", 6);
    let _hello = lp.router_read();

    // sleep 500ms，但 DDL 只有 100ms。
    let body = TaskBody::new("sleep", b"500".to_vec()).unwrap();
    let frame = stdin_bytes(9, Kind::Task, body.encode().unwrap(), 100);
    lp.router.write_frame(&frame).expect("写链路失败");
    let frames = lp.router_read_n(2);
    assert_eq!(phases(&frames), [Phase::Accepted, Phase::Started]);

    lp.clock.advance(101);
    let frames = lp.router_read_n(1);
    let failed = decode_stdout(&frames[0]);
    assert_eq!(failed.phase, Phase::Failed);
    assert_eq!(failed.cmd_id, 9);
    let error = ErrorPayload::decode(&failed.payload).unwrap();
    assert_eq!(error.code, ErrorCode::Timeout);
    lp.assert_router_idle();
}

// ---- pc.exec 任务化：执行期间帧循环仍响应其它命令（B4.2/P3） ----

#[test]
fn pc_exec_task_keeps_runtime_responsive() {
    let mut lp = Loop::new("pc-exec-par", 31);
    let _hello = lp.router_read();

    // powershell Start-Sleep 不依赖网络（受限/离线环境下 ping 会立刻失败）。
    let command = if cfg!(windows) {
        "powershell -NoProfile -Command Start-Sleep 2"
    } else {
        "sleep 1"
    };
    // DDL 放宽：本用例等命令自然结束（不是靠 DDL 超时）。
    lp.router
        .write_frame(&task_stdin_with_ddl(
            41,
            "pc.exec",
            command.as_bytes(),
            60_000,
        ))
        .expect("写链路失败");
    let frames = lp.router_read_n(2);
    assert_eq!(phases(&frames), [Phase::Accepted, Phase::Started]);

    // pc.exec 仍在执行：Worker 必须继续处理其它命令。旧实现（同步 Tool）
    // 会在这里阻塞整个帧循环，sekai.time 不可能被回应。
    lp.router
        .write_frame(&tool_stdin(42, "sekai.time", b""))
        .expect("写链路失败");
    let frames = lp.router_read_n(2);
    assert_eq!(phases(&frames), [Phase::Accepted, Phase::Completed]);
    let time_completed = decode_stdout(&frames[1]);
    assert_eq!(time_completed.cmd_id, 42);
    assert_eq!(time_completed.payload.len(), 16);

    // 此刻 pc.exec 还没有终态帧（命令仍在跑）。
    lp.assert_router_idle();

    // 真实等待子进程退出 + 推进手动时钟，收到 pc.exec 的 Completed。
    let completed = wait_for_cmd_frame(&mut lp, 41, 30_000);
    assert_eq!(completed.phase, Phase::Completed);
    let text = String::from_utf8(completed.payload).expect("exec 结果应为 UTF-8");
    assert!(text.starts_with("exit=0\n"), "{text}");
    lp.assert_router_idle();
}

// ---- pc.exec 任务化：DDL 到期杀进程树并 Failed(Timeout)（B4.3/P1） ----

#[test]
fn pc_exec_task_ddl_fails_with_timeout() {
    let mut lp = Loop::new("pc-exec-ddl", 33);
    let _hello = lp.router_read();

    // powershell Start-Sleep 不依赖网络（受限/离线环境下 ping 会立刻失败）。
    let command = if cfg!(windows) {
        "powershell -NoProfile -Command Start-Sleep 30"
    } else {
        "sleep 30"
    };
    lp.router
        .write_frame(&task_stdin_with_ddl(51, "pc.exec", command.as_bytes(), 100))
        .expect("写链路失败");
    let frames = lp.router_read_n(2);
    assert_eq!(phases(&frames), [Phase::Accepted, Phase::Started]);

    // 越过 DDL：运行时丢弃任务（ExecTask::Drop 杀进程树）并回 Failed(Timeout)。
    lp.clock.advance(101);
    let frames = lp.router_read_n(1);
    let failed = decode_stdout(&frames[0]);
    assert_eq!(failed.phase, Phase::Failed);
    assert_eq!(failed.cmd_id, 51);
    let error = ErrorPayload::decode(&failed.payload).expect("错误载荷解码失败");
    assert_eq!(error.code, ErrorCode::Timeout);
    lp.assert_router_idle();
}
