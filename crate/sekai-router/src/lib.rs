//! Sekai Router：AI 控制平面的常驻路由器。
//!
//! 本 crate 是 host-only 标准库程序：作为 Router 管理 Sekai Worker
//! （订阅Worker状态、下发 Tool/Task 命令），并把 DeepSeek
//! Chat Completions 的 function tools 映射到Worker能力上。
//! Worker 链路两种形态：嵌入式 Worker 经 MQTT 5 连 broker；
//! PC Worker 走 [`direct`] 直连（stdio/TCP + 长度前缀 V1 帧，不经 broker）。
//!
//! 现行架构见 `docs/sekai-architecture.md`，协议见 `docs/sekai-protocol.md`；
//! 设计沿革见 `docs/sekai-ai.md`（历史文档，原 sekai-agent CLI）。
//!
//! # 模块
//!
//! - [`config`]：环境变量/CLI 配置解析；
//! - [`topic`]：服务端主题构造与解析（支持命名空间主题根）；
//! - [`json`]：零依赖 JSON 解析/序列化（LLM 协议与 trace 使用）；
//! - [`http`]：零依赖 HTTP/1.1 客户端（明文直连 + HTTPS 外部 helper）；
//! - [`host_link`]：std TCP 链路、MQTT 字节流分帧与重连退避；
//! - [`direct`]：PC Worker 直连端点（虚拟 broker 适配器 + stdio/TCP 链路）；
//! - [`direct_gateway`]：把直连端点包装成线程化Worker网关（stdio/tcp listen）；
//! - [`worker`]：Worker注册表、能力快照与 `(boot_id, seq)` 去重；
//! - [`control`]：纯 MQTT 控制平面状态机（命令生命周期/超时/断链）；
//! - [`gateway`]：把控制平面包装成线程化Worker网关；
//! - [`session`]：REPL/TUI 共用的会话命令层（/link、/workers、目标执行）；
//! - [`link_builder`]：tcp/stdio/mqtt 网关装配共享层（session/serve/CLI 共用）；
//! - [`serve`]：`--serve` 无头 JSONL 服务模式（DSH 插件驱动）；
//! - [`repl`]：行式 REPL 前端；
//! - [`tui`]：全屏 TUI 前端（ratatui + crossterm）；
//! - [`llm`]：DeepSeek Chat Completions 客户端与类型；
//! - [`tools`]：LLM 工具 schema、参数解析与服务端策略；
//! - [`prompt`]：system prompt 构造（含注入防护）；
//! - [`agent`]：多步 tool-calling agent 循环；
//! - [`trace`]：脱敏 JSONL 轨迹。
#![forbid(unsafe_code)]

pub mod agent;
pub mod config;
pub mod control;
pub mod direct;
pub mod direct_gateway;
pub mod gateway;
pub mod host_link;
pub mod http;
pub mod json;
pub mod link_builder;
pub mod llm;
pub mod prompt;
pub mod repl;
pub mod serve;
pub mod session;
pub mod tools;
pub mod topic;
pub mod trace;
pub mod tui;
pub mod worker;
