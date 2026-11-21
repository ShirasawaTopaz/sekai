use std::{
    alloc::{GlobalAlloc, Layout, System},
    cell::Cell,
    convert::Infallible,
};

use sekai::{
    direct::uart::{Kind, Packet, SESSION_TIMEOUT_MS},
    sekai_clock::Clock,
    sekai_stdin::{Kind as CommandKind, Stdin, ToolBody},
    sekai_stdout::{Hello, Phase, Stdout},
    sekai_wire::MAX_PAYLOAD,
};
use sekai_gpio::{ChipFamily, virtual_gpio::VirtualGpio};
use sekai_stm32::{BLUE_PILL, HEAP_BYTES, Worker};

#[derive(Default)]
struct TestClock(Cell<u64>);

impl Clock for TestClock {
    fn monotonic_ms(&self) -> u64 {
        self.0.get()
    }
    fn unix_ms(&self) -> Option<u64> {
        None
    }
    fn calibrate_unix_ms(&self, _: u64) {}
}

#[derive(Clone, Copy)]
struct Usage {
    live: usize,
    peak: usize,
}

thread_local! {
    static USAGE: Cell<Option<Usage>> = const { Cell::new(None) };
}

struct CountingAllocator;

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

fn block_cost(layout: Layout) -> usize {
    // Include alignment and space for free-list metadata on the host.
    layout.size().next_multiple_of(layout.align().max(8)) + 2 * size_of::<usize>()
}

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let pointer = unsafe { System.alloc(layout) };
        if !pointer.is_null() {
            let _ = USAGE.try_with(|usage| {
                if let Some(mut current) = usage.get() {
                    current.live += block_cost(layout);
                    current.peak = current.peak.max(current.live);
                    usage.set(Some(current));
                }
            });
        }
        pointer
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        let _ = USAGE.try_with(|usage| {
            if let Some(mut current) = usage.get() {
                current.live = current.live.saturating_sub(block_cost(layout));
                usage.set(Some(current));
            }
        });
        unsafe { System.dealloc(pointer, layout) };
    }
}

type TestWorker<'a> = Worker<VirtualGpio, &'a TestClock>;

fn exchange(
    worker: &mut TestWorker<'_>,
    kind: Kind,
    session: u64,
    payload: &[u8],
) -> Vec<(Kind, Vec<u8>)> {
    let mut replies = Vec::new();
    worker
        .receive(
            Packet {
                kind,
                session,
                payload,
            },
            |reply| {
                assert_eq!(reply.session, session);
                replies.push((reply.kind, reply.payload.to_vec()));
                Ok::<_, Infallible>(())
            },
        )
        .unwrap();
    replies
}

fn command(id: u64, tool: &str, args: &[u8]) -> Vec<u8> {
    let body = ToolBody::new(tool, args.to_vec())
        .unwrap()
        .encode()
        .unwrap();
    Stdin::new(id, CommandKind::ToolCall, 0, 0, 0, body)
        .unwrap()
        .encode()
        .unwrap()
}

fn call(worker: &mut TestWorker<'_>, session: u64, id: u64, tool: &str, args: &[u8]) -> Stdout {
    let replies = exchange(worker, Kind::Data, session, &command(id, tool, args));
    assert_eq!(replies.last().unwrap().0, Kind::Ready);
    Stdout::decode(&replies[replies.len() - 2].1).unwrap()
}

#[test]
fn duplicates_preserve_session_and_old_packets_cannot_write_after_reconnect() {
    let clock = TestClock::default();
    let mut worker = Worker::new(
        VirtualGpio::new(ChipFamily::Stm32F1),
        &BLUE_PILL,
        "blue-test",
        &clock,
    )
    .unwrap();
    let replies = exchange(&mut worker, Kind::Open, 1, &[]);
    let hello = Stdout::decode(&replies[0].1).unwrap();
    assert_eq!(hello.boot_id, 1);
    assert_eq!(Hello::decode(&hello.payload).unwrap().tools.len(), 4);
    assert_eq!(
        call(&mut worker, 1, 1, "gpio.config", b"PB0 out level=1").phase,
        Phase::Completed
    );

    assert_eq!(
        exchange(&mut worker, Kind::Open, 1, &[]),
        [(Kind::Pong, Vec::new())]
    );
    let pin = call(&mut worker, 1, 2, "gpio.info", b"PB0");
    assert!(String::from_utf8_lossy(&pin.payload).contains("mode=output\nlevel=1"));

    clock.0.set(SESSION_TIMEOUT_MS);
    worker.poll().unwrap();
    assert!(exchange(&mut worker, Kind::Ping, 1, &[]).is_empty());
    let replies = exchange(&mut worker, Kind::Open, 2, &[]);
    let hello = Stdout::decode(&replies[0].1).unwrap();
    assert_eq!((hello.boot_id, hello.seq), (2, 1));

    assert!(exchange(&mut worker, Kind::Open, 1, &[]).is_empty());
    assert!(exchange(&mut worker, Kind::Close, 1, &[]).is_empty());
    let old = command(3, "gpio.config", b"PB0 out level=1");
    assert!(exchange(&mut worker, Kind::Data, 1, &old).is_empty());
    let pin = call(&mut worker, 2, 2, "gpio.info", b"PB0");
    assert!(String::from_utf8_lossy(&pin.payload).contains("mode=input"));
}

#[test]
fn full_length_input_and_continuous_calls_stay_within_the_firmware_heap() {
    let clock = TestClock::default();
    let tool = "gpio.config";
    let max = command(1, tool, &vec![b'X'; MAX_PAYLOAD - 2 - tool.len()]);
    USAGE.set(Some(Usage { live: 0, peak: 0 }));
    {
        let mut worker = Worker::new(
            VirtualGpio::new(ChipFamily::Stm32F1),
            &BLUE_PILL,
            "blue-test",
            &clock,
        )
        .unwrap();
        exchange(&mut worker, Kind::Open, 7, &[]);
        let replies = exchange(&mut worker, Kind::Data, 7, &max);
        assert_eq!(Stdout::decode(&replies[1].1).unwrap().phase, Phase::Failed);
        assert_eq!(replies[2].0, Kind::Ready);
        drop(replies);
        assert_eq!(
            call(&mut worker, 7, 2, "gpio.config", b"PB0 out level=0").phase,
            Phase::Completed
        );
        let max_args = "PB0 out ".to_string() + &"x ".repeat(245);
        assert_eq!(
            call(&mut worker, 7, 3, "gpio.config", max_args.as_bytes()).phase,
            Phase::Failed
        );
        drop(max_args);
        for id in 4..2_004 {
            let value = if id % 2 == 0 { b"PB0 0" } else { b"PB0 1" };
            assert_eq!(
                call(&mut worker, 7, id, "gpio.write", value).phase,
                Phase::Completed
            );
        }
    }
    let usage = USAGE.take().unwrap();
    assert_eq!(usage.live, 0);
    assert!(
        usage.peak <= HEAP_BYTES,
        "{} bytes peak ({} budget)",
        usage.peak,
        HEAP_BYTES
    );
    eprintln!("GPIO session host peak allocation: {} bytes", usage.peak);
}
