//! 进程内环回 GPIO/Task 测试：真实 Runtime + VirtualGpio + ControlPlane。
//!
//! 覆盖计划 §8.1 loopback_gpio 行：gpio.info help 逐字、config/write/read、
//! 保留/仅输入引脚错误、Task echo/sleep、多Worker选择、retained Hello 重放、
//! 离线/重启/超时。

mod support;

use sekai::sekai_stdin::Kind;
use sekai::sekai_stdout::Phase;
use sekai::sekai_wire::ErrorCode;
use sekai_gpio::pin::PinId;
use sekai_gpio::tools::INFO_HELP;
use sekai_gpio::{ChipFamily, Level, Port};
use sekai_router::control::{CommandRequest, ControlError};

use support::{DEFAULT_BASE_UNIX_MS, DEFAULT_TOPIC_ROOT, Harness};

fn harness() -> Harness {
    let mut harness = Harness::new(DEFAULT_TOPIC_ROOT, DEFAULT_BASE_UNIX_MS);
    harness.add_worker("esp32-1", ChipFamily::Esp32, &[]);
    harness.connect();
    harness
}

fn tool(worker: &str, name: &str, args: &str) -> CommandRequest {
    CommandRequest::tool(worker, name, args.as_bytes().to_vec())
}

fn text(result: &sekai_router::control::CommandResult) -> String {
    String::from_utf8_lossy(&result.payload).to_string()
}

#[test]
fn gpio_info_help_is_verbatim_contract() {
    let mut harness = harness();
    let result = harness.run_command(tool("esp32-1", "gpio.info", "help"), 2_000);
    assert!(result.ok(), "failure={:?}", result.failure);
    assert_eq!(text(&result), INFO_HELP);
    assert_eq!(
        result.phase_names(),
        vec!["Accepted", "Completed"],
        "Tool 正常应只有 Accepted + Completed"
    );

    let publish = harness
        .last_stdin_publish("esp32-1")
        .expect("应记录一条 in 发布");
    assert_eq!(
        publish.content_type(),
        Some("application/vnd.sekai.stdin.v1")
    );
    assert!(!publish.retain, "in 主题不得 retained");
    assert_eq!(publish.qos, sekai::mqtt::qos::QoS::AtLeastOnce);
    let stdin = publish.decode_stdin().expect("Stdin 可解码");
    assert_eq!(stdin.kind, Kind::ToolCall);
    assert_eq!(stdin.id, result.cmd_id);
    harness.assert_no_errors();
}

#[test]
fn gpio_config_write_then_read_observes_state() {
    let mut harness = harness();
    let config = harness.run_command(tool("esp32-1", "gpio.config", "IO4 out"), 2_000);
    assert!(config.ok(), "failure={:?}", config.failure);
    assert!(text(&config).contains("mode=output"));
    assert!(text(&config).contains("pin=IO4"));

    let write = harness.run_command(tool("esp32-1", "gpio.write", "IO4 1"), 2_000);
    assert!(write.ok(), "failure={:?}", write.failure);
    assert_eq!(text(&write), "pin=IO4 level=1");

    let info = harness.run_command(tool("esp32-1", "gpio.info", "IO4"), 2_000);
    assert!(info.ok());
    let info_text = text(&info);
    assert!(info_text.contains("mode=output"), "info={info_text}");
    assert!(info_text.contains("level=1"), "info={info_text}");
    assert!(info_text.contains("configured=1"), "info={info_text}");
    harness.assert_no_errors();
}

#[test]
fn gpio_read_uses_initial_virtual_input() {
    let mut harness = Harness::new(DEFAULT_TOPIC_ROOT, DEFAULT_BASE_UNIX_MS);
    harness.add_worker("esp32-1", ChipFamily::Esp32, &[(PinId::io(4), Level::High)]);
    harness.connect();
    let config = harness.run_command(tool("esp32-1", "gpio.config", "IO4 in"), 2_000);
    assert!(config.ok(), "failure={:?}", config.failure);
    let read = harness.run_command(tool("esp32-1", "gpio.read", "IO4"), 2_000);
    assert!(read.ok(), "failure={:?}", read.failure);
    assert_eq!(text(&read), "pin=IO4 level=1");
    harness.assert_no_errors();
}

#[test]
fn reserved_pin_is_rejected_by_worker_and_state_unchanged() {
    let mut harness = harness();
    let config = harness.run_command(tool("esp32-1", "gpio.config", "IO6 out"), 2_000);
    assert!(!config.ok(), "保留引脚必须失败");
    let failure = config.failure.expect("失败信息");
    assert_eq!(failure.code, "worker_failed");
    assert_eq!(failure.worker_code, Some(ErrorCode::InvalidArgument));
    assert!(!failure.message.is_empty());

    let info = harness.run_command(tool("esp32-1", "gpio.info", "IO6"), 2_000);
    assert!(info.ok(), "保留引脚允许查看");
    let info_text = text(&info);
    assert!(info_text.contains("mode=unconfigured"), "info={info_text}");
    assert!(!info_text.contains("mode=output"), "info={info_text}");
    harness.assert_no_errors();
}

#[test]
fn input_only_pin_cannot_be_configured_as_output() {
    let mut harness = harness();
    let config = harness.run_command(tool("esp32-1", "gpio.config", "IO34 out"), 2_000);
    assert!(!config.ok());
    assert_eq!(
        config.failure.expect("失败信息").worker_code,
        Some(ErrorCode::InvalidArgument)
    );
    harness.assert_no_errors();
}

#[test]
fn task_echo_and_sleep_lifecycle() {
    let mut harness = harness();
    let echo = harness.run_command(
        CommandRequest::task("esp32-1", "echo", b"hello".to_vec()).with_ddl(1_000),
        5_000,
    );
    assert!(echo.ok(), "failure={:?}", echo.failure);
    assert_eq!(text(&echo), "hello");
    assert_eq!(echo.phase_names(), vec!["Accepted", "Started", "Completed"]);

    // harness 的虚拟Worker sleep 任务参数是毫秒文本（嵌入式 Worker 是 4 字节大端；
    // 两者都是任务自定义参数格式，协议层不解释）。
    let sleep_args = b"150".to_vec();
    let sleep = harness.run_command(
        CommandRequest::task("esp32-1", "sleep", sleep_args).with_ddl(2_000),
        5_000,
    );
    assert!(sleep.ok(), "failure={:?}", sleep.failure);
    assert!(sleep.payload.is_empty());
    assert!(sleep.phase_names().contains(&"Started"));
    assert!(
        sleep.elapsed_ms >= 100,
        "sleep 应至少推进约 150ms，实际 {}",
        sleep.elapsed_ms
    );
    harness.assert_no_errors();
}

#[test]
fn two_workers_are_selected_by_id_and_family() {
    let mut harness = Harness::new(DEFAULT_TOPIC_ROOT, DEFAULT_BASE_UNIX_MS);
    harness.add_worker("esp32-1", ChipFamily::Esp32, &[]);
    harness.add_worker(
        "host-f1",
        ChipFamily::Stm32F1,
        &[(PinId::port(Port::A, 5), Level::High)],
    );
    harness.connect();

    // STM32F1 Worker可以使用 PA5，ESP32 Worker不认识 PA5。
    let read = harness.run_command(tool("host-f1", "gpio.read", "PA5"), 2_000);
    assert!(!read.ok(), "PA5 未配置为输入时必须失败");
    let config = harness.run_command(tool("host-f1", "gpio.config", "PA5 in"), 2_000);
    assert!(config.ok(), "failure={:?}", config.failure);
    let read = harness.run_command(tool("host-f1", "gpio.read", "PA5"), 2_000);
    assert!(read.ok(), "failure={:?}", read.failure);
    assert_eq!(text(&read), "pin=PA5 level=1");

    let esp = harness.run_command(tool("esp32-1", "gpio.config", "PA5 out"), 2_000);
    assert!(!esp.ok(), "ESP32 不应认识 PA5");
    assert_eq!(
        esp.failure.expect("失败信息").worker_code,
        Some(ErrorCode::InvalidArgument)
    );

    // 命令只发给目标Worker：每次 submit 只有一条 in 发布。
    let stdin_topics: Vec<&str> = harness
        .recorded_publishes()
        .iter()
        .filter(|record| record.topic.ends_with("/in"))
        .map(|record| record.topic.as_str())
        .collect();
    assert!(stdin_topics.iter().all(|topic| {
        topic.starts_with("sekai/v1/dev/host-f1/in") || topic.starts_with("sekai/v1/dev/esp32-1/in")
    }));
    harness.assert_no_errors();
}

#[test]
fn retained_hello_replay_after_server_reconnect() {
    let mut harness = harness();
    let result = harness.run_command(tool("esp32-1", "gpio.info", "IO4"), 2_000);
    assert!(result.ok());

    harness.server_link_lost();
    assert!(
        !harness
            .server()
            .worker("esp32-1")
            .map(|d| d.online)
            .unwrap_or(false)
    );

    harness.server_reconnect();
    assert!(harness.server().is_ready());
    let worker = harness
        .server()
        .worker("esp32-1")
        .expect("重连后应重新发现Worker");
    assert!(worker.online, "retained Hello 应恢复在线状态");
    let result = harness.run_command(tool("esp32-1", "gpio.info", "IO4"), 2_000);
    assert!(result.ok(), "重连后命令应可用：{:?}", result.failure);
    harness.assert_no_errors();
}

#[test]
fn offline_worker_rejects_submit_until_reconnect() {
    let mut harness = harness();
    harness.worker_link_lost("esp32-1");
    assert!(
        !harness
            .server()
            .worker("esp32-1")
            .map(|d| d.online)
            .unwrap_or(false),
        "LWT Offline 后Worker应离线"
    );
    let error = harness
        .try_submit(tool("esp32-1", "gpio.info", "IO4"))
        .expect_err("离线Worker必须拒绝提交");
    assert!(matches!(error, ControlError::WorkerOffline(_)), "{error:?}");

    harness.worker_reconnect("esp32-1");
    let result = harness.run_command(tool("esp32-1", "gpio.info", "IO4"), 2_000);
    assert!(result.ok(), "failure={:?}", result.failure);
    harness.assert_no_errors();
}

#[test]
fn worker_reboot_fails_inflight_commands() {
    let mut harness = harness();
    harness.silence_worker("esp32-1", true);
    let cmd_id = harness
        .submit(CommandRequest::task("esp32-1", "sleep", b"60000".to_vec()).with_ddl(60_000));
    harness.settle();
    assert!(harness.finished(cmd_id).is_none());

    harness.restart_worker("esp32-1", 2);
    let result = harness.finished(cmd_id).expect("重启必须结束在途命令");
    let failure = result.failure.expect("失败信息");
    assert_eq!(failure.code, "worker_rebooted");
    assert_eq!(
        harness.server().worker("esp32-1").map(|d| d.boot_id),
        Some(2)
    );
}

#[test]
fn second_inflight_command_is_rejected() {
    let mut harness = harness();
    harness.silence_worker("esp32-1", true);
    harness.submit(CommandRequest::task("esp32-1", "sleep", b"60000".to_vec()).with_ddl(60_000));
    harness.settle();
    let error = harness
        .try_submit(tool("esp32-1", "gpio.info", "IO4"))
        .expect_err("每Worker只允许 1 条在途命令");
    assert!(matches!(error, ControlError::WorkerBusy(_)), "{error:?}");
}

#[test]
fn timeout_ignores_late_final_event() {
    let mut harness = harness();
    harness.silence_worker("esp32-1", true);
    let request = tool("esp32-1", "gpio.info", "IO4")
        .with_ddl(100)
        .with_timeout(200);
    let cmd_id = harness.submit(request);
    harness.settle();
    assert!(harness.finished(cmd_id).is_none());

    // 推进超过控制端超时：命令以 timeout 结束。
    harness.advance(400);
    let result = harness.finished(cmd_id).expect("超时后应结束");
    let failure = result.failure.expect("失败信息");
    assert_eq!(failure.code, "timeout");

    // 恢复Worker出站，让Worker把晚到的最终事件发回来；服务端应忽略而不是重复完成。
    harness.silence_worker("esp32-1", false);
    harness.advance(500);
    assert_eq!(
        harness.finished_count(cmd_id),
        1,
        "晚到最终事件不得再次触发 CommandFinished"
    );
}

#[test]
fn unknown_tool_is_rejected_before_publish() {
    let mut harness = harness();
    let publishes_before = harness
        .recorded_publishes()
        .iter()
        .filter(|record| record.topic.ends_with("/in"))
        .count();
    let error = harness
        .try_submit(tool("esp32-1", "no.such.tool", ""))
        .expect_err("未通告工具必须拒绝");
    assert!(
        matches!(error, ControlError::NotAdvertised { .. }),
        "{error:?}"
    );
    let publishes_after = harness
        .recorded_publishes()
        .iter()
        .filter(|record| record.topic.ends_with("/in"))
        .count();
    assert_eq!(publishes_before, publishes_after, "拒绝时不得发布 Stdin");
}

#[test]
fn phases_only_include_observed_events() {
    let mut harness = harness();
    let result = harness.run_command(tool("esp32-1", "gpio.info", ""), 2_000);
    assert!(result.ok());
    let phases: Vec<Phase> = result.phases.clone();
    assert_eq!(phases.first(), Some(&Phase::Accepted));
    assert_eq!(phases.last(), Some(&Phase::Completed));
    assert!(!phases.contains(&Phase::Started), "Tool 不应出现 Started");
}
