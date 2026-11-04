//! 共享测试支持：手动时钟、内存 MQTT broker、真实 Runtime Worker节点与环回 harness。
//!
//! 用法（集成测试内）：
//!
//! ```ignore
//! mod support;
//!
//! use support::{Harness, DEFAULT_BASE_UNIX_MS, DEFAULT_TOPIC_ROOT};
//!
//! let mut harness = Harness::new(DEFAULT_TOPIC_ROOT, DEFAULT_BASE_UNIX_MS);
//! harness.add_worker("host-gpio", sekai_gpio::ChipFamily::Esp32, &[]);
//! harness.connect();
//! let result = harness.run_command(request, 2_000);
//! ```

// 共享支持模块被多个集成测试以不同子集引用；集中 re-export 的便利性优先，
// 允许未使用的 pub use（cargo clippy --all-targets -D warnings 时也保持干净）。
#![allow(unused_imports)]
#![allow(dead_code)]

pub mod broker;
pub mod clock;
pub mod harness;
pub mod worker;

pub use broker::{
    BROKER_MAXIMUM_PACKET_SIZE, BROKER_RECEIVE_MAXIMUM, BrokerError, ClientId, ConnackRecord,
    ConnectRecord, MemoryBroker, PublishRecord, RecordedPublish, SERVER_CLIENT, SubscribeRecord,
    topic_matches_filter,
};
pub use clock::ManualClock;
pub use harness::{
    DEFAULT_BASE_UNIX_MS, DEFAULT_TOPIC_ROOT, Harness, SERVER_CLIENT_ID, decode_stdout,
};
pub use worker::{DEFAULT_BOOT_ID, DEFAULT_FIRMWARE, WorkerNode, WorkerSpec};
