//! PINGREQ / PINGRESP 报文。
//!
//! 这两种报文没有可变头与载荷，Remaining Length 必须为 0；
//! 其编解码逻辑在 `super::Packet` 中直接处理，因此本文件只承载文档说明。
