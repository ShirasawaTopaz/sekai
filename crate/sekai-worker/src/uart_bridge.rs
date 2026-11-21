//! Forward one Router TCP connection through a session-tagged UART stream.
use std::{
    io::{self, ErrorKind, Read, Write},
    net::{Shutdown, TcpStream},
    time::{Duration, Instant},
};

use sekai::{
    direct::{
        FRAME_PREFIX_LEN, MAX_FRAME_LEN,
        uart::{self, Decoder, HEARTBEAT_MS, Kind, Packet, SESSION_TIMEOUT_MS},
    },
    sekai_stdout::{Phase, Stdout},
};

use crate::link::{FrameDecoder, write_frame_stream};

pub const IO_TIMEOUT: Duration = Duration::from_millis(20);
const HEARTBEAT: Duration = Duration::from_millis(HEARTBEAT_MS);
const SESSION_TIMEOUT: Duration = Duration::from_millis(SESSION_TIMEOUT_MS);

/// The serial reader must return a timeout periodically when idle.
pub fn bridge(mut tcp: TcpStream, serial: &mut (impl Read + Write), id: u64) -> io::Result<()> {
    if id == 0 {
        return Err(io::Error::new(
            ErrorKind::InvalidInput,
            "UART session ID cannot be zero",
        ));
    }
    tcp.set_nodelay(true)?;
    tcp.set_read_timeout(Some(IO_TIMEOUT))?;
    tcp.set_write_timeout(Some(SESSION_TIMEOUT))?;
    let result = forward(&mut tcp, serial, id);
    let _ = tcp.shutdown(Shutdown::Both);
    let _ = send(serial, Kind::Close, id, &[]);
    result
}

fn forward(tcp: &mut TcpStream, serial: &mut (impl Read + Write), id: u64) -> io::Result<()> {
    send(serial, Kind::Open, id, &[])?;
    let mut uart_decoder = Decoder::new();
    uart_decoder.discard_until_delimiter();
    let mut tcp_decoder = FrameDecoder::new(MAX_FRAME_LEN);
    let mut scratch = [0; 512];
    let mut last_response = Instant::now();
    let mut last_ping = last_response;
    let mut waiting_for_ready = Some(last_response);
    let mut hello_seen = false;

    loop {
        let now = Instant::now();
        if now.duration_since(last_response) >= SESSION_TIMEOUT
            || waiting_for_ready.is_some_and(|at| now.duration_since(at) >= SESSION_TIMEOUT)
        {
            return Err(io::Error::new(
                ErrorKind::TimedOut,
                "STM32 UART session timed out",
            ));
        }
        if now.duration_since(last_ping) >= HEARTBEAT {
            send(serial, Kind::Ping, id, &[])?;
            last_ping = now;
        }

        let count = match serial.read(&mut scratch) {
            Ok(0) => return Err(ErrorKind::UnexpectedEof.into()),
            Ok(count) => count,
            Err(error) if idle(&error) => 0,
            Err(error) => return Err(error),
        };
        for &byte in &scratch[..count] {
            let Some(packet) = uart_decoder
                .push(byte)
                .filter(|packet| packet.session == id)
            else {
                continue;
            };
            match packet.kind {
                Kind::Pong => last_response = Instant::now(),
                Kind::Ready if hello_seen => waiting_for_ready = None,
                Kind::Data => {
                    if !hello_seen {
                        hello_seen = Stdout::decode(packet.payload)
                            .is_ok_and(|frame| frame.phase == Phase::Hello && frame.boot_id == id);
                        if !hello_seen {
                            continue;
                        }
                    }
                    write_frame_stream(tcp, packet.payload).map_err(io::Error::other)?;
                }
                _ => {}
            }
        }

        // At most one TCP frame is in flight on UART. If a frame or its Ready
        // is lost, drop the connection so a command can never be replayed here.
        if waiting_for_ready.is_none() {
            let mut frame = tcp_decoder.next_frame().map_err(io::Error::other)?;
            if frame.is_none() {
                let space =
                    (MAX_FRAME_LEN + FRAME_PREFIX_LEN - tcp_decoder.buffered()).min(scratch.len());
                match tcp.read(&mut scratch[..space]) {
                    Ok(0) => return Ok(()),
                    Ok(count) => tcp_decoder
                        .push(&scratch[..count])
                        .map_err(io::Error::other)?,
                    Err(error) if idle(&error) => {}
                    Err(error) => return Err(error),
                }
                frame = tcp_decoder.next_frame().map_err(io::Error::other)?;
            }
            if let Some(frame) = frame {
                send(serial, Kind::Data, id, &frame)?;
                waiting_for_ready = Some(Instant::now());
            }
        }
    }
}

fn send(serial: &mut impl Write, kind: Kind, session: u64, payload: &[u8]) -> io::Result<()> {
    uart::write(
        Packet {
            kind,
            session,
            payload,
        },
        |bytes| serial.write_all(bytes),
    )
}

fn idle(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        ErrorKind::TimedOut | ErrorKind::WouldBlock | ErrorKind::Interrupted
    )
}
