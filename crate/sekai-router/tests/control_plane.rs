//! 控制平面行为测试（计划 §4.6 / §8.1 / §9）。
//!
//! 使用 `tests/support` 的进程内环回 harness：真实 `ControlPlane`、真实Worker
//! `Runtime`（虚拟 GPIO）与内存 MQTT broker，手动时钟推进，无网络依赖。

mod support;

use sekai::mqtt::qos::QoS;
use sekai::mqtt::reason::ReasonCode;
use sekai::sekai_device_list::DeviceList;
use sekai::sekai_runtime::{CONTENT_TYPE_STDIN, CONTENT_TYPE_STDOUT};
use sekai::sekai_stdin::{JumpBody, Kind, Stdin, ToolBody};
use sekai::sekai_stdout::{Hello, Phase, Stdout};
use sekai::sekai_wire::ErrorCode;
use sekai_gpio::{ChipFamily, Level, PinId, Port};
use sekai_router::control::{
    CommandKind, CommandRequest, ControlError, ControlEvent, TASK_TIMEOUT_EXTRA_MS,
    TOOL_TIMEOUT_EXTRA_MS,
};

use support::{DEFAULT_BASE_UNIX_MS, DEFAULT_TOPIC_ROOT, Harness, SERVER_CLIENT, SERVER_CLIENT_ID};

// ---- 辅助 ----

fn base_harness() -> Harness {
    let mut harness = Harness::new(DEFAULT_TOPIC_ROOT, DEFAULT_BASE_UNIX_MS);
    harness.add_worker("esp32-1", ChipFamily::Esp32, &[(PinId::io(4), Level::High)]);
    harness.connect();
    harness
}

fn two_worker_harness() -> Harness {
    let mut harness = Harness::new(DEFAULT_TOPIC_ROOT, DEFAULT_BASE_UNIX_MS);
    harness.add_worker("esp32-1", ChipFamily::Esp32, &[(PinId::io(4), Level::High)]);
    harness.add_worker(
        "stm32-1",
        ChipFamily::Stm32F1,
        &[(PinId::port(Port::A, 5), Level::High)],
    );
    harness.connect();
    harness
}

fn text(payload: &[u8]) -> String {
    String::from_utf8_lossy(payload).into_owned()
}

fn hello(worker_id: &str, tools: &[&str], tasks: &[&str]) -> Hello {
    Hello {
        worker_id: String::from(worker_id),
        firmware: String::from("9.9.9"),
        uptime_ms: 42,
        unix_ms: DEFAULT_BASE_UNIX_MS,
        tasks: tasks.iter().map(|name| String::from(*name)).collect(),
        tools: tools.iter().map(|name| String::from(*name)).collect(),
        workers: DeviceList::new(),
        active_tasks: Vec::new(),
    }
}

fn worker_updated_count(harness: &Harness, worker_id: &str) -> usize {
    harness
        .events()
        .iter()
        .filter(|event| {
            matches!(event, ControlEvent::WorkerUpdated(info) if info.worker_id == worker_id)
        })
        .count()
}

fn stdin_publish_count(harness: &Harness, worker_id: &str) -> usize {
    let topic = harness.topic_root().worker_in(worker_id);
    harness
        .recorded_publishes()
        .iter()
        .filter(|record| record.from == SERVER_CLIENT && record.topic == topic)
        .count()
}

// ---- CONNECT / CONNACK / SUBSCRIBE ----

#[test]
fn connect_connack_and_subscribe_parameters() {
    let harness = base_harness();
    assert!(harness.server().is_connected());
    assert!(harness.server().is_ready());
    harness.assert_no_errors();

    // CONNECT：服务端参数符合计划 §4.1。
    let connects = harness.broker().connects();
    assert_eq!(connects.len(), 2, "server + worker CONNECT");
    let server = connects
        .iter()
        .find(|record| record.client == SERVER_CLIENT)
        .expect("缺少服务端 CONNECT");
    assert_eq!(server.client_id, SERVER_CLIENT_ID);
    assert!(!server.client_id.is_empty());
    assert!(server.clean_start);
    assert_eq!(server.session_expiry_interval, Some(0));
    assert_eq!(server.keep_alive, 30);
    assert_eq!(server.receive_maximum, Some(8));
    assert_eq!(server.maximum_packet_size, Some(16 * 1024));
    assert!(server.will.is_none(), "服务端不应注册遗嘱");
    assert!(server.username.is_none());

    // CONNACK：session_present=false、能力属性正确。
    let connack = harness
        .broker()
        .connacks()
        .iter()
        .find(|record| record.client == SERVER_CLIENT)
        .expect("缺少服务端 CONNACK");
    assert!(!connack.connack.session_present);
    assert_eq!(connack.connack.reason_code, ReasonCode::Success);
    let properties = &connack.connack.properties;
    assert_eq!(properties.receive_maximum, Some(8));
    assert_eq!(properties.maximum_packet_size, Some(16 * 1024));
    assert_eq!(properties.retain_available, Some(true));
    assert_eq!(properties.wildcard_subscription_available, Some(true));
    assert_eq!(properties.maximum_qos, Some(1));
    assert_eq!(properties.topic_alias_maximum, Some(0));

    // SUBSCRIBE：一次请求携带 out/state 两条过滤器，QoS1 granted。
    let subscribe = harness
        .broker()
        .subscribes()
        .iter()
        .find(|record| record.client == SERVER_CLIENT)
        .expect("缺少服务端 SUBSCRIBE");
    assert_eq!(subscribe.subscriptions.len(), 2);
    let mut filters: Vec<&str> = subscribe
        .subscriptions
        .iter()
        .map(|subscription| subscription.filter.as_str())
        .collect();
    filters.sort_unstable();
    let mut expected = vec![
        harness.topic_root().out_filter(),
        harness.topic_root().state_filter(),
    ];
    expected.sort();
    assert_eq!(filters, expected);
    for subscription in &subscribe.subscriptions {
        assert_eq!(subscription.options.qos, QoS::AtLeastOnce);
        assert!(!subscription.options.no_local);
        assert!(subscription.options.retain_as_published);
        assert_eq!(subscription.options.retain_handling, 0);
    }
}

#[test]
fn retained_hello_discovers_worker_capabilities() {
    let harness = base_harness();
    let info = harness.server().worker("esp32-1").expect("Worker应已发现");
    assert!(info.online);
    assert_eq!(info.worker_id, "esp32-1");
    assert_eq!(info.boot_id, 1);
    assert_eq!(info.firmware, support::DEFAULT_FIRMWARE);
    assert!(info.unix_ms > 0);
    assert!(info.hello_seq > 0);
    assert!(info.active_tasks.is_empty());

    for tool in [
        "gpio.config",
        "gpio.info",
        "gpio.read",
        "gpio.write",
        "sekai.info",
        "sekai.time",
        "sekai.cancel",
        "sekai.workers",
        "sekai.workers.set",
        "sekai.set_time",
    ] {
        assert!(info.has_tool(tool), "Worker应通告工具 {tool}");
    }
    assert!(info.has_task("echo"));
    assert!(info.has_task("sleep"));
    assert!(!info.has_tool("no.such.tool"));

    // retained Hello 存在且可解码。
    let state_topic = harness.topic_root().worker_state("esp32-1");
    let retained = harness
        .broker()
        .retained(&state_topic)
        .expect("state 主题应有 retained Hello");
    let stdout = Stdout::decode(&retained.payload).expect("Stdout 解码");
    assert_eq!(stdout.phase, Phase::Hello);
    let decoded = Hello::decode(&stdout.payload).expect("Hello 解码");
    assert_eq!(decoded.worker_id, "esp32-1");

    // list_workers 返回稳定顺序。
    let workers = harness.server().workers();
    assert_eq!(workers.len(), 1);
    assert_eq!(workers[0].worker_id, "esp32-1");
}

// ---- 发布 Stdin 的字段 ----

#[test]
fn stdin_publish_uses_qos1_without_retain_and_stdin_content_type() {
    let mut harness = base_harness();
    let baseline = harness.now_unix_ms();
    let cmd_id = harness.submit(CommandRequest::tool("esp32-1", "gpio.info", "IO4"));
    harness.settle();

    let record = harness.last_stdin_publish("esp32-1").expect("应发布 Stdin");
    assert_eq!(record.from, SERVER_CLIENT);
    assert_eq!(record.topic, harness.topic_root().worker_in("esp32-1"));
    assert_eq!(record.qos, QoS::AtLeastOnce);
    assert!(!record.retain, "in 主题禁止 retained");
    assert_eq!(record.content_type(), Some(CONTENT_TYPE_STDIN));

    let stdin = record.decode_stdin().expect("Stdin 可解码");
    assert_eq!(stdin.id, cmd_id);
    assert_eq!(stdin.kind, Kind::ToolCall);
    assert_eq!(stdin.delay_ms, 0);
    assert_eq!(stdin.timestamp, baseline);
    assert!(cmd_id > 0);
    assert!(cmd_id >= baseline, "cmd_id 使用时间种子避免重启冲突");
}

// ---- Accepted / Started / Completed / Failed ----

#[test]
fn tool_lifecycle_is_accepted_then_completed() {
    let mut harness = base_harness();
    let result = harness.run_command(CommandRequest::tool("esp32-1", "gpio.info", "IO4"), 2_000);
    assert!(result.ok());
    assert_eq!(result.kind, CommandKind::Tool);
    assert_eq!(result.name, "gpio.info");
    assert_eq!(result.worker_id, "esp32-1");
    assert_eq!(result.phase_names(), vec!["Accepted", "Completed"]);
    let payload = text(&result.payload);
    assert!(payload.contains("pin=IO4"), "payload = {payload}");
    assert!(payload.contains("mode=unconfigured"), "payload = {payload}");
    assert_eq!(harness.finished_count(result.cmd_id), 1);
    harness.assert_no_errors();
}

#[test]
fn task_lifecycle_is_accepted_started_completed() {
    let mut harness = base_harness();
    let result = harness.run_command(
        CommandRequest::task("esp32-1", "echo", b"hello".to_vec()),
        2_000,
    );
    assert!(result.ok());
    assert_eq!(result.kind, CommandKind::Task);
    assert_eq!(
        result.phase_names(),
        vec!["Accepted", "Started", "Completed"]
    );
    assert_eq!(result.payload, b"hello");
    harness.assert_no_errors();
}

#[test]
fn failed_lifecycle_maps_worker_error() {
    let mut harness = base_harness();
    let result = harness.run_command(CommandRequest::tool("esp32-1", "gpio.read", "IO4"), 2_000);
    assert!(!result.ok());
    assert_eq!(result.phase_names(), vec!["Accepted", "Failed"]);
    assert!(result.payload.is_empty());
    let failure = result.failure.expect("应有失败信息");
    assert_eq!(failure.code, "worker_failed");
    assert_eq!(failure.worker_code, Some(ErrorCode::InvalidArgument));
    assert!(
        failure.message.contains("未配置"),
        "message = {}",
        failure.message
    );
    assert!(!failure.retryable);
    harness.assert_no_errors();
}

#[test]
fn gpio_config_write_then_read_changes_worker_state() {
    let mut harness = base_harness();
    let configured = harness.run_command(
        CommandRequest::tool("esp32-1", "gpio.config", "IO4 out level=0"),
        2_000,
    );
    assert!(configured.ok());
    assert!(text(&configured.payload).contains("mode=output"));

    let written = harness.run_command(
        CommandRequest::tool("esp32-1", "gpio.write", "IO4 1"),
        2_000,
    );
    assert!(written.ok());
    assert!(text(&written.payload).contains("level=1"));

    let info = harness.run_command(CommandRequest::tool("esp32-1", "gpio.info", "IO4"), 2_000);
    assert!(info.ok());
    let payload = text(&info.payload);
    assert!(payload.contains("mode=output"), "payload = {payload}");
    assert!(payload.contains("level=1"), "payload = {payload}");
}

// ---- cmd_id ----

#[test]
fn cmd_ids_are_monotonic_nonzero_and_per_worker() {
    let mut harness = two_worker_harness();
    let mut esp32_ids = Vec::new();
    for _ in 0..3 {
        let result =
            harness.run_command(CommandRequest::tool("esp32-1", "gpio.info", "IO4"), 2_000);
        esp32_ids.push(result.cmd_id);
    }
    for pair in esp32_ids.windows(2) {
        assert!(pair[1] > pair[0], "每Worker cmd_id 必须严格递增");
    }
    assert!(esp32_ids.iter().all(|id| *id != 0));

    let mut stm32_ids = Vec::new();
    for _ in 0..2 {
        let result =
            harness.run_command(CommandRequest::tool("stm32-1", "gpio.info", "PA5"), 2_000);
        stm32_ids.push(result.cmd_id);
    }
    assert!(stm32_ids[1] > stm32_ids[0]);
    assert!(stm32_ids.iter().all(|id| *id != 0));

    // 不同Worker之间不重用 id。
    for esp in &esp32_ids {
        assert!(!stm32_ids.contains(esp), "cmd_id 不能跨Worker重用");
    }
    // 同一Worker继续递增（不因其他Worker提交而倒退）。
    let again = harness.run_command(CommandRequest::tool("esp32-1", "gpio.info", "IO4"), 2_000);
    assert!(again.cmd_id > esp32_ids[2]);
}

// ---- 超时（手动时钟覆盖 tool / task 额外余量） ----

#[test]
fn tool_timeout_uses_ddl_plus_extra_margin() {
    let mut harness = base_harness();
    // 静默Worker出站报文：Worker处理命令但服务端收不到任何事件。
    harness.silence_worker("esp32-1", true);
    let expected_timeout =
        u64::from(harness.control_config().default_ddl_ms) + TOOL_TIMEOUT_EXTRA_MS;
    let cmd_id = harness.submit(CommandRequest::tool("esp32-1", "gpio.info", "IO4"));
    harness.settle();
    assert!(harness.finished(cmd_id).is_none());

    harness.advance(expected_timeout - 1);
    assert!(
        harness.finished(cmd_id).is_none(),
        "余量为 {TOOL_TIMEOUT_EXTRA_MS}ms，提前超时"
    );
    harness.advance(1);
    let result = harness.finished(cmd_id).expect("应超时返回最终事件");
    let failure = result.failure.expect("超时必须有失败信息");
    assert_eq!(failure.code, "timeout");
    assert!(failure.retryable);
    assert_eq!(harness.finished_count(cmd_id), 1);
    // 最终事件晚到时不再改变结果。
    harness.advance(5_000);
    assert_eq!(harness.finished_count(cmd_id), 1);
}

#[test]
fn task_timeout_uses_task_extra_margin() {
    let mut harness = base_harness();
    harness.silence_worker("esp32-1", true);
    let expected_timeout =
        u64::from(harness.control_config().default_ddl_ms) + TASK_TIMEOUT_EXTRA_MS;
    // ddl=0 时使用 ControlPlane 默认 DDL（500ms），加上 Task 额外余量。
    let cmd_id = harness.submit(CommandRequest::task("esp32-1", "sleep", b"600000".to_vec()));
    harness.settle();
    assert!(harness.finished(cmd_id).is_none());
    harness.advance(expected_timeout - 1);
    assert!(harness.finished(cmd_id).is_none());
    harness.advance(1);
    let result = harness.finished(cmd_id).expect("应超时返回最终事件");
    assert_eq!(result.failure.expect("超时失败").code, "timeout");
}

#[test]
fn explicit_timeout_override_is_honoured() {
    let mut harness = base_harness();
    harness.silence_worker("esp32-1", true);
    let cmd_id =
        harness.submit(CommandRequest::tool("esp32-1", "gpio.info", "IO4").with_timeout(120));
    harness.settle();
    harness.advance(119);
    assert!(harness.finished(cmd_id).is_none());
    harness.advance(1);
    assert_eq!(
        harness
            .finished(cmd_id)
            .expect("覆盖超时应生效")
            .failure
            .expect("失败信息")
            .code,
        "timeout"
    );
}

// ---- 断链 / 重连 / 重启 / 离线 ----

#[test]
fn link_lost_fails_pending_then_retained_hello_rediscovers() {
    let mut harness = base_harness();
    harness.silence_worker("esp32-1", true);
    let cmd_id = harness
        .submit(CommandRequest::task("esp32-1", "sleep", b"60000".to_vec()).with_ddl(60_000));
    harness.settle();
    assert!(harness.finished(cmd_id).is_none());

    harness.server_link_lost();
    let result = harness.finished(cmd_id).expect("断链必须失败所有在途命令");
    assert_eq!(result.failure.expect("失败信息").code, "link_lost");
    assert!(!harness.server().is_ready());
    assert!(!harness.server().is_connected());
    assert!(!harness.server().worker("esp32-1").unwrap().online);

    let publishes_before = harness.recorded_publishes().len();
    harness.server_reconnect();
    assert!(harness.server().is_ready());
    assert!(harness.server().worker("esp32-1").unwrap().online);
    // Worker连接没有断，不会重新发布 Hello；服务端只能通过 broker 回放的
    // retained Hello 重新发现Worker。
    let replayed_hello = harness.recorded_publishes()[publishes_before..]
        .iter()
        .any(|record| {
            record.delivered_to.contains(&SERVER_CLIENT)
                && record
                    .decode_stdout()
                    .map(|frame| frame.phase == Phase::Hello)
                    .unwrap_or(false)
        });
    assert!(
        replayed_hello,
        "重连后应回放 retained Hello 以重新发现Worker"
    );
    assert_eq!(harness.finished_count(cmd_id), 1, "不得自动重发命令");
    harness.assert_no_errors();
}

#[test]
fn worker_reboot_fails_pending_and_updates_boot_id() {
    let mut harness = base_harness();
    harness.silence_worker("esp32-1", true);
    let cmd_id = harness
        .submit(CommandRequest::task("esp32-1", "sleep", b"60000".to_vec()).with_ddl(60_000));
    harness.settle();
    assert!(harness.finished(cmd_id).is_none());

    harness.restart_worker("esp32-1", 2);
    let result = harness
        .finished(cmd_id)
        .expect("Worker重启必须失败在途命令");
    assert_eq!(result.failure.expect("失败信息").code, "worker_rebooted");

    let info = harness.server().worker("esp32-1").unwrap();
    assert!(info.online);
    assert_eq!(info.boot_id, 2);
    assert!(info.has_tool("gpio.info"));
    assert_eq!(harness.finished_count(cmd_id), 1);
    harness.assert_no_errors();
}

#[test]
fn retained_offline_marks_worker_offline_and_keeps_capabilities() {
    let mut harness = base_harness();
    harness.silence_worker("esp32-1", true);
    let cmd_id = harness
        .submit(CommandRequest::task("esp32-1", "sleep", b"60000".to_vec()).with_ddl(60_000));
    harness.settle();

    harness.publish_retained_offline("esp32-1");
    harness.settle();
    let result = harness.finished(cmd_id).expect("离线下线必须失败在途命令");
    assert_eq!(result.failure.expect("失败信息").code, "worker_offline");
    let info = harness.server().worker("esp32-1").unwrap();
    assert!(!info.online);
    assert!(!info.tools.is_empty(), "离线后保留最后能力摘要");
    assert!(info.has_tool("gpio.info"));
    assert_eq!(
        harness
            .try_submit(CommandRequest::tool("esp32-1", "gpio.info", "IO4"))
            .unwrap_err(),
        ControlError::WorkerOffline(String::from("esp32-1"))
    );

    // 新的 Hello 让Worker重新上线。
    let frame = Stdout::new(
        1,
        0,
        900,
        Phase::Hello,
        harness.now_unix_ms(),
        hello("esp32-1", &["gpio.info"], &["echo"])
            .encode()
            .expect("Hello 编码"),
    )
    .expect("Hello 帧");
    harness.inject_stdout("esp32-1", &frame, true, Some(CONTENT_TYPE_STDOUT));
    harness.settle();
    assert!(harness.server().worker("esp32-1").unwrap().online);
}

#[test]
fn lwt_offline_fails_pending_with_worker_offline() {
    let mut harness = base_harness();
    harness.silence_worker("esp32-1", true);
    let cmd_id = harness
        .submit(CommandRequest::task("esp32-1", "sleep", b"60000".to_vec()).with_ddl(60_000));
    harness.settle();
    assert!(harness.finished(cmd_id).is_none());

    harness.worker_link_lost("esp32-1");
    let result = harness.finished(cmd_id).expect("LWT 离线应失败在途命令");
    assert_eq!(result.failure.expect("失败信息").code, "worker_offline");
    assert!(!harness.server().worker("esp32-1").unwrap().online);
}

// ---- 未知命令 / 重复帧 / 能力更新 ----

#[test]
fn unknown_cmd_id_final_frames_are_recorded_as_late() {
    let mut harness = base_harness();
    let before = harness.command_results().len();
    let frame = Stdout::new(
        1,
        424_242,
        777,
        Phase::Completed,
        harness.now_unix_ms(),
        b"late".to_vec(),
    )
    .expect("Stdout 帧");
    harness.inject_stdout("esp32-1", &frame, false, Some(CONTENT_TYPE_STDOUT));
    harness.settle();
    assert_eq!(
        harness.command_results().len(),
        before,
        "未知 cmd_id 不产生结果"
    );
    // B8：记为晚到终态（环形 + 计数），而不是写 last_error。
    let diagnostics = harness.server().diagnostics(harness.now_mono_ms());
    assert_eq!(diagnostics.counters.late_results, 1);
    let recorded = diagnostics
        .late_results
        .first()
        .expect("未知 cmd_id 应进 late_results");
    assert_eq!(recorded.cmd_id, 424_242);
    assert_eq!(recorded.worker_id, "esp32-1");
    assert_eq!(recorded.code, "completed");
    assert!(
        !harness
            .server()
            .last_error()
            .unwrap_or_default()
            .contains("424242"),
        "last_error = {:?}",
        harness.server().last_error()
    );
}

#[test]
fn duplicate_boot_id_seq_frames_are_deduped() {
    let mut harness = base_harness();
    let result = harness.run_command(CommandRequest::tool("esp32-1", "gpio.info", "IO4"), 2_000);
    // 找到真正的 Completed 帧记录并重放（模拟 QoS1 重复投递）。
    let completed_index = harness
        .recorded_publishes()
        .iter()
        .position(|record| {
            record
                .decode_stdout()
                .map(|frame| frame.phase == Phase::Completed && frame.cmd_id == result.cmd_id)
                .unwrap_or(false)
        })
        .expect("应存在 Completed 发布记录");
    let finished_before = harness.finished_count(result.cmd_id);
    harness.replay_publish(completed_index);
    harness.settle();
    assert_eq!(harness.finished_count(result.cmd_id), finished_before);

    // 重复 Hello：只更新 last_seen，不重复产生Worker更新事件。
    let hello_index = harness
        .recorded_publishes()
        .iter()
        .position(|record| {
            record
                .decode_stdout()
                .map(|frame| frame.phase == Phase::Hello)
                .unwrap_or(false)
        })
        .expect("应存在 Hello 发布记录");
    let updates_before = worker_updated_count(&harness, "esp32-1");
    harness.replay_publish(hello_index);
    harness.settle();
    assert_eq!(worker_updated_count(&harness, "esp32-1"), updates_before);
    harness.assert_no_errors();
}

#[test]
fn hello_updates_capabilities_and_submit_uses_latest_snapshot() {
    let mut harness = base_harness();
    let custom = hello(
        "esp32-1",
        &["custom.tool", "gpio.info", "sekai.info"],
        &["custom-task"],
    );
    harness
        .inject_hello("esp32-1", &custom, 1, 500, true)
        .expect("注入 Hello");
    harness.settle();

    let info = harness.server().worker("esp32-1").unwrap();
    assert!(info.has_tool("custom.tool"));
    assert!(info.has_task("custom-task"));
    assert!(!info.has_tool("gpio.config"), "能力清单应被最新 Hello 覆盖");

    // 已被移除的工具：提交前拒绝，不产生 Stdin。
    let stdin_before = stdin_publish_count(&harness, "esp32-1");
    let error = harness
        .try_submit(CommandRequest::tool("esp32-1", "gpio.config", "IO4 out"))
        .unwrap_err();
    assert!(matches!(error, ControlError::NotAdvertised { .. }));
    harness.settle();
    assert_eq!(stdin_publish_count(&harness, "esp32-1"), stdin_before);

    // 新通告的工具：控制平面放行，Worker返回 UnknownTool。
    let result = harness.run_command(CommandRequest::tool("esp32-1", "custom.tool", ""), 2_000);
    assert!(!result.ok());
    let failure = result.failure.expect("失败信息");
    assert_eq!(failure.code, "worker_failed");
    assert_eq!(failure.worker_code, Some(ErrorCode::UnknownTool));
}

#[test]
fn hello_with_mismatched_worker_id_is_ignored() {
    let mut harness = base_harness();
    let wrong = hello("other-worker", &["gpio.info"], &["echo"]);
    harness
        .inject_hello("esp32-1", &wrong, 1, 800, true)
        .expect("注入 Hello");
    harness.settle();
    let info = harness.server().worker("esp32-1").unwrap();
    assert!(info.online);
    assert!(info.has_tool("gpio.config"), "不一致的 Hello 不应覆盖能力");
    assert!(
        harness
            .server()
            .last_error()
            .unwrap_or_default()
            .contains("不一致"),
        "last_error = {:?}",
        harness.server().last_error()
    );
}

// ---- 提交前检查 ----

#[test]
fn submit_prechecks_reject_before_publishing_stdin() {
    let mut harness = Harness::new(DEFAULT_TOPIC_ROOT, DEFAULT_BASE_UNIX_MS);
    assert_eq!(
        harness
            .try_submit(CommandRequest::tool("esp32-1", "gpio.info", ""))
            .unwrap_err(),
        ControlError::NotReady
    );

    harness.add_worker("esp32-1", ChipFamily::Esp32, &[]);
    harness.connect();
    let stdin_before = stdin_publish_count(&harness, "esp32-1");

    assert!(matches!(
        harness
            .try_submit(CommandRequest::tool("missing", "gpio.info", ""))
            .unwrap_err(),
        ControlError::WorkerNotFound(_)
    ));
    assert!(matches!(
        harness
            .try_submit(CommandRequest::tool("esp32-1", "no.such.tool", ""))
            .unwrap_err(),
        ControlError::NotAdvertised { .. }
    ));
    assert!(matches!(
        harness
            .try_submit(CommandRequest::task(
                "esp32-1",
                "no-such-task",
                b"x".to_vec()
            ))
            .unwrap_err(),
        ControlError::NotAdvertised { .. }
    ));

    // 在途上限：默认每Worker 1 条。
    harness.silence_worker("esp32-1", true);
    let _pending = harness
        .submit(CommandRequest::task("esp32-1", "sleep", b"60000".to_vec()).with_ddl(60_000));
    harness.settle();
    assert!(matches!(
        harness
            .try_submit(CommandRequest::tool("esp32-1", "gpio.info", ""))
            .unwrap_err(),
        ControlError::WorkerBusy(_)
    ));

    // 离线Worker：标记离线后拒绝。
    harness.publish_retained_offline("esp32-1");
    harness.settle();
    assert!(matches!(
        harness
            .try_submit(CommandRequest::tool("esp32-1", "gpio.info", ""))
            .unwrap_err(),
        ControlError::WorkerOffline(_)
    ));
    harness.settle();
    assert_eq!(
        stdin_publish_count(&harness, "esp32-1"),
        stdin_before + 1,
        "被拒绝的提交不得产生 Stdin"
    );
}

// ---- 主题根命名空间 ----

#[test]
fn namespaced_topic_root_works_end_to_end() {
    let topic_root = "factory-a/sekai/v1";
    let mut harness = Harness::new(topic_root, DEFAULT_BASE_UNIX_MS);
    harness.add_worker("esp32-1", ChipFamily::Esp32, &[]);
    harness.connect();
    assert!(harness.server().worker("esp32-1").unwrap().online);

    let result = harness.run_command(CommandRequest::tool("esp32-1", "gpio.info", "IO4"), 2_000);
    assert!(result.ok());
    let expected_topic = format!("{topic_root}/dev/esp32-1/in");
    assert_eq!(
        harness.last_stdin_publish("esp32-1").unwrap().topic,
        expected_topic
    );
    let out_topic = harness.topic_root().worker_out("esp32-1");
    assert!(out_topic.ends_with("/dev/esp32-1/out"));
    assert!(out_topic.starts_with("factory-a/"));
    harness.assert_no_errors();
}

// ---- 非法/忽略的入站帧 ----

#[test]
fn malformed_stdout_wrong_content_type_and_bad_topic_are_ignored() {
    let mut harness = base_harness();
    let before = harness.command_results().len();

    // 非法 Stdout 载荷。
    harness.inject_publish(
        "esp32-1",
        harness.topic_root().worker_out("esp32-1"),
        b"\xff\xfe".to_vec(),
        false,
        Some(CONTENT_TYPE_STDOUT),
    );
    harness.settle();
    assert_eq!(harness.command_results().len(), before);
    assert!(
        harness
            .server()
            .last_error()
            .unwrap_or_default()
            .contains("Stdout 解码失败")
    );

    // 无法解析的Worker主题（worker id 含空格）。
    harness.inject_publish(
        "esp32-1",
        String::from("sekai/v1/dev/bad id/out"),
        b"x".to_vec(),
        false,
        Some(CONTENT_TYPE_STDOUT),
    );
    harness.settle();
    assert!(
        harness
            .server()
            .last_error()
            .unwrap_or_default()
            .contains("无法解析")
    );

    // content_type 不匹配的合法 Stdout：忽略。
    harness.silence_worker("esp32-1", true);
    let cmd_id = harness.submit(CommandRequest::tool("esp32-1", "gpio.info", "IO4"));
    harness.settle();
    let frame = Stdout::new(
        1,
        cmd_id,
        901,
        Phase::Completed,
        harness.now_unix_ms(),
        b"x".to_vec(),
    )
    .expect("Stdout 帧");
    harness.inject_stdout("esp32-1", &frame, false, Some("text/plain"));
    harness.settle();
    assert!(harness.finished(cmd_id).is_none());
    assert!(
        harness
            .server()
            .last_error()
            .unwrap_or_default()
            .contains("content_type")
    );

    // 同一帧改用正确 content_type 可以被接受。
    let frame = Stdout::new(
        1,
        cmd_id,
        902,
        Phase::Completed,
        harness.now_unix_ms(),
        b"done".to_vec(),
    )
    .expect("Stdout 帧");
    harness.inject_stdout("esp32-1", &frame, false, Some(CONTENT_TYPE_STDOUT));
    harness.settle();
    let result = harness
        .finished(cmd_id)
        .expect("正确 content_type 应被接受");
    assert!(result.ok());
    assert_eq!(result.payload, b"done");

    // 缺失 content_type 的 Hello 也可接受（协议允许Worker不携带）。
    let missing = hello("esp32-1", &["gpio.info"], &["echo"]);
    harness
        .inject_hello("esp32-1", &missing, 1, 903, true)
        .expect("注入 Hello");
    harness.settle();
    assert_eq!(harness.server().worker("esp32-1").unwrap().hello_seq, 903);
}

#[test]
fn late_final_event_after_timeout_is_recorded_as_late_result() {
    let mut harness = base_harness();
    harness.silence_worker("esp32-1", true);
    let cmd_id =
        harness.submit(CommandRequest::tool("esp32-1", "gpio.info", "IO4").with_timeout(50));
    harness.settle();
    harness.advance(50);
    let result = harness.finished(cmd_id).expect("应超时");
    assert_eq!(result.failure.expect("失败信息").code, "timeout");
    let finished_before = harness.finished_count(cmd_id);

    // B8：最终事件晚到（已超时）不再当作正常终态，也不污染 last_error，
    // 而是进环形 late_results 供 status/诊断读取。
    let late = Stdout::new(
        1,
        cmd_id,
        950,
        Phase::Completed,
        harness.now_unix_ms(),
        b"late".to_vec(),
    )
    .expect("Stdout 帧");
    harness.inject_stdout("esp32-1", &late, false, Some(CONTENT_TYPE_STDOUT));
    harness.settle();
    assert_eq!(
        harness.finished_count(cmd_id),
        finished_before,
        "晚到事件不得改写已返回的超时结果"
    );
    let diagnostics = harness.server().diagnostics(harness.now_mono_ms());
    assert_eq!(diagnostics.counters.late_results, 1);
    let recorded = diagnostics
        .late_results
        .first()
        .expect("晚到终态应进 late_results");
    assert_eq!(recorded.cmd_id, cmd_id);
    assert_eq!(recorded.worker_id, "esp32-1");
    assert_eq!(recorded.code, "completed");
    assert!(
        !harness
            .server()
            .last_error()
            .unwrap_or_default()
            .contains("未知 cmd_id"),
        "晚到终态不应写入 last_error：{:?}",
        harness.server().last_error()
    );
}

#[test]
fn progress_frames_are_ignored_but_phases_accumulate() {
    let mut harness = base_harness();
    harness.silence_worker("esp32-1", true);
    let cmd_id = harness
        .submit(CommandRequest::task("esp32-1", "sleep", b"60000".to_vec()).with_ddl(60_000));
    harness.settle();
    let now = harness.now_unix_ms();
    let progress = Stdout::new(1, cmd_id, 1000, Phase::Progress, now, Vec::new()).expect("帧");
    harness.inject_stdout("esp32-1", &progress, false, Some(CONTENT_TYPE_STDOUT));
    harness.settle();
    assert!(harness.finished(cmd_id).is_none(), "Progress 不是最终事件");

    let started = Stdout::new(1, cmd_id, 1001, Phase::Started, now, Vec::new()).expect("帧");
    harness.inject_stdout("esp32-1", &started, false, Some(CONTENT_TYPE_STDOUT));
    harness.settle();
    let completed =
        Stdout::new(1, cmd_id, 1002, Phase::Completed, now, b"ok".to_vec()).expect("帧");
    harness.inject_stdout("esp32-1", &completed, false, Some(CONTENT_TYPE_STDOUT));
    harness.settle();
    let result = harness.finished(cmd_id).expect("Completed 应结束命令");
    assert!(result.ok());
    assert_eq!(result.phases, vec![Phase::Started, Phase::Completed]);
    assert_eq!(result.payload, b"ok");
}

// ---- 多Worker隔离 ----

#[test]
fn two_workers_are_isolated() {
    let mut harness = two_worker_harness();
    let configured = harness.run_command(
        CommandRequest::tool("stm32-1", "gpio.config", "PA5 out level=1"),
        2_000,
    );
    assert!(configured.ok());
    let written = harness.run_command(
        CommandRequest::tool("stm32-1", "gpio.write", "PA5 1"),
        2_000,
    );
    assert!(written.ok());
    let stm32_info =
        harness.run_command(CommandRequest::tool("stm32-1", "gpio.info", "PA5"), 2_000);
    assert!(text(&stm32_info.payload).contains("level=1"));

    // 另一台Worker状态不受影响。
    let esp32_info =
        harness.run_command(CommandRequest::tool("esp32-1", "gpio.info", "IO4"), 2_000);
    assert!(text(&esp32_info.payload).contains("mode=unconfigured"));

    assert_eq!(harness.server().workers().len(), 2);
    let online: Vec<bool> = harness
        .server()
        .workers()
        .iter()
        .map(|info| info.online)
        .collect();
    assert_eq!(online, vec![true, true]);
    harness.assert_no_errors();
}

// ---- v1.2：管理命令 / 取消 / 自动对时 / Jump（B2/B6/B7）----

/// 解出服务端最近一次发给 Worker 的 Stdin 帧。
fn last_server_stdin(harness: &Harness, worker_id: &str) -> Stdin {
    harness
        .last_stdin_publish(worker_id)
        .expect("应有 stdin 发布")
        .decode_stdin()
        .expect("Stdin 帧可解码")
}

/// `submit_control` 绕过在途上限：长 Task 在途时管理命令仍可送达。
#[test]
fn submit_control_bypasses_inflight_limit() {
    let mut harness = base_harness();
    harness.silence_worker("esp32-1", true);
    let _long_task = harness
        .submit(CommandRequest::task("esp32-1", "sleep", b"60000".to_vec()).with_ddl(60_000));
    harness.settle();
    // 普通命令被在途上限挡住（默认每 Worker 1 条）。
    assert!(matches!(
        harness
            .try_submit(CommandRequest::tool("esp32-1", "gpio.info", "IO4"))
            .unwrap_err(),
        ControlError::WorkerBusy(_)
    ));

    let now = harness.now_unix_ms();
    let now_mono = harness.now_mono_ms();
    let cmd_id = harness
        .server_mut()
        .submit_control(
            "esp32-1",
            "sekai.set_time",
            now.to_be_bytes().to_vec(),
            now,
            now_mono,
        )
        .expect("管理命令必须绕过在途上限");
    harness.settle();
    let frame = last_server_stdin(&harness, "esp32-1");
    assert_eq!(frame.id, cmd_id);
    assert_eq!(frame.kind, Kind::ToolCall);
    let body = ToolBody::decode(&frame.payload).expect("ToolBody 可解码");
    assert_eq!(body.name, "sekai.set_time");
    assert_eq!(body.args, now.to_be_bytes().to_vec());
    // 管理命令同样登记进在途账本（诊断可见）。
    let diagnostics = harness.server().diagnostics(now_mono);
    assert!(
        diagnostics
            .inflight
            .iter()
            .any(|item| item.cmd_id == cmd_id),
        "管理命令应在 inflight 中可见"
    );
}

/// `cancel(cmd_id)` 内部映射到属主 Worker 并下发 `sekai.cancel`（8 字节大端）。
#[test]
fn cancel_maps_inflight_command_to_sekai_cancel() {
    let mut harness = base_harness();
    harness.silence_worker("esp32-1", true);
    let cmd_id = harness
        .submit(CommandRequest::task("esp32-1", "sleep", b"60000".to_vec()).with_ddl(60_000));
    harness.settle();
    let now = harness.now_unix_ms();
    let now_mono = harness.now_mono_ms();
    let cancel_id = harness
        .server_mut()
        .cancel(cmd_id, now, now_mono)
        .expect("在途命令应可取消");
    harness.settle();
    let frame = last_server_stdin(&harness, "esp32-1");
    assert_eq!(frame.id, cancel_id);
    assert_eq!(frame.kind, Kind::ToolCall);
    let body = ToolBody::decode(&frame.payload).expect("ToolBody 可解码");
    assert_eq!(body.name, "sekai.cancel");
    assert_eq!(body.args, cmd_id.to_be_bytes().to_vec());
    // 未知 cmd_id：稳定错误，不发帧。
    let publishes = stdin_publish_count(&harness, "esp32-1");
    assert!(matches!(
        harness.server_mut().cancel(9_999_999, now, now_mono),
        Err(ControlError::UnknownCommand(9_999_999))
    ));
    assert_eq!(stdin_publish_count(&harness, "esp32-1"), publishes);
}

/// B6：首个（重连后）Hello 时钟未校准（unix_ms==0 或偏差 >2s）时自动下发
/// `sekai.set_time`（同一 boot_id 只发一次），校准后 `clock_synced` 变真。
#[test]
fn hello_clock_skew_triggers_automatic_set_time_once() {
    let mut harness = base_harness();
    // 重启以取得独立 boot_id（自动校时按 (worker, boot) 去重）。
    harness.restart_worker("esp32-1", 2);
    let publishes = stdin_publish_count(&harness, "esp32-1");
    assert!(harness.server().clock_synced(), "重启 Hello 时钟正常");

    // 注入的 Hello 需通告 sekai.set_time（submit_control 会做能力校验；
    // 真实 Worker 的 Hello 一定带全部内置工具）。
    let mut skewed = hello("esp32-1", &["gpio.info", "sekai.set_time"], &["echo"]);
    skewed.unix_ms = 0;
    harness
        .inject_hello("esp32-1", &skewed, 2, 910, false)
        .expect("注入 Hello");
    harness.settle();
    assert!(!harness.server().clock_synced(), "未校准前不应报告已同步");
    assert_eq!(
        stdin_publish_count(&harness, "esp32-1"),
        publishes + 1,
        "应自动下发一条 sekai.set_time"
    );
    let now = harness.now_unix_ms();
    let frame = last_server_stdin(&harness, "esp32-1");
    let body = ToolBody::decode(&frame.payload).expect("ToolBody 可解码");
    assert_eq!(body.name, "sekai.set_time");
    assert_eq!(body.args, now.to_be_bytes().to_vec());

    // 同一 boot_id 再次收到偏差 Hello：不重复自动校时。
    harness
        .inject_hello("esp32-1", &skewed, 2, 911, false)
        .expect("注入 Hello");
    harness.settle();
    assert_eq!(
        stdin_publish_count(&harness, "esp32-1"),
        publishes + 1,
        "同一 boot_id 只自动校时一次"
    );

    // Worker 报回正确时钟：状态恢复为已校准。
    let mut synced = hello("esp32-1", &["gpio.info"], &["echo"]);
    synced.unix_ms = harness.now_unix_ms();
    harness
        .inject_hello("esp32-1", &synced, 2, 912, false)
        .expect("注入 Hello");
    harness.settle();
    assert!(harness.server().clock_synced(), "校准后应报告 clock_synced");
}

/// B7：`CommandRequest::jump` 编码 Stdin::Jump(JumpBody)，目标不做通告校验。
#[test]
fn jump_request_encodes_jump_body() {
    let mut harness = base_harness();
    let cmd_id = harness.submit(CommandRequest::jump("esp32-1", "peer-1"));
    harness.settle();
    let frame = last_server_stdin(&harness, "esp32-1");
    assert_eq!(frame.id, cmd_id);
    assert_eq!(frame.kind, Kind::Jump);
    let body = JumpBody::decode(&frame.payload).expect("JumpBody 可解码");
    assert_eq!(body.target, "peer-1");
}
