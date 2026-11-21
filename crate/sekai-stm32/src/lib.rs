//! UART session and board GPIO policy shared by the firmware and host simulations.
#![no_std]

extern crate alloc;

use alloc::{format, rc::Rc};
use core::{cell::RefCell, fmt};
use sekai::direct::ToolHost;
use sekai::direct::uart::{Kind, Packet, SESSION_TIMEOUT_MS};
use sekai::sekai_clock::Clock;
use sekai::sekai_tool::{RegistryError, ToolRegistry};
use sekai_gpio::config::{Direction, Level, PinConfig, Pull};
use sekai_gpio::family::{FamilyKind, FamilySpec, PinCaps, PinFlags, PinSpec};
use sekai_gpio::pin::{PinId, Port};
use sekai_gpio::port::{BackendKind, GpioError, GpioPort, PinDescription};
use sekai_gpio::tools::register_tools;

pub const LOOPBACK_OUT: PinId = PinId::port(Port::B, 0);
pub const LOOPBACK_IN: PinId = PinId::port(Port::B, 1);

const PINS: &[PinSpec] = &[
    PinSpec {
        id: LOOPBACK_OUT,
        slot: 16,
        caps: PinCaps::ALL,
        flags: PinFlags::NONE,
        note: "loopback output",
    },
    PinSpec {
        id: LOOPBACK_IN,
        slot: 17,
        caps: PinCaps::ALL,
        flags: PinFlags::NONE,
        note: "loopback input",
    },
];

pub static BLUE_PILL: FamilySpec = FamilySpec {
    family: FamilyKind::Stm32,
    chip: "stm32f103c8",
    pins: PINS,
    slots: 32,
    ports: &[Port::B],
};

pub static DISCOVERY: FamilySpec = FamilySpec {
    chip: "stm32f407vg",
    ..BLUE_PILL
};

const INPUT: PinConfig = PinConfig {
    direction: Direction::Input,
    level: Level::Low,
    pull: Pull::None,
    open_drain: false,
};

struct BoardGpio<P> {
    port: Rc<RefCell<P>>,
    spec: &'static FamilySpec,
}

impl<P> Clone for BoardGpio<P> {
    fn clone(&self) -> Self {
        Self {
            port: Rc::clone(&self.port),
            spec: self.spec,
        }
    }
}

impl<P> BoardGpio<P> {
    fn require_pin(&self, pin: PinId) -> Result<&'static PinSpec, GpioError> {
        self.spec
            .by_id(pin)
            .ok_or_else(|| GpioError::ReservedPin(format!("{pin} is unavailable on this board")))
    }
}

impl<P: GpioPort> GpioPort for BoardGpio<P> {
    fn spec(&self) -> &'static FamilySpec {
        self.spec
    }

    fn backend(&self) -> BackendKind {
        self.port.borrow().backend()
    }

    fn configure(&mut self, pin: PinId, config: PinConfig) -> Result<(), GpioError> {
        self.require_pin(pin)?;
        self.port.borrow_mut().configure(pin, config)
    }

    fn read(&mut self, pin: PinId) -> Result<Level, GpioError> {
        self.require_pin(pin)?;
        self.port.borrow_mut().read(pin)
    }

    fn write(&mut self, pin: PinId, level: Level) -> Result<(), GpioError> {
        self.require_pin(pin)?;
        self.port.borrow_mut().write(pin, level)
    }

    fn describe(&mut self, pin: PinId) -> Result<PinDescription, GpioError> {
        let spec = self.require_pin(pin)?;
        let mut description = self.port.borrow_mut().describe(pin)?;
        description.spec = spec;
        Ok(description)
    }

    fn configured_count(&self) -> usize {
        self.port.borrow().configured_count()
    }
}

#[derive(Debug)]
pub enum Error<E> {
    Gpio(GpioError),
    Registry(RegistryError),

    Transport(E),
}

impl<E: fmt::Debug> fmt::Display for Error<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}

pub struct Worker<P, C: Clock + Clone> {
    gpio: BoardGpio<P>,
    clock: C,
    worker_id: &'static str,
    active: Option<Session<C>>,
    retired: [u64; 8],
    next_retired: usize,
}

struct Session<C: Clock> {
    id: u64,
    last_ping: u64,
    host: ToolHost<C>,
}

impl<P: GpioPort + 'static, C: Clock + Clone> Worker<P, C> {
    pub fn new(
        port: P,
        spec: &'static FamilySpec,
        worker_id: &'static str,
        clock: C,
    ) -> Result<Self, GpioError> {
        let mut gpio = BoardGpio {
            port: Rc::new(RefCell::new(port)),
            spec,
        };
        gpio.configure(LOOPBACK_OUT, INPUT)?;
        gpio.configure(LOOPBACK_IN, INPUT)?;
        Ok(Self {
            gpio,
            clock,
            worker_id,
            active: None,
            retired: [0; 8],
            next_retired: 0,
        })
    }

    pub fn close(&mut self) -> Result<(), GpioError> {
        if let Some(session) = self.active.take() {
            self.retired[self.next_retired] = session.id;
            self.next_retired = (self.next_retired + 1) % self.retired.len();
        }
        self.gpio.configure(LOOPBACK_OUT, INPUT)?;
        self.gpio.configure(LOOPBACK_IN, INPUT)
    }

    pub fn receive<E>(
        &mut self,
        packet: Packet<'_>,
        mut send: impl FnMut(Packet<'_>) -> Result<(), E>,
    ) -> Result<(), Error<E>> {
        let result = self.handle(packet, &mut send);
        if result.is_err() {
            self.close().map_err(Error::Gpio)?;
        }
        result
    }

    fn handle<E>(
        &mut self,
        packet: Packet<'_>,
        send: &mut impl FnMut(Packet<'_>) -> Result<(), E>,
    ) -> Result<(), Error<E>> {
        let now = self.clock.monotonic_ms();
        self.expire(now).map_err(Error::Gpio)?;
        let id = packet.session;
        if packet.kind == Kind::Open
            && !self.retired.contains(&id)
            && self.active.as_ref().map(|s| s.id) != Some(id)
        {
            self.close().map_err(Error::Gpio)?;
            let mut tools = ToolRegistry::new();
            register_tools(&mut tools, self.gpio.clone()).map_err(Error::Registry)?;
            let mut host = ToolHost::new(self.clock.clone(), tools, id);
            let hello = host.hello(self.worker_id, env!("CARGO_PKG_VERSION"));
            send(Packet {
                kind: Kind::Data,
                session: id,
                payload: &hello,
            })
            .map_err(Error::Transport)?;
            self.active = Some(Session {
                id,
                last_ping: now,
                host,
            });
            return send(Packet {
                kind: Kind::Ready,
                session: id,
                payload: &[],
            })
            .map_err(Error::Transport);
        }
        let Some(session) = self.active.as_mut().filter(|active| active.id == id) else {
            return Ok(());
        };
        match packet.kind {
            Kind::Open | Kind::Ping => {
                session.last_ping = now;
                send(Packet {
                    kind: Kind::Pong,
                    session: id,
                    payload: &[],
                })
                .map_err(Error::Transport)
            }
            Kind::Close => self.close().map_err(Error::Gpio),
            Kind::Data => {
                session
                    .host
                    .on_frame(packet.payload, |payload| {
                        send(Packet {
                            kind: Kind::Data,
                            session: id,
                            payload,
                        })
                    })
                    .map_err(Error::Transport)?;
                send(Packet {
                    kind: Kind::Ready,
                    session: id,
                    payload: &[],
                })
                .map_err(Error::Transport)
            }
            Kind::Pong | Kind::Ready => Ok(()),
        }
    }

    pub fn poll(&mut self) -> Result<(), GpioError> {
        self.expire(self.clock.monotonic_ms())
    }

    fn expire(&mut self, now: u64) -> Result<(), GpioError> {
        if self
            .active
            .as_ref()
            .is_some_and(|session| now.saturating_sub(session.last_ping) >= SESSION_TIMEOUT_MS)
        {
            self.close()?;
        }
        Ok(())
    }
}

/// Heap reserved by both firmware images.
pub const HEAP_BYTES: usize = 10 * 1024;
