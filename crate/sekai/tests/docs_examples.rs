//! 校验 `docs/sekai-protocol.md` 中的十六进制示例与当前编码一致。

use sekai::sekai_device_list::Endpoint;
use sekai::sekai_stdin::{Kind, Stdin, TaskBody};
use sekai::sekai_stdout::{ErrorPayload, JumpOutcome, Phase, Stdout};
use sekai::sekai_wire::ErrorCode;

fn hex(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|byte| format!("{byte:02X}"))
        .collect::<Vec<_>>()
        .join(" ")
}

#[test]
fn stdin_task_example_matches_docs() {
    let body = TaskBody::new("echo", b"hello".to_vec()).unwrap();
    let stdin = Stdin::new(
        1,
        Kind::Task,
        1_700_000_000_000,
        0,
        500,
        body.encode().unwrap(),
    )
    .unwrap();
    assert_eq!(
        hex(&stdin.encode().unwrap()),
        "01 01 00 00 00 00 00 00 00 01 00 00 01 8B CF E5 68 00 \
         00 00 00 00 00 00 01 F4 00 00 00 0B \
         00 04 65 63 68 6F 68 65 6C 6C 6F"
    );
}

#[test]
fn stdout_events_match_docs() {
    let accepted = Stdout::new(1, 1, 2, Phase::Accepted, 1_700_000_000_000, Vec::new()).unwrap();
    assert_eq!(
        hex(&accepted.encode().unwrap()),
        "01 01 00 00 00 00 00 00 00 01 00 00 00 00 00 00 00 01 \
         00 00 00 02 00 00 01 8B CF E5 68 00 00 00 00 00"
    );

    let started = Stdout::new(1, 1, 3, Phase::Started, 1_700_000_000_000, Vec::new()).unwrap();
    assert_eq!(
        hex(&started.encode().unwrap()),
        "01 02 00 00 00 00 00 00 00 01 00 00 00 00 00 00 00 01 \
         00 00 00 03 00 00 01 8B CF E5 68 00 00 00 00 00"
    );

    let completed = Stdout::new(
        1,
        1,
        4,
        Phase::Completed,
        1_700_000_000_000,
        b"hello".to_vec(),
    )
    .unwrap();
    assert_eq!(
        hex(&completed.encode().unwrap()),
        "01 04 00 00 00 00 00 00 00 01 00 00 00 00 00 00 00 01 \
         00 00 00 04 00 00 01 8B CF E5 68 00 00 00 00 05 \
         68 65 6C 6C 6F"
    );
}

#[test]
fn payload_examples_match_docs() {
    let failed = ErrorPayload::new(ErrorCode::Timeout, "超时")
        .encode()
        .unwrap();
    assert_eq!(hex(&failed), "00 08 00 06 E8 B6 85 E6 97 B6");

    let outcome = JumpOutcome::new(Endpoint {
        host: String::from("peer-1"),
        port: 1883,
    });
    assert_eq!(
        hex(&outcome.encode().unwrap()),
        "00 06 70 65 65 72 2D 31 07 5B"
    );
}

#[test]
fn lwt_offline_matches_docs() {
    let offline = Stdout::new(1, 0, 0, Phase::Offline, 0, Vec::new()).unwrap();
    assert_eq!(
        hex(&offline.encode().unwrap()),
        "01 07 00 00 00 00 00 00 00 01 00 00 00 00 00 00 00 00 \
         00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00"
    );
}
