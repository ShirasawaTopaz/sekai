//! UART envelope: COBS(kind | session BE u64 | length BE u16 | payload | CRC-16/CCITT-FALSE).
//! A zero before and after each packet allows recovery after a truncated transmission.

use super::MAX_FRAME_LEN;

pub const HEARTBEAT_MS: u64 = 1_000;
pub const SESSION_TIMEOUT_MS: u64 = 3 * HEARTBEAT_MS;
const HEADER: usize = 11;
const RAW_MAX: usize = HEADER + MAX_FRAME_LEN + 2;
const ENCODED_MAX: usize = RAW_MAX + RAW_MAX / 254 + 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Kind {
    Open = 1,
    Close = 2,
    Ping = 3,
    Pong = 4,
    Data = 5,
    Ready = 6,
}

#[derive(Debug, PartialEq, Eq)]
pub struct Packet<'a> {
    pub kind: Kind,
    pub session: u64,
    pub payload: &'a [u8],
}

pub fn write<E>(packet: Packet<'_>, mut send: impl FnMut(&[u8]) -> Result<(), E>) -> Result<(), E> {
    assert!(packet.session != 0 && packet.payload.len() <= MAX_FRAME_LEN);
    assert_eq!(packet.kind == Kind::Data, !packet.payload.is_empty());

    let mut header = [0; HEADER];
    header[0] = packet.kind as u8;
    header[1..9].copy_from_slice(&packet.session.to_be_bytes());
    header[9..].copy_from_slice(&(packet.payload.len() as u16).to_be_bytes());
    let crc = crc16(header.iter().chain(packet.payload).copied()).to_be_bytes();
    let mut block = [0; 255];
    let mut used = 1;
    send(&[0])?;
    for byte in header
        .into_iter()
        .chain(packet.payload.iter().copied())
        .chain(crc)
    {
        if byte != 0 {
            block[used] = byte;
            used += 1;
        }
        if byte == 0 || used == block.len() {
            block[0] = used as u8;
            send(&block[..used])?;
            used = 1;
        }
    }
    block[0] = used as u8;
    send(&block[..used])?;
    send(&[0])
}

pub struct Decoder {
    buffer: [u8; ENCODED_MAX],
    used: usize,
    discard: bool,
}

impl Decoder {
    pub const fn new() -> Self {
        Self {
            buffer: [0; ENCODED_MAX],
            used: 0,
            discard: false,
        }
    }

    /// Drop a partial packet after UART overrun or framing error.
    pub fn discard_until_delimiter(&mut self) {
        self.discard = true;
        self.used = 0;
    }

    pub fn push(&mut self, byte: u8) -> Option<Packet<'_>> {
        if byte != 0 {
            if !self.discard {
                if self.used == self.buffer.len() {
                    self.discard_until_delimiter();
                } else {
                    self.buffer[self.used] = byte;
                    self.used += 1;
                }
            }
            return None;
        }
        self.discard = false;
        let used = core::mem::take(&mut self.used);
        let mut read = 0;
        let mut written = 0;
        while read < used {
            let code = usize::from(self.buffer[read]);
            read += 1;
            let end = read + code - 1;
            if end > used {
                return None;
            }
            self.buffer.copy_within(read..end, written);
            written += code - 1;
            read = end;
            if code < 255 && read < used {
                self.buffer[written] = 0;
                written += 1;
            }
        }
        let raw = &self.buffer[..written];
        if raw.len() < HEADER + 2 {
            return None;
        }
        let kind = match raw[0] {
            1 => Kind::Open,
            2 => Kind::Close,
            3 => Kind::Ping,
            4 => Kind::Pong,
            5 => Kind::Data,
            6 => Kind::Ready,
            _ => return None,
        };
        let session = u64::from_be_bytes(raw[1..9].try_into().ok()?);
        let len = usize::from(u16::from_be_bytes(raw[9..11].try_into().ok()?));
        if session == 0
            || len > MAX_FRAME_LEN
            || (kind == Kind::Data) != (len != 0)
            || raw.len() != HEADER + len + 2
        {
            return None;
        }
        let (checked, checksum) = raw.split_at(HEADER + len);
        if crc16(checked.iter().copied()).to_be_bytes() != checksum {
            return None;
        }
        Some(Packet {
            kind,
            session,
            payload: &checked[HEADER..],
        })
    }
}

impl Default for Decoder {
    fn default() -> Self {
        Self::new()
    }
}

fn crc16(bytes: impl Iterator<Item = u8>) -> u16 {
    let mut crc = 0xffff;
    for byte in bytes {
        crc ^= u16::from(byte) << 8;
        for _ in 0..8 {
            crc = (crc << 1) ^ if crc & 0x8000 != 0 { 0x1021 } else { 0 };
        }
    }
    crc
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::{vec, vec::Vec};

    #[test]
    fn fragmented_full_length_and_resync_after_noise_bad_crc_and_truncation() {
        assert_eq!(crc16(b"123456789".iter().copied()), 0x29b1);
        let mut payload = vec![0x55; MAX_FRAME_LEN];
        payload[255] = 0;
        let mut wire = Vec::new();
        write(
            Packet {
                kind: Kind::Data,
                session: 7,
                payload: &payload,
            },
            |bytes| {
                wire.extend_from_slice(bytes);
                Ok::<_, ()>(())
            },
        )
        .unwrap();
        let mut corrupt = wire.clone();
        corrupt[100] ^= 1;
        let mut decoder = Decoder::new();
        for byte in [0xff, 0]
            .into_iter()
            .chain(core::iter::repeat_n(1, ENCODED_MAX + 1))
            .chain(corrupt)
            .chain(wire[..wire.len() - 5].iter().copied())
        {
            assert!(decoder.push(byte).is_none());
        }
        let mut count = 0;
        for chunk in wire.chunks(17) {
            for &byte in chunk {
                if let Some(packet) = decoder.push(byte) {
                    assert_eq!(packet.payload, payload);
                    assert_eq!(packet.session, 7);
                    count += 1;
                }
            }
        }
        assert_eq!(count, 1);
    }
}
