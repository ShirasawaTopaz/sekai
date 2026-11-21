use core::{
    alloc::{GlobalAlloc, Layout},
    cell::{Cell, RefCell, UnsafeCell},
    convert::Infallible,
    ptr::{self, NonNull},
};
use cortex_m::{
    interrupt::{self as irq, Mutex},
    peripheral::{NVIC, syst::SystClkSource},
};
use cortex_m_rt::{entry, exception};
use linked_list_allocator::Heap;
use pac::interrupt;
use sekai::direct::uart::{self, Decoder, Packet};
use sekai::sekai_clock::Clock;
use sekai_gpio::stm32::Stm32Gpio;
use sekai_stm32::Worker;
#[cfg(feature = "blue-pill")]
use stm32f1::stm32f103 as pac;
#[cfg(feature = "discovery")]
use stm32f4::stm32f407 as pac;

const BAUD: u32 = 115_200;
#[cfg(feature = "blue-pill")]
const CLOCK_HZ: u32 = 8_000_000;
#[cfg(feature = "discovery")]
const CLOCK_HZ: u32 = 16_000_000;
#[cfg(feature = "blue-pill")]
const DEFAULT_ID: &str = "stm32-blue-pill";
#[cfg(feature = "discovery")]
const DEFAULT_ID: &str = "stm32-discovery";
const WORKER_ID: &str = match option_env!("SEKAI_STM32_WORKER_ID") {
    Some(id) => id,
    None => DEFAULT_ID,
};

const HEAP_BYTES: usize = sekai_stm32::HEAP_BYTES;

#[repr(align(8))]
struct HeapStorage([u8; HEAP_BYTES]);

struct ForegroundHeap(UnsafeCell<Heap>);

// Only the foreground allocates. Neither SysTick nor the USART ISR uses alloc.
unsafe impl Sync for ForegroundHeap {}

unsafe impl GlobalAlloc for ForegroundHeap {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        unsafe { &mut *self.0.get() }
            .allocate_first_fit(layout)
            .map_or(ptr::null_mut(), NonNull::as_ptr)
    }

    unsafe fn dealloc(&self, address: *mut u8, layout: Layout) {
        unsafe { (&mut *self.0.get()).deallocate(NonNull::new_unchecked(address), layout) }
    }
}

#[global_allocator]
static ALLOCATOR: ForegroundHeap = ForegroundHeap(UnsafeCell::new(Heap::empty()));
static TICKS: Mutex<Cell<u64>> = Mutex::new(Cell::new(0));
static UNIX_CALIBRATION: Mutex<Cell<Option<(u64, u64)>>> = Mutex::new(Cell::new(None));

#[derive(Clone, Copy)]
struct SysTickClock;

impl Clock for SysTickClock {
    fn monotonic_ms(&self) -> u64 {
        irq::free(|cs| TICKS.borrow(cs).get())
    }

    fn unix_ms(&self) -> Option<u64> {
        irq::free(|cs| {
            let (unix, at) = UNIX_CALIBRATION.borrow(cs).get()?;
            Some(unix.saturating_add(TICKS.borrow(cs).get().saturating_sub(at)))
        })
    }

    fn calibrate_unix_ms(&self, unix_ms: u64) {
        irq::free(|cs| {
            UNIX_CALIBRATION
                .borrow(cs)
                .set(Some((unix_ms, TICKS.borrow(cs).get())))
        });
    }
}

#[exception]
fn SysTick() {
    irq::free(|cs| {
        let ticks = TICKS.borrow(cs);
        ticks.set(ticks.get() + 1);
    });
}

struct RxRing {
    bytes: [u8; 128],
    read: usize,
    write: usize,
    lost: bool,
}

static RX: Mutex<RefCell<RxRing>> = Mutex::new(RefCell::new(RxRing {
    bytes: [0; 128],
    read: 0,
    write: 0,
    lost: false,
}));

fn receive_byte() -> Option<Result<u8, ()>> {
    irq::free(|cs| {
        let mut rx = RX.borrow(cs).borrow_mut();
        if core::mem::take(&mut rx.lost) {
            rx.read = rx.write;
            return Some(Err(()));
        }
        if rx.read == rx.write {
            return None;
        }
        let byte = rx.bytes[rx.read];
        rx.read = (rx.read + 1) % rx.bytes.len();
        Some(Ok(byte))
    })
}

#[cfg(feature = "blue-pill")]
fn uart_regs() -> &'static pac::usart1::RegisterBlock {
    // USART1 is dedicated to this driver. Foreground writes TX, ISR reads RX.
    unsafe { &*pac::USART1::ptr() }
}

#[cfg(feature = "discovery")]
fn uart_regs() -> &'static pac::usart2::RegisterBlock {
    // USART2 is dedicated to this driver. Foreground writes TX, ISR reads RX.
    unsafe { &*pac::USART2::ptr() }
}

#[cfg(feature = "blue-pill")]
#[interrupt]
fn USART1() {
    uart_rx();
}

#[cfg(feature = "discovery")]
#[interrupt]
fn USART2() {
    uart_rx();
}

fn uart_rx() {
    let uart = uart_regs();
    let status = uart.sr().read().bits();
    // Reading SR followed by DR clears parity, framing, noise and overrun flags.
    const ERRORS: u16 = 0x0f;
    const RXNE: u16 = 1 << 5;
    if status & (ERRORS | RXNE) == 0 {
        return;
    }
    let byte = uart.dr().read().bits() as u8;
    irq::free(|cs| {
        let mut rx = RX.borrow(cs).borrow_mut();
        let write = rx.write;
        let next = (write + 1) % rx.bytes.len();
        if status & ERRORS != 0 || next == rx.read {
            rx.lost = true;
        } else {
            rx.bytes[write] = byte;
            rx.write = next;
        }
    });
}

fn transmit(packet: Packet<'_>) -> Result<(), Infallible> {
    let uart = uart_regs();
    uart::write(packet, |block| {
        for &byte in block {
            while uart.sr().read().txe().bit_is_clear() {}
            uart.dr().write(|w| unsafe { w.bits(u16::from(byte)) });
        }
        Ok(())
    })
}

#[entry]
fn main() -> ! {
    static mut STORAGE: HeapStorage = HeapStorage([0; HEAP_BYTES]);
    static mut DECODER: Decoder = Decoder::new();
    unsafe { (&mut *ALLOCATOR.0.get()).init(STORAGE.0.as_mut_ptr(), HEAP_BYTES) };
    DECODER.discard_until_delimiter();
    let mut core = cortex_m::Peripherals::take().unwrap();
    core.SYST.set_clock_source(SystClkSource::Core);
    core.SYST.set_reload(CLOCK_HZ / 1_000 - 1);
    core.SYST.clear_current();
    core.SYST.enable_interrupt();
    core.SYST.enable_counter();

    // The reset clock tree runs from HSI with AHB/APB prescalers at divide-by-one.
    let mut device = pac::Peripherals::take().unwrap();
    #[cfg(feature = "blue-pill")]
    let (gpio, spec, interrupt) = {
        use sekai_gpio::stm32::f1::{F1, F1Ports};
        device
            .RCC
            .apb2enr()
            .modify(|_, w| w.iopaen().set_bit().usart1en().set_bit());
        let _ = device.RCC.apb2enr().read();
        // PA9 alternate-function push-pull, 2 MHz. PA10 floating input.
        device
            .GPIOA
            .crh()
            .modify(|r, w| unsafe { w.bits((r.bits() & !0xff0) | 0x4a0) });
        let ports = F1Ports {
            gpioa: device.GPIOA,
            gpiob: device.GPIOB,
            gpioc: device.GPIOC,
        };
        (
            Stm32Gpio::<F1>::new(ports, &mut device.RCC),
            &sekai_stm32::BLUE_PILL,
            pac::Interrupt::USART1,
        )
    };

    #[cfg(feature = "discovery")]
    let (gpio, spec, interrupt) = {
        use sekai_gpio::stm32::f4::{F4, F4Ports};
        device.RCC.ahb1enr().modify(|_, w| w.gpioaen().set_bit());
        device.RCC.apb1enr().modify(|_, w| w.usart2en().set_bit());
        let _ = device.RCC.apb1enr().read();
        // PA2 and PA3 use AF7 for USART2.
        device
            .GPIOA
            .afrl()
            .modify(|r, w| unsafe { w.bits((r.bits() & !0xff00) | 0x7700) });
        device
            .GPIOA
            .moder()
            .modify(|r, w| unsafe { w.bits((r.bits() & !0xf0) | 0xa0) });
        let ports = F4Ports {
            gpioa: device.GPIOA,
            gpiob: device.GPIOB,
            gpioc: device.GPIOC,
            gpiod: device.GPIOD,
            gpioe: device.GPIOE,
        };
        (
            Stm32Gpio::<F4>::new(ports, &mut device.RCC),
            &sekai_stm32::DISCOVERY,
            pac::Interrupt::USART2,
        )
    };

    let mut worker = Worker::new(gpio, spec, WORKER_ID, SysTickClock).unwrap();
    let uart = uart_regs();
    uart.brr()
        .write(|w| unsafe { w.bits(((CLOCK_HZ + BAUD / 2) / BAUD) as u16) });
    // Word length, parity, stop bits and flow control keep their reset values (8N1).
    uart.cr1().write(|w| {
        w.ue()
            .set_bit()
            .te()
            .set_bit()
            .re()
            .set_bit()
            .rxneie()
            .set_bit()
    });
    unsafe { NVIC::unmask(interrupt) };

    let mut last_poll = 0;
    loop {
        while let Some(received) = receive_byte() {
            match received {
                Ok(byte) => {
                    if let Some(packet) = DECODER.push(byte) {
                        let _ = worker.receive(packet, transmit);
                    }
                }
                Err(()) => DECODER.discard_until_delimiter(),
            }
        }
        let now = SysTickClock.monotonic_ms();
        if now != last_poll {
            last_poll = now;
            let _ = worker.poll();
        }
        cortex_m::asm::wfi();
    }
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo<'_>) -> ! {
    cortex_m::peripheral::SCB::sys_reset()
}
