//! Single-command direct dispatcher for Workers with synchronous tools.
use alloc::{string::String, vec::Vec};

use super::{KeepAlive, decode_keepalive, encode_keepalive};
use crate::{
    sekai_clock::Clock,
    sekai_device_list::DeviceList,
    sekai_stdin::{Kind, Stdin, decode_named_ref},
    sekai_stdout::{ErrorPayload, Hello, Phase, Stdout},
    sekai_tool::ToolRegistry,
    sekai_wire::{CommandError, DEFAULT_DDL_MS, ErrorCode, MAX_PAYLOAD},
};

pub struct ToolHost<C> {
    clock: C,
    tools: ToolRegistry,
    boot_id: u64,
    seq: u32,
    recent: [u64; 8],
    next_recent: usize,
    last_result: Option<Vec<u8>>,
}

impl<C: Clock> ToolHost<C> {
    pub fn new(clock: C, tools: ToolRegistry, boot_id: u64) -> Self {
        Self {
            clock,
            tools,
            boot_id,
            seq: 1,
            recent: [0; 8],
            next_recent: 0,
            last_result: None,
        }
    }

    pub fn hello(&mut self, worker_id: &str, firmware: &str) -> Vec<u8> {
        let payload = Hello {
            worker_id: String::from(worker_id),
            firmware: String::from(firmware),
            uptime_ms: self.clock.monotonic_ms(),
            unix_ms: self.clock.unix_ms().unwrap_or_default(),
            tasks: Vec::new(),
            tools: self.tools.names().map(String::from).collect(),
            workers: DeviceList::new(),
            active_tasks: Vec::new(),
        }
        .encode()
        .expect("valid Worker identity and registered tools");
        self.event(0, Phase::Hello, payload)
    }

    /// Emit V1 events in order, retaining only the most recent final event for replay.
    pub fn on_frame<E>(
        &mut self,
        frame: &[u8],
        mut send: impl FnMut(&[u8]) -> Result<(), E>,
    ) -> Result<(), E> {
        match decode_keepalive(frame) {
            Some(KeepAlive::Pingreq) => return send(&encode_keepalive(KeepAlive::Pingresp)),
            Some(KeepAlive::Pingresp) => return Ok(()),
            None => {}
        }

        let (id, command) = match Stdin::decode(frame) {
            Ok(stdin) => (stdin.id, Ok(stdin)),
            Err(error) => {
                let Some(id) = error.cmd_id.filter(|id| *id != 0) else {
                    return Ok(());
                };
                (id, Err(ErrorCode::InvalidCommand))
            }
        };
        if self.recent[(self.next_recent + self.recent.len() - 1) % self.recent.len()] == id {
            if let Some(previous) = &self.last_result {
                send(previous)?;
            }
            return Ok(());
        }
        if self.recent.contains(&id) {
            return Ok(());
        }

        let result = match command
            .as_ref()
            .map_err(|code| CommandError::from(*code))
            .and_then(|stdin| self.prepare(stdin))
        {
            Ok((name, args, ddl)) => {
                send(&self.event(id, Phase::Accepted, Vec::new()))?;
                let now = self.clock.monotonic_ms();
                let output =
                    self.tools
                        .call(name, args, now, self.clock.unix_ms(), &DeviceList::new());
                if self.clock.monotonic_ms().saturating_sub(now) >= u64::from(ddl) {
                    Err(ErrorCode::Timeout.into())
                } else if output
                    .as_ref()
                    .is_ok_and(|payload| payload.len() > MAX_PAYLOAD)
                {
                    Err(ErrorCode::ResultTooLarge.into())
                } else {
                    output
                }
            }
            Err(error) => Err(error),
        };
        let (phase, payload) = match result {
            Ok(payload) => (Phase::Completed, payload),
            Err(error) => (
                Phase::Failed,
                ErrorPayload::new(error.code, error.message)
                    .encode()
                    .expect("bounded error payload"),
            ),
        };
        let reply = self.event(id, phase, payload);
        send(&reply)?;
        self.last_result = Some(reply);
        self.recent[self.next_recent] = id;
        self.next_recent = (self.next_recent + 1) % self.recent.len();
        Ok(())
    }

    fn prepare<'a>(&self, stdin: &'a Stdin) -> Result<(&'a str, &'a [u8], u32), CommandError> {
        if stdin.kind != Kind::ToolCall {
            return Err(ErrorCode::InvalidCommand.into());
        }
        if stdin.delay_ms != 0 {
            return Err(CommandError::new(
                ErrorCode::InvalidArgument,
                "direct GPIO tools execute immediately",
            ));
        }
        let (name, args) =
            decode_named_ref(&stdin.payload).map_err(|_| ErrorCode::InvalidArgument)?;
        if !self.tools.contains(name) {
            return Err(ErrorCode::UnknownTool.into());
        }
        let ddl = if stdin.ddl_ms == 0 {
            DEFAULT_DDL_MS
        } else {
            stdin.ddl_ms
        };
        Ok((name, args, ddl))
    }

    fn event(&mut self, id: u64, phase: Phase, payload: Vec<u8>) -> Vec<u8> {
        let seq = self.seq;
        self.seq = seq.checked_add(1).unwrap_or(1);
        Stdout::new(
            self.boot_id,
            id,
            seq,
            phase,
            self.clock.unix_ms().unwrap_or_default(),
            payload,
        )
        .expect("valid direct Worker event")
        .encode()
        .expect("bounded direct Worker event")
    }
}
