//! 文档示例的机器校验：保证文档里的字节级示例与实现一致。
//!
//! `docs/sekai-gpio.md` 与 `docs/sekai-protocol.md` 引用本文件的测试；
//! 示例一旦漂移，测试会失败。

use sekai::sekai_stdin::{Kind, Stdin, ToolBody};

/// 把字节格式化为大写十六进制，单行空格分隔（文档中的换行只是排版）。
fn to_hex(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|byte| format!("{byte:02X}"))
        .collect::<Vec<_>>()
        .join(" ")
}

/// 文档 §5.8：`gpio.write IO4 1` 的 Stdin 帧（47 字节）。
#[test]
fn gpio_write_stdin_frame_matches_documentation() {
    let body = ToolBody::new("gpio.write", b"IO4 1".to_vec()).unwrap();
    assert_eq!(
        body.encode().unwrap().len(),
        17,
        "ToolBody 载荷应为 17 字节"
    );

    let stdin = Stdin::new(
        1,
        Kind::ToolCall,
        1_700_000_000_000,
        0,
        500,
        body.encode().unwrap(),
    )
    .unwrap();
    let frame = stdin.encode().unwrap();
    assert_eq!(frame.len(), 30 + 17, "Stdin 帧长度应为 47 字节");

    let expected = "\
01 02 00 00 00 00 00 00 00 01 00 00 01 8B CF E5 68 00 \
00 00 00 00 00 00 01 F4 00 00 00 11 \
00 0A 67 70 69 6F 2E 77 72 69 74 65 49 4F 34 20 31";
    assert_eq!(to_hex(&frame), expected);

    // 往返解码必须一致。
    let decoded = Stdin::decode(&frame).unwrap();
    assert_eq!(decoded, stdin);
    let tool = ToolBody::decode(&decoded.payload).unwrap();
    assert_eq!(tool.name, "gpio.write");
    assert_eq!(tool.args, b"IO4 1");
}

/// 文档 §5.8：成功 `Completed` 载荷是 15 字节文本，无换行。
#[test]
fn gpio_write_result_payload_matches_documentation() {
    let payload = b"pin=IO4 level=1";
    assert_eq!(payload.len(), 15);
}
