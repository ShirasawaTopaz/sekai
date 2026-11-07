//! SHA-1 与 base64（RFC 4648）自备实现。
//!
//! 仅为 [`crate::ws`] 的 WebSocket 握手所需（`Sec-WebSocket-Accept` 校验与
//! `Sec-WebSocket-Key` 生成，见 RFC 6455 §4.2.2）；仓库不引入 `sha1`/`base64`
//! crate（见 docs/sekai-feishu.md §7.3 零依赖取舍）。SHA-1 已不满足密码学
//! 抗碰撞要求，本实现**只用于协议握手**，不得用于任何安全敏感用途。

/// SHA-1 摘要字节数。
pub const DIGEST_BYTES: usize = 20;

/// 单块字节数（512 位）。
const BLOCK_BYTES: usize = 64;

/// SHA-1（FIPS 180-1）：返回 20 字节摘要。
pub fn sha1(data: &[u8]) -> [u8; DIGEST_BYTES] {
    let mut state: [u32; 5] = [
        0x6745_2301,
        0xEFCD_AB89,
        0x98BA_DCFE,
        0x1032_5476,
        0xC3D2_E1F0,
    ];
    // 填充：0x80 + 0 填充到 ≡56 (mod 64) + 8 字节大端位长。
    let bit_len = u64::try_from(data.len()).unwrap_or(u64::MAX) * 8;
    let mut padded = Vec::with_capacity(data.len() + BLOCK_BYTES + 1);
    padded.extend_from_slice(data);
    padded.push(0x80);
    while padded.len() % BLOCK_BYTES != 56 {
        padded.push(0);
    }
    padded.extend_from_slice(&bit_len.to_be_bytes());
    for block in padded.as_chunks::<BLOCK_BYTES>().0 {
        compress(&mut state, block);
    }
    let mut digest = [0u8; DIGEST_BYTES];
    for (chunk, word) in digest.as_chunks_mut::<4>().0.iter_mut().zip(state) {
        chunk.copy_from_slice(&word.to_be_bytes());
    }
    digest
}

/// SHA-1 摘要的小写十六进制（16 进制串）。
pub fn sha1_hex(data: &[u8]) -> String {
    sha1(data)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// base64 标准字母表（RFC 4648 §4，含 `=` 填充）。
const BASE64_ALPHABET: &[u8; 64] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// base64 编码（RFC 4648 标准 alphabet，输出含 `=` 填充）。
pub fn base64_encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let word = (b0 << 16) | (b1 << 8) | b2;
        out.push(BASE64_ALPHABET[(word >> 18) as usize & 0x3F] as char);
        out.push(BASE64_ALPHABET[(word >> 12) as usize & 0x3F] as char);
        if chunk.len() > 1 {
            out.push(BASE64_ALPHABET[(word >> 6) as usize & 0x3F] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(BASE64_ALPHABET[word as usize & 0x3F] as char);
        } else {
            out.push('=');
        }
    }
    out
}

/// 单块压缩（80 轮）。
fn compress(state: &mut [u32; 5], block: &[u8]) {
    let mut w = [0u32; 80];
    for (index, word) in w.iter_mut().take(16).enumerate() {
        *word = u32::from_be_bytes([
            block[index * 4],
            block[index * 4 + 1],
            block[index * 4 + 2],
            block[index * 4 + 3],
        ]);
    }
    for index in 16..80 {
        w[index] = (w[index - 3] ^ w[index - 8] ^ w[index - 14] ^ w[index - 16]).rotate_left(1);
    }
    let [mut a, mut b, mut c, mut d, mut e] = *state;
    for (index, word) in w.iter().enumerate() {
        let (f, k) = match index {
            0..=19 => ((b & c) | ((!b) & d), 0x5A82_7999u32),
            20..=39 => (b ^ c ^ d, 0x6ED9_EBA1),
            40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1B_BCDC),
            _ => (b ^ c ^ d, 0xCA62_C1D6),
        };
        let temp = a
            .rotate_left(5)
            .wrapping_add(f)
            .wrapping_add(e)
            .wrapping_add(k)
            .wrapping_add(*word);
        e = d;
        d = c;
        c = b.rotate_left(30);
        b = a;
        a = temp;
    }
    state[0] = state[0].wrapping_add(a);
    state[1] = state[1].wrapping_add(b);
    state[2] = state[2].wrapping_add(c);
    state[3] = state[3].wrapping_add(d);
    state[4] = state[4].wrapping_add(e);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// FIPS 180-1 / NIST SHAVS 官方向量。
    ///
    /// 取例刻意覆盖填充全部分支：短消息（同块内补长）、恰好 56 字节
    /// （长度字挤不进本块、追加一块）、百万 `a`（len%64==0 且多块）。
    #[test]
    fn sha1_fips_vectors() {
        assert_eq!(sha1_hex(b"abc"), "a9993e364706816aba3e25717850c26c9cd0d89d");
        assert_eq!(sha1_hex(b""), "da39a3ee5e6b4b0d3255bfef95601890afd80709");
        assert_eq!(
            sha1_hex(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
            "84983e441c3bd26ebaae4aa1f95129e5e54670f1"
        );
        let million_a = vec![b'a'; 1_000_000];
        assert_eq!(
            sha1_hex(&million_a),
            "34aa973cd4c4daa4f61eeb2bdbad27316534016f"
        );
    }

    #[test]
    fn sha1_bytes_match_hex() {
        let digest = sha1(b"abc");
        assert_eq!(digest.len(), DIGEST_BYTES);
        assert_eq!(hex(&digest), sha1_hex(b"abc"));
    }

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    #[test]
    fn base64_rfc4648_vectors() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
        // RFC 6455 §3.24 示例（握手 Accept 推导输入）。
        assert_eq!(base64_encode(b"Hello"), "SGVsbG8=");
    }
}
