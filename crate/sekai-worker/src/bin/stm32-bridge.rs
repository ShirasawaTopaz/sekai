use std::{env, net::TcpStream, process::ExitCode, thread, time::Duration};

use sekai_worker::uart_bridge::bridge;
use serialport::{ClearBuffer, DataBits, FlowControl, Parity, StopBits};

fn main() -> ExitCode {
    let args: Vec<_> = env::args().skip(1).collect();
    let usage = "Usage: stm32-bridge SERIAL_PORT [ROUTER_TCP_ADDR] (default 127.0.0.1:8765)";
    if args == ["--help"] {
        println!("{usage}");
        return ExitCode::SUCCESS;
    }
    let (port, address) = match args.as_slice() {
        [port] => (port.as_str(), "127.0.0.1:8765"),
        [port, address] => (port.as_str(), address.as_str()),
        _ => {
            eprintln!("{usage}");
            return ExitCode::FAILURE;
        }
    };

    loop {
        if let Err(error) = connect(port, address) {
            eprintln!("stm32-bridge: {port} -> {address}: {error}");
        }
        thread::sleep(Duration::from_millis(500));
    }
}

fn connect(port: &str, address: &str) -> Result<(), Box<dyn std::error::Error>> {
    let mut serial = serialport::new(port, 115_200)
        .data_bits(DataBits::Eight)
        .parity(Parity::None)
        .stop_bits(StopBits::One)
        .flow_control(FlowControl::None)
        .timeout(Duration::from_millis(50))
        .open()?;
    serial.clear(ClearBuffer::All)?;
    let tcp = TcpStream::connect(address)?;
    let id = loop {
        let mut bytes = [0; 8];
        getrandom::fill(&mut bytes)?;
        let id = u64::from_le_bytes(bytes);
        if id != 0 {
            break id;
        }
    };
    bridge(tcp, &mut serial, id)?;
    Ok(())
}
