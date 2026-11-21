//! Shared Worker direct transport, with host-independent framing and runtime adapter.

mod adapter;
mod tools;
pub mod uart;
pub use tools::ToolHost;
mod wire;

pub use adapter::{HostError, WorkerHost};
pub use wire::{
    FRAME_PREFIX_LEN, KeepAlive, MAX_FRAME_LEN, MAX_STDIN_FRAME_LEN, decode_keepalive,
    encode_keepalive,
};
