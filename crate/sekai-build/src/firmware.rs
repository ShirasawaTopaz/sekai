//! ESP32 固件镜像。
//!
//! 该二进制刻意保持为空：`sekai-embedded` 是一个 `no_std` 库，
//! 它持有复位向量（`#[esp_hal::main] fn main()`）、panic handler
//! 和 app descriptor，因此这个 target 只需把它链接进来。
//!
//! 由 `cargo run -p sekai-build -- build` 构建；永远不在宿主上构建，
//! 因为它需要 `esp32` feature。

#![no_std]
#![no_main]

// 将启动层保留在链接结果中。
use sekai_embedded as _;
