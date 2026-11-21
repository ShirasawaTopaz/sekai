//! Real Router TCP link and bridge with a fragmented, in-memory UART.
use std::{
    collections::VecDeque,
    io::{self, Read, Write},
    net::TcpStream,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use sekai::direct::uart::{self, Decoder};
use sekai::sekai_wire::ErrorCode;
use sekai_gpio::{ChipFamily, virtual_gpio::VirtualGpio};
use sekai_router::{
    control::{CommandRequest, ControlPlaneConfig},
    direct_gateway::{DirectGateway, DirectGatewayConfig, DirectMode},
    gateway::WorkerGateway,
    topic::TopicRoot,
};
use sekai_stm32::{BLUE_PILL, Worker};
use sekai_worker::{
    capability::SystemClock,
    uart_bridge::{IO_TIMEOUT, bridge},
};

struct Serial {
    worker: Worker<VirtualGpio, SystemClock>,
    decoder: Decoder,
    rx: VecDeque<u8>,
    silent: Arc<AtomicBool>,
}

impl Write for Serial {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if !self.silent.load(Ordering::Relaxed) {
            for &byte in bytes {
                if let Some(packet) = self.decoder.push(byte) {
                    self.worker
                        .receive(packet, |reply| {
                            uart::write(reply, |block| {
                                self.rx.extend(block);
                                Ok::<_, io::Error>(())
                            })
                        })
                        .unwrap();
                }
            }
        }
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Read for Serial {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        self.worker.poll().unwrap();
        let count = bytes.len().min(self.rx.len()).min(11);
        if count == 0 {
            thread::sleep(IO_TIMEOUT);
            return Err(io::ErrorKind::TimedOut.into());
        }
        for (byte, received) in bytes.iter_mut().zip(self.rx.drain(..count)) {
            *byte = received;
        }
        Ok(count)
    }
}

fn wait_until(mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(8);
    while !condition() {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for Router state"
        );
        thread::sleep(IO_TIMEOUT);
    }
}

#[test]
fn router_roundtrip_and_reconnect_after_missing_heartbeats() {
    let worker_id = "stm32-uart-test";
    let plane = ControlPlaneConfig::new("uart-test", TopicRoot::new("sekai/v1").unwrap());
    let mut gateway = DirectGateway::listen_tcp(
        "127.0.0.1:0",
        DirectGatewayConfig {
            plane,
            worker_id_hint: worker_id.into(),
            default_ddl_ms: 500,
            tool_timeout_ms: Some(2500),
            task_timeout_ms: Some(2500),
        },
    )
    .unwrap();
    let DirectMode::TcpListen { addr } = gateway.mode() else {
        unreachable!()
    };
    let addr = addr.clone();
    let silent = Arc::new(AtomicBool::new(false));
    let worker_silent = Arc::clone(&silent);
    let reconnect = Arc::new(AtomicBool::new(false));
    let worker_reconnect = Arc::clone(&reconnect);
    let transport = thread::spawn(move || {
        let mut serial = Serial {
            worker: Worker::new(
                VirtualGpio::new(ChipFamily::Stm32F1),
                &BLUE_PILL,
                worker_id,
                SystemClock::new(),
            )
            .unwrap(),
            decoder: Decoder::new(),
            rx: VecDeque::new(),
            silent: worker_silent,
        };
        let connected = TcpStream::connect(&addr).unwrap();
        let error = bridge(connected, &mut serial, 41).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        wait_until(|| worker_reconnect.load(Ordering::Relaxed));
        serial.silent.store(false, Ordering::Relaxed);
        serial.rx.clear();
        bridge(TcpStream::connect(&addr).unwrap(), &mut serial, 42).unwrap();
    });

    for boot_id in [41, 42] {
        wait_until(|| {
            gateway
                .list_workers()
                .iter()
                .any(|worker| worker.online && worker.boot_id == boot_id)
        });
        let mut call = |tool, args: &str| {
            gateway
                .call(CommandRequest::tool(
                    worker_id,
                    tool,
                    args.as_bytes().to_vec(),
                ))
                .unwrap()
        };
        let info = call("gpio.info", "PB0");
        assert!(info.ok(), "{:?}", info.failure);
        assert!(String::from_utf8_lossy(&info.payload).contains("mode=input"));
        if boot_id == 41 {
            let failure = call("gpio.config", "PA9 out level=1").failure.unwrap();
            assert_eq!(failure.worker_code, Some(ErrorCode::InvalidArgument));
            assert!(call("gpio.config", "PB0 out level=0").ok());
            assert!(call("gpio.write", "PB0 1").ok());
            silent.store(true, Ordering::Relaxed);
            wait_until(|| !gateway.list_workers().iter().any(|worker| worker.online));
            reconnect.store(true, Ordering::Relaxed);
        }
    }
    gateway.shutdown();
    transport.join().unwrap();
}
