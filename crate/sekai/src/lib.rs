//! sekai 框架库。
//!
//! 本 crate 同时面向宿主测试与 ESP32 固件，因此库本体为 `no_std`，
//! 只依赖 `core` 与 `alloc`。测试构建通过 `#[cfg(test)] extern crate std`
//! 允许测试使用标准库提供的测试框架。
#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

#[cfg(test)]
extern crate std;

pub mod mqtt;
pub mod sekai_stdin;
pub mod sekai_stdout;

trait Looper {
    fn looper() -> !;
}
