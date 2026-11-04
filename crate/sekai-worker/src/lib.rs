//! Sekai PC Worker 启动层（宿主进程）。
//!
//! 职责（对齐重构：原 server → Router，原嵌入式设备 → Worker；
//! 第一阶段以 PC Worker 为主，嵌入式 Worker 优先级置后，见 `crate/sekai-embedded`）：
//!
//! - 直连链路：与 Router 的通信**不经过 MQTT broker**，使用 stdio 或 TCP
//!   直连承载 V1 控制平面帧。链路帧格式为「u32 大端长度 + V1 Stdin/Stdout
//!   帧字节」，帧编解码全部复用 `sekai` 核心库（见 [`link`]）。
//! - 运行时：驱动 [`sekai::sekai_runtime::Runtime`] 执行 Task/ToolCall，
//!   适配层 [`host::WorkerHost`] 在进程内合成最小虚拟 broker，使 Runtime 的
//!   boot_id/seq/去重/Hello retained 语义在直连承载下原样生效。
//! - 能力：PC Worker 默认注册 **PC agent 能力**（`pc.*` 工具/任务，见
//!   [`capability`]）；virtual GPIO 改为 `--gpio` 显式 opt-in。
//! - LLM prompt 通道：`pc.ask` 经 [`llm_channel`] 的带外传输（TCP JSONL 到
//!   Router ingress，或 stdin/stdout JSONL 经 DSH 插件中转）取回回答；
//!   JSONL 编解码用本 crate 的零依赖子集 [`mini_json`]。
//!
//! 典型用法：
//!
//! - 子进程模式：Router 拉起 `sekai-worker --mode stdio`，经其 stdin/stdout 交换帧；
//! - 长驻模式：`sekai-worker --mode tcp --router-addr 127.0.0.1:7421`，
//!   断线按指数退避重连。

pub mod capability;
pub mod host;
pub mod link;
pub mod llm_channel;
pub mod mini_json;

pub use host::{HostError, WorkerHost};
pub use link::{
    Backoff, FrameDecoder, Link, LinkError, MemoryLink, StdioLink, TcpLink, connect_hint,
    memory_pair, memory_pair_with_timeout,
};
pub use llm_channel::{
    DshPromptChannel, LineIo, PromptError, PromptTransport, RouterPromptChannel, StdioLineIo,
    ask_timeout_ms,
};

/// crate 版本。
pub const VERSION: &str = "0.1.0";
