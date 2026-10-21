//! MQTT Variable Byte Integer（可变字节整数）编解码。
//!
//! Variable Byte Integer 使用 7 位一组、小端顺序的延续位编码，
//! 最多 4 个字节，取值范围 0..=268435455。

use alloc::vec::Vec;

use crate::mqtt::error::{DecodeError, EncodeError};

/// Variable Byte Integer 的最大值（4 字节编码可表示的最大值）。
pub const MAX: u32 = 268_435_455;

/// 编码 `value` 所需的字节数（1..=4）。
///
/// `value > MAX` 时同样返回 4：编码阶段会报错，但长度计算保持稳定。
pub fn encoded_len(value: u32) -> usize {
    if value < 128 {
        1
    } else if value < 16_384 {
        2
    } else if value < 2_097_152 {
        3
    } else {
        4
    }
}

/// 解码 Variable Byte Integer，返回 (值, 消耗的字节数)。
///
/// 必须是最小编码形式。错误语义：
/// - 缓冲区不足以完成解码：`Incomplete { needed: buf.len() + 1 }`；
/// - 第 5 个字节仍带延续位 / 4 字节仍不终止：`Malformed`；
/// - 非最小编码：`Malformed`。
pub fn decode(buf: &[u8]) -> Result<(u32, usize), DecodeError> {
    let mut value: u32 = 0;
    for (index, &byte) in buf.iter().take(4).enumerate() {
        // 读满 4 个字节后仍带延续位，说明编码超过 4 字节。
        if index == 3 && byte & 0x80 != 0 {
            return Err(DecodeError::Malformed("Variable Byte Integer 超过 4 字节"));
        }
        value |= u32::from(byte & 0x7F) << (7 * index);
        if byte & 0x80 == 0 {
            // 多字节编码的最后一个字节为 0 属于非最小编码。
            if index > 0 && byte == 0 {
                return Err(DecodeError::Malformed("Variable Byte Integer 非最小编码"));
            }
            return Ok((value, index + 1));
        }
    }
    Err(DecodeError::Incomplete {
        needed: buf.len() + 1,
    })
}

/// 以最小形式编码 Variable Byte Integer，追加写入 `out`。
///
/// `value > MAX` 时返回 `ValueOutOfRange`。
pub fn encode(value: u32, out: &mut Vec<u8>) -> Result<(), EncodeError> {
    if value > MAX {
        return Err(EncodeError::ValueOutOfRange {
            field: "Variable Byte Integer",
            value,
            max: MAX,
        });
    }
    out.try_reserve_exact(encoded_len(value))
        .map_err(|_| EncodeError::AllocationFailed)?;
    let mut remaining = value;
    loop {
        let mut byte = (remaining % 128) as u8;
        remaining /= 128;
        if remaining > 0 {
            byte |= 0x80;
        }
        out.push(byte);
        if remaining == 0 {
            break;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{MAX, decode, encode, encoded_len};
    use crate::mqtt::error::{DecodeError, EncodeError};
    use alloc::vec::Vec;

    #[test]
    fn encoded_len_boundaries() {
        let cases: [(u32, usize); 10] = [
            (0, 1),
            (127, 1),
            (128, 2),
            (16_383, 2),
            (16_384, 3),
            (2_097_151, 3),
            (2_097_152, 4),
            (MAX, 4),
            (MAX + 1, 4),
            (u32::MAX, 4),
        ];
        for (value, expected) in cases {
            assert_eq!(encoded_len(value), expected, "value = {value}");
        }
    }

    #[test]
    fn encode_and_decode_roundtrip_boundaries() {
        let cases: [(u32, &[u8]); 8] = [
            (0, &[0x00]),
            (127, &[0x7F]),
            (128, &[0x80, 0x01]),
            (16_383, &[0xFF, 0x7F]),
            (16_384, &[0x80, 0x80, 0x01]),
            (2_097_151, &[0xFF, 0xFF, 0x7F]),
            (2_097_152, &[0x80, 0x80, 0x80, 0x01]),
            (MAX, &[0xFF, 0xFF, 0xFF, 0x7F]),
        ];
        for (value, expected) in cases {
            let mut out = Vec::new();
            encode(value, &mut out).unwrap();
            assert_eq!(out.as_slice(), expected, "value = {value}");
            assert_eq!(decode(&out), Ok((value, out.len())));
            assert_eq!(encoded_len(value), out.len());
        }
    }

    #[test]
    fn decode_incomplete() {
        assert_eq!(decode(&[]), Err(DecodeError::Incomplete { needed: 1 }));
        assert_eq!(decode(&[0x80]), Err(DecodeError::Incomplete { needed: 2 }));
        assert_eq!(
            decode(&[0x80, 0x80]),
            Err(DecodeError::Incomplete { needed: 3 })
        );
        assert_eq!(
            decode(&[0x80, 0x80, 0x80]),
            Err(DecodeError::Incomplete { needed: 4 })
        );
    }

    #[test]
    fn decode_rejects_oversized_encoding() {
        assert_eq!(
            decode(&[0x80, 0x80, 0x80, 0x80]),
            Err(DecodeError::Malformed("Variable Byte Integer 超过 4 字节"))
        );
        assert_eq!(
            decode(&[0xFF, 0xFF, 0xFF, 0xFF, 0x00]),
            Err(DecodeError::Malformed("Variable Byte Integer 超过 4 字节"))
        );
    }

    #[test]
    fn decode_rejects_non_minimal_encoding() {
        for bytes in [
            &[0x80u8, 0x00][..],
            &[0x81, 0x00][..],
            &[0x80, 0x80, 0x00][..],
            &[0xFF, 0xFF, 0x80, 0x00][..],
        ] {
            assert_eq!(
                decode(bytes),
                Err(DecodeError::Malformed("Variable Byte Integer 非最小编码"))
            );
        }
    }

    #[test]
    fn encode_rejects_out_of_range() {
        let mut out = Vec::new();
        assert_eq!(
            encode(MAX + 1, &mut out),
            Err(EncodeError::ValueOutOfRange {
                field: "Variable Byte Integer",
                value: MAX + 1,
                max: MAX,
            })
        );
        assert!(out.is_empty());
        assert_eq!(
            encode(u32::MAX, &mut out),
            Err(EncodeError::ValueOutOfRange {
                field: "Variable Byte Integer",
                value: u32::MAX,
                max: MAX,
            })
        );
    }
}
