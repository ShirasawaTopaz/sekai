//! 配置：环境变量默认值 + CLI 参数覆盖。
//!
//! 安全约束：
//! - API key、MQTT 密码只从环境变量读取，或由 CLI 显式传入；Debug 输出脱敏；
//! - 不读取、不写回任何凭据文件；
//! - trace 路径只决定写到哪里，本身不包含凭据。
//!
//! 环境变量以计划为准，并兼容常见的简写：
//!
//! | 变量 | 简写 | 含义 |
//! | --- | --- | --- |
//! | DEEPSEEK_API_KEY | API_KEY | DeepSeek API key |
//! | DEEPSEEK_BASE_URL | BASE_URL | OpenAI 兼容 base URL |
//! | SEKAI_ROUTER_MODEL | - | 模型名 |
//! | SEKAI_MQTT_ADDR | - | broker host:port |
//! | SEKAI_MQTT_USERNAME / PASSWORD | - | MQTT 凭据 |
//! | SEKAI_TOPIC_ROOT | - | 主题根 |
//! | SEKAI_ROUTER_TRACE | - | JSONL trace 路径 |
//! | SEKAI_PROMPT_ADDR | - | prompt ingress 监听地址（HOST:PORT 或 off） |
//! | SEKAI_MAX_STEPS / SEKAI_DDL_MS / SEKAI_TIMEOUT_MS / SEKAI_WAIT_WORKER_MS / SEKAI_HTTP_TIMEOUT_MS | - | 调参 |

use std::collections::HashMap;
use std::fmt;
use std::path::PathBuf;

use sekai::sekai_device_list::Endpoint;
use sekai::sekai_wire::{MAX_DDL_MS, validate_worker_id};

use crate::topic::{DEFAULT_TOPIC_ROOT, TopicError, TopicRoot};

/// 默认模型。
pub const DEFAULT_MODEL: &str = "deepseek-flash";
/// 默认 base URL。
pub const DEFAULT_BASE_URL: &str = "https://api.deepseek.com";
/// 默认最大 agent 步数。
pub const DEFAULT_MAX_STEPS: u32 = 12;
/// 默认 DDL（毫秒）。
pub const DEFAULT_DDL_MS: u32 = 1_000;
/// 默认等待Worker上线时间（毫秒）。
pub const DEFAULT_WAIT_WORKER_MS: u64 = 5_000;
/// 默认 HTTP 超时（毫秒）。
pub const DEFAULT_HTTP_TIMEOUT_MS: u64 = 90_000;
/// 默认直连监听地址（tcp 链路；与 sekai-worker 文档示例端口一致）。
pub const DEFAULT_LISTEN_ADDR: &str = "127.0.0.1:7421";
/// 默认 Worker 子进程程序（stdio 链路）。
pub const DEFAULT_WORKER_BIN: &str = "sekai-worker";

/// 环境变量名：API key。
pub const ENV_API_KEY: &str = "DEEPSEEK_API_KEY";
/// 环境变量名：API key 简写。
pub const ENV_API_KEY_ALT: &str = "API_KEY";
/// 环境变量名：base URL。
pub const ENV_BASE_URL: &str = "DEEPSEEK_BASE_URL";
/// 环境变量名：base URL 简写。
pub const ENV_BASE_URL_ALT: &str = "BASE_URL";
/// 环境变量名：模型。
pub const ENV_MODEL: &str = "SEKAI_ROUTER_MODEL";
/// 环境变量名：链路类型（mqtt/stdio/tcp）。
pub const ENV_LINK: &str = "SEKAI_LINK";
/// 环境变量名：MQTT 地址。
pub const ENV_MQTT_ADDR: &str = "SEKAI_MQTT_ADDR";
/// 环境变量名：MQTT 用户名。
pub const ENV_MQTT_USERNAME: &str = "SEKAI_MQTT_USERNAME";
/// 环境变量名：MQTT 密码。
pub const ENV_MQTT_PASSWORD: &str = "SEKAI_MQTT_PASSWORD";
/// 环境变量名：Worker 子进程程序（stdio 链路）。
pub const ENV_WORKER_BIN: &str = "SEKAI_WORKER_BIN";
/// 环境变量名：直连监听地址（tcp 链路）。
pub const ENV_LISTEN_ADDR: &str = "SEKAI_LISTEN_ADDR";
/// 环境变量名：主题根。
pub const ENV_TOPIC_ROOT: &str = "SEKAI_TOPIC_ROOT";
/// 环境变量名：trace 路径。
pub const ENV_TRACE: &str = "SEKAI_ROUTER_TRACE";
/// 环境变量名：最大步数。
pub const ENV_MAX_STEPS: &str = "SEKAI_MAX_STEPS";
/// 环境变量名：DDL。
pub const ENV_DDL_MS: &str = "SEKAI_DDL_MS";
/// 环境变量名：命令超时覆盖。
pub const ENV_TIMEOUT_MS: &str = "SEKAI_TIMEOUT_MS";
/// 环境变量名：等待Worker时间。
pub const ENV_WAIT_WORKER_MS: &str = "SEKAI_WAIT_WORKER_MS";
/// 环境变量名：HTTP 超时。
pub const ENV_HTTP_TIMEOUT_MS: &str = "SEKAI_HTTP_TIMEOUT_MS";
/// 环境变量名：prompt ingress 监听地址（HOST:PORT 或 off）。
pub const ENV_PROMPT_ADDR: &str = "SEKAI_PROMPT_ADDR";
/// 默认 prompt ingress 监听地址（交互与 --serve 模式；批处理默认 off）。
pub const DEFAULT_PROMPT_ADDR: &str = "127.0.0.1:7422";

/// Worker 链路类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LinkMode {
    /// 经 MQTT broker（嵌入式 Worker 链路，需 SEKAI_MQTT_ADDR）。
    #[default]
    Mqtt,
    /// 直连：拉起 Worker 子进程（stdio 管道承载 V1 帧）。
    Stdio,
    /// 直连：监听端口等 Worker 连入（长度前缀 V1 帧）。
    Tcp,
}

impl LinkMode {
    /// 从环境变量/CLI 取值解析。
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "mqtt" => Some(Self::Mqtt),
            "stdio" => Some(Self::Stdio),
            "tcp" => Some(Self::Tcp),
            _ => None,
        }
    }

    /// 人读名称。
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Mqtt => "mqtt",
            Self::Stdio => "stdio",
            Self::Tcp => "tcp",
        }
    }
}

/// 配置错误。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigError {
    /// 缺少必需的环境变量。
    MissingEnv(&'static str),
    /// 环境变量取值非法。
    InvalidEnv {
        /// 变量名。
        name: &'static str,
        /// 原因。
        message: String,
    },
    /// CLI 参数非法。
    InvalidArg(String),
    /// 参数冲突。
    Conflict(String),
    /// 主题根非法。
    Topic(TopicError),
    /// 读取 goal 文件失败。
    Io(String),
    /// 非 dry-run 时必须提供 API key。
    ApiKeyRequired,
}

impl ConfigError {
    /// CLI 退出码：配置错误统一为 2。
    pub fn exit_code(&self) -> i32 {
        2
    }
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingEnv(name) => write!(f, "缺少必需环境变量 {name}"),
            Self::InvalidEnv { name, message } => {
                write!(f, "环境变量 {name} 非法：{message}")
            }
            Self::InvalidArg(message) => write!(f, "参数非法：{message}"),
            Self::Conflict(message) => write!(f, "参数冲突：{message}"),
            Self::Topic(error) => write!(f, "主题根非法：{error}"),
            Self::Io(message) => write!(f, "读取 goal 文件失败：{message}"),
            Self::ApiKeyRequired => write!(
                f,
                "缺少 API key：设置 {ENV_API_KEY}（或 {ENV_API_KEY_ALT}，--dry-run/交互会话/--serve 除外）"
            ),
        }
    }
}

impl std::error::Error for ConfigError {}

/// 解析结果。
#[derive(Debug)]
pub enum Parsed {
    /// 打印帮助后退出。
    Help,
    /// 打印版本后退出。
    Version,
    /// 运行。
    Run(Box<Config>),
}

/// 运行配置。
#[derive(Clone)]
pub struct Config {
    /// 自然语言目标（dry-run/交互模式可空）。
    pub goal: Option<String>,
    /// DeepSeek API key（dry-run/交互模式可空）。
    pub api_key: Option<String>,
    /// OpenAI 兼容 base URL，不带 /chat/completions 后缀。
    pub base_url: String,
    /// 模型名。
    pub model: String,
    /// 链路类型（mqtt/stdio/tcp）。
    pub link: LinkMode,
    /// MQTT broker 端点（link=mqtt 时必需，批处理模式缺失即报错）。
    pub mqtt: Option<Endpoint>,
    /// MQTT 用户名。
    pub mqtt_username: Option<String>,
    /// MQTT 密码。
    pub mqtt_password: Option<String>,
    /// stdio 链路拉起的 Worker 程序。
    pub worker_bin: String,
    /// stdio 链路追加到 Worker 的参数（--worker-arg 可重复）。
    pub worker_args: Vec<String>,
    /// tcp 链路监听地址。
    pub listen_addr: String,
    /// 主题根。
    pub topic_root: TopicRoot,
    /// agent 最大步数。
    pub max_steps: u32,
    /// 默认 DDL。
    pub ddl_ms: u32,
    /// 命令超时覆盖（毫秒）。
    pub timeout_ms: Option<u64>,
    /// HTTP 超时。
    pub http_timeout_ms: u64,
    /// 等待Worker上线时间。
    pub wait_worker_ms: u64,
    /// Worker过滤器。
    pub worker_filter: Option<String>,
    /// 是否允许 GPIO 写。
    pub allow_write: bool,
    /// 是否允许 Task。
    pub allow_tasks: bool,
    /// 是否允许管理工具。
    pub allow_admin: bool,
    /// 是否输出 JSON。
    pub json: bool,
    /// 是否只列出Worker（不调用 LLM）。
    pub dry_run: bool,
    /// 交互模式（REPL）：无 goal 且非 dry-run 时自动启用。
    pub interactive: bool,
    /// 全屏 TUI 前端（ratatui）：蕴含 interactive；非 TTY 时回退行式 REPL。
    pub tui: bool,
    /// 无头 JSONL 服务模式：stdin 读请求、stdout 只写 JSONL（DSH 插件
    /// 驱动）；不调用 LLM，不需要 API key，与 --goal/--goal-file/--tui/
    /// -i/--dry-run 互斥。
    pub serve: bool,
    /// prompt ingress 监听地址（HOST:PORT）；None = 关闭。
    ///
    /// 默认：交互（REPL/TUI）与 --serve 模式取 [`DEFAULT_PROMPT_ADDR`]，
    /// 批处理（--goal）模式默认 off（进程短命，不常驻通道）；
    /// `--prompt-addr off` / `SEKAI_PROMPT_ADDR=off` 显式关闭。
    pub prompt_addr: Option<String>,
    /// trace 路径。
    pub trace_path: Option<PathBuf>,
}

impl fmt::Debug for Config {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Config")
            .field("goal", &self.goal)
            .field("api_key", &self.api_key.as_ref().map(|_| "<redacted>"))
            .field("base_url", &self.base_url)
            .field("model", &self.model)
            .field("link", &self.link.as_str())
            .field("mqtt", &self.mqtt)
            .field("mqtt_username", &self.mqtt_username)
            .field(
                "mqtt_password",
                &self.mqtt_password.as_ref().map(|_| "<redacted>"),
            )
            .field("worker_bin", &self.worker_bin)
            .field("worker_args", &self.worker_args)
            .field("listen_addr", &self.listen_addr)
            .field("topic_root", &self.topic_root)
            .field("max_steps", &self.max_steps)
            .field("ddl_ms", &self.ddl_ms)
            .field("timeout_ms", &self.timeout_ms)
            .field("http_timeout_ms", &self.http_timeout_ms)
            .field("wait_worker_ms", &self.wait_worker_ms)
            .field("worker_filter", &self.worker_filter)
            .field("allow_write", &self.allow_write)
            .field("allow_tasks", &self.allow_tasks)
            .field("allow_admin", &self.allow_admin)
            .field("json", &self.json)
            .field("dry_run", &self.dry_run)
            .field("interactive", &self.interactive)
            .field("tui", &self.tui)
            .field("serve", &self.serve)
            .field("prompt_addr", &self.prompt_addr)
            .field("trace_path", &self.trace_path)
            .finish()
    }
}

impl Config {
    /// 从当前进程环境构造配置（不含 goal）。
    pub fn from_env() -> Result<Self, ConfigError> {
        Self::from_env_map(&current_env())
    }

    /// 从给定环境构造配置（测试用）。
    ///
    /// `SEKAI_MQTT_ADDR` 不再是硬性要求：仅当链路为 mqtt 且非交互模式时
    /// 由 [`Config::apply_args`] 末尾校验（stdio/tcp 直连链路不需要 broker）。
    pub fn from_env_map(env: &HashMap<String, String>) -> Result<Self, ConfigError> {
        let mqtt = match env_string(env, &[ENV_MQTT_ADDR]) {
            Some(text) => {
                Some(
                    Endpoint::parse(&text).map_err(|error| ConfigError::InvalidEnv {
                        name: ENV_MQTT_ADDR,
                        message: error.to_string(),
                    })?,
                )
            }
            None => None,
        };
        let link = match env_string(env, &[ENV_LINK]) {
            Some(text) => LinkMode::parse(&text).ok_or(ConfigError::InvalidEnv {
                name: ENV_LINK,
                message: format!("必须是 mqtt/stdio/tcp：{text}"),
            })?,
            None => LinkMode::default(),
        };
        let worker_bin =
            env_string(env, &[ENV_WORKER_BIN]).unwrap_or_else(|| String::from(DEFAULT_WORKER_BIN));
        let listen_addr = env_string(env, &[ENV_LISTEN_ADDR])
            .unwrap_or_else(|| String::from(DEFAULT_LISTEN_ADDR));
        let topic_text =
            env_string(env, &[ENV_TOPIC_ROOT]).unwrap_or_else(|| String::from(DEFAULT_TOPIC_ROOT));
        let topic_root = TopicRoot::new(topic_text).map_err(ConfigError::Topic)?;
        let base_url = env_string(env, &[ENV_BASE_URL, ENV_BASE_URL_ALT])
            .unwrap_or_else(|| String::from(DEFAULT_BASE_URL));
        validate_base_url(&base_url)?;
        let model = env_string(env, &[ENV_MODEL]).unwrap_or_else(|| String::from(DEFAULT_MODEL));
        let api_key = env_string(env, &[ENV_API_KEY, ENV_API_KEY_ALT]);
        let max_steps = env_u64(env, ENV_MAX_STEPS)?.unwrap_or(u64::from(DEFAULT_MAX_STEPS));
        if !(1..=64).contains(&max_steps) {
            return Err(ConfigError::InvalidEnv {
                name: ENV_MAX_STEPS,
                message: String::from("必须在 1..=64"),
            });
        }
        let ddl_ms = env_u64(env, ENV_DDL_MS)?.unwrap_or(u64::from(DEFAULT_DDL_MS));
        if ddl_ms > u64::from(MAX_DDL_MS) {
            return Err(ConfigError::InvalidEnv {
                name: ENV_DDL_MS,
                message: format!("不能超过 {MAX_DDL_MS}"),
            });
        }
        let timeout_ms = match env_u64(env, ENV_TIMEOUT_MS)? {
            Some(0) => {
                return Err(ConfigError::InvalidEnv {
                    name: ENV_TIMEOUT_MS,
                    message: String::from("必须大于 0"),
                });
            }
            value => value,
        };
        let http_timeout_ms = env_u64(env, ENV_HTTP_TIMEOUT_MS)?.unwrap_or(DEFAULT_HTTP_TIMEOUT_MS);
        if !(1_000..=600_000).contains(&http_timeout_ms) {
            return Err(ConfigError::InvalidEnv {
                name: ENV_HTTP_TIMEOUT_MS,
                message: String::from("必须在 1000..=600000"),
            });
        }
        let wait_worker_ms = env_u64(env, ENV_WAIT_WORKER_MS)?.unwrap_or(DEFAULT_WAIT_WORKER_MS);
        Ok(Self {
            goal: None,
            api_key,
            base_url,
            model,
            link,
            mqtt,
            mqtt_username: env_string(env, &[ENV_MQTT_USERNAME]),
            mqtt_password: env_string(env, &[ENV_MQTT_PASSWORD]),
            worker_bin,
            worker_args: Vec::new(),
            listen_addr,
            topic_root,
            max_steps: max_steps as u32,
            ddl_ms: ddl_ms as u32,
            timeout_ms,
            http_timeout_ms,
            wait_worker_ms,
            worker_filter: None,
            allow_write: false,
            allow_tasks: false,
            allow_admin: false,
            json: false,
            dry_run: false,
            interactive: false,
            tui: false,
            serve: false,
            // 环境变量原始值（可为 off）；apply_args 里统一校验并按运行形态
            // 决定默认值。
            prompt_addr: env_string(env, &[ENV_PROMPT_ADDR]),
            trace_path: env_string(env, &[ENV_TRACE]).map(PathBuf::from),
        })
    }

    /// 应用 CLI 参数。
    pub fn apply_args(&mut self, args: &[String]) -> Result<(), ConfigError> {
        let mut goal: Option<String> = None;
        let mut goal_file: Option<PathBuf> = None;
        let mut index = 0;
        while index < args.len() {
            let arg = args[index].as_str();
            let mut take_value = |name: &str| -> Result<String, ConfigError> {
                index += 1;
                args.get(index)
                    .cloned()
                    .ok_or_else(|| ConfigError::InvalidArg(format!("{name} 缺少取值")))
            };
            match arg {
                "--goal" => {
                    if goal.is_some() {
                        return Err(ConfigError::Conflict(String::from("--goal 重复")));
                    }
                    goal = Some(take_value("--goal")?);
                }
                "--link" => {
                    let value = take_value("--link")?;
                    self.link = LinkMode::parse(&value).ok_or(ConfigError::InvalidArg(format!(
                        "--link 不支持 {value:?}（可选 mqtt/stdio/tcp）"
                    )))?;
                }
                "--worker-bin" => {
                    let value = take_value("--worker-bin")?;
                    if value.trim().is_empty() {
                        return Err(ConfigError::InvalidArg(String::from(
                            "--worker-bin 不能为空",
                        )));
                    }
                    self.worker_bin = value;
                }
                "--worker-arg" => {
                    self.worker_args.push(take_value("--worker-arg")?);
                }
                "--listen" => {
                    let value = take_value("--listen")?;
                    Endpoint::parse(&value).map_err(|error| {
                        ConfigError::InvalidArg(format!("--listen 非法：{error}"))
                    })?;
                    self.listen_addr = value;
                }
                "--goal-file" => {
                    if goal_file.is_some() {
                        return Err(ConfigError::Conflict(String::from("--goal-file 重复")));
                    }
                    goal_file = Some(PathBuf::from(take_value("--goal-file")?));
                }
                "--worker" => {
                    let value = take_value("--worker")?;
                    validate_worker_id(&value).map_err(|message| {
                        ConfigError::InvalidArg(format!("--worker 非法：{message}"))
                    })?;
                    self.worker_filter = Some(value);
                }
                "--model" => {
                    let value = take_value("--model")?;
                    if value.trim().is_empty() {
                        return Err(ConfigError::InvalidArg(String::from("--model 不能为空")));
                    }
                    self.model = value;
                }
                "--base-url" => {
                    let value = take_value("--base-url")?;
                    validate_base_url(&value)?;
                    self.base_url = value;
                }
                "--max-steps" => {
                    let value = parse_u64("--max-steps", &take_value("--max-steps")?)?;
                    if !(1..=64).contains(&value) {
                        return Err(ConfigError::InvalidArg(String::from(
                            "--max-steps 必须在 1..=64",
                        )));
                    }
                    self.max_steps = value as u32;
                }
                "--ddl-ms" => {
                    let value = parse_u64("--ddl-ms", &take_value("--ddl-ms")?)?;
                    if value > u64::from(MAX_DDL_MS) {
                        return Err(ConfigError::InvalidArg(format!(
                            "--ddl-ms 不能超过 {MAX_DDL_MS}"
                        )));
                    }
                    self.ddl_ms = value as u32;
                }
                "--timeout-ms" => {
                    let value = parse_u64("--timeout-ms", &take_value("--timeout-ms")?)?;
                    if value == 0 {
                        return Err(ConfigError::InvalidArg(String::from(
                            "--timeout-ms 必须大于 0",
                        )));
                    }
                    self.timeout_ms = Some(value);
                }
                "--wait-worker-ms" => {
                    let value = parse_u64("--wait-worker-ms", &take_value("--wait-worker-ms")?)?;
                    self.wait_worker_ms = value;
                }
                "--trace" => {
                    let value = take_value("--trace")?;
                    if value.trim().is_empty() {
                        return Err(ConfigError::InvalidArg(String::from("--trace 不能为空")));
                    }
                    self.trace_path = Some(PathBuf::from(value));
                }
                "--allow-write" => self.allow_write = true,
                "--allow-tasks" => self.allow_tasks = true,
                "--allow-admin" => self.allow_admin = true,
                "--json" => self.json = true,
                "--dry-run" => self.dry_run = true,
                "--serve" => self.serve = true,
                "--prompt-addr" => {
                    let value = take_value("--prompt-addr")?;
                    if value.trim().is_empty() {
                        return Err(ConfigError::InvalidArg(String::from(
                            "--prompt-addr 不能为空（HOST:PORT 或 off）",
                        )));
                    }
                    self.prompt_addr = Some(value);
                }
                "-i" | "--interactive" => self.interactive = true,
                "--tui" => {
                    // 全屏 TUI 蕴含交互会话；其余交互语义（goal 互斥等）复用
                    // interactive 的校验路径。
                    self.tui = true;
                    self.interactive = true;
                }
                "-h" | "--help" | "--version" => {
                    return Err(ConfigError::InvalidArg(format!(
                        "{arg} 应在解析配置之前处理"
                    )));
                }
                other => {
                    return Err(ConfigError::InvalidArg(format!("未知参数 {other}")));
                }
            }
            index += 1;
        }
        if goal.is_some() && goal_file.is_some() {
            return Err(ConfigError::Conflict(String::from(
                "--goal 与 --goal-file 二选一",
            )));
        }
        // --serve 与其余运行形态互斥（一次只允许一种前端）。
        if self.serve {
            let mut conflicts: Vec<&str> = Vec::new();
            if goal.is_some() {
                conflicts.push("--goal");
            }
            if goal_file.is_some() {
                conflicts.push("--goal-file");
            }
            if self.tui {
                conflicts.push("--tui");
            } else if self.interactive {
                conflicts.push("-i/--interactive");
            }
            if self.dry_run {
                conflicts.push("--dry-run");
            }
            if !conflicts.is_empty() {
                return Err(ConfigError::Conflict(format!(
                    "--serve 与 {} 互斥",
                    conflicts.join("/")
                )));
            }
        }
        if let Some(path) = goal_file {
            let text = std::fs::read_to_string(&path)
                .map_err(|error| ConfigError::Io(format!("{}：{error}", path.display())))?;
            let text = text.trim();
            if text.is_empty() {
                return Err(ConfigError::Io(format!("{} 内容为空", path.display())));
            }
            goal = Some(String::from(text));
        }
        if let Some(goal) = goal {
            if self.interactive {
                return Err(ConfigError::Conflict(String::from(
                    "--interactive 与 --goal/--goal-file 二选一（交互会话内直接输入目标）",
                )));
            }
            self.goal = Some(goal);
        } else if !self.dry_run && !self.serve {
            // 无 goal 且非 dry-run/serve：进入交互 REPL，而非报错。
            self.interactive = true;
        }
        // prompt ingress 地址：显式值（CLI 覆盖环境变量）校验后生效；
        // 未显式给出时按运行形态取默认（交互/--serve 开、批处理 off）。
        // 运行形态（serve/interactive）到此处已定。
        match self.prompt_addr.take().as_deref() {
            Some("off") | Some("OFF") => {
                self.prompt_addr = None;
            }
            Some(text) => {
                validate_prompt_addr(text)?;
                self.prompt_addr = Some(String::from(text));
            }
            None => {
                self.prompt_addr = if self.serve || self.interactive {
                    Some(String::from(DEFAULT_PROMPT_ADDR))
                } else {
                    None
                };
            }
        }
        if !self.dry_run
            && !self.interactive
            && !self.serve
            && self
                .api_key
                .as_deref()
                .unwrap_or_default()
                .trim()
                .is_empty()
        {
            return Err(ConfigError::ApiKeyRequired);
        }
        if self.link == LinkMode::Mqtt && self.mqtt.is_none() && !self.interactive && !self.serve {
            return Err(ConfigError::MissingEnv(ENV_MQTT_ADDR));
        }
        Ok(())
    }

    /// 命令行显示的模型名（--model 可能覆盖）。
    pub fn effective_model(&self) -> &str {
        &self.model
    }

    /// 目标文本；缺失时返回空串。
    pub fn goal_text(&self) -> &str {
        self.goal.as_deref().unwrap_or_default()
    }
}

/// 解析命令行；先处理 --help/--version，再读环境与参数。
pub fn parse(args: &[String]) -> Result<Parsed, ConfigError> {
    if args.iter().any(|arg| arg == "-h" || arg == "--help") {
        return Ok(Parsed::Help);
    }
    if args.iter().any(|arg| arg == "--version") {
        return Ok(Parsed::Version);
    }
    let mut config = Config::from_env()?;
    config.apply_args(args)?;
    Ok(Parsed::Run(Box::new(config)))
}

/// 版本字符串。
pub fn version_text() -> String {
    format!(
        "sekai-router {}",
        option_env!("CARGO_PKG_VERSION").unwrap_or("0.1.0")
    )
}

/// 用法帮助文本。
pub fn usage() -> String {
    format!(
        "sekai-router — Sekai Worker AI 控制端

用法：
  sekai-router --goal <TEXT> [选项]     批处理：执行一个目标后退出
  sekai-router [选项]                   交互会话（REPL）：无 --goal 时默认进入
  sekai-router --dry-run [选项]         只连接并列出Worker，不调用 LLM
  sekai-router --serve [选项]           无头 JSONL 服务模式：stdin 读请求、
                                        stdout 只写 JSONL（DSH 插件驱动；
                                        不调用 LLM，不需要 API key）
                                        --prompt-addr 另开 prompt ingress

必需环境变量：
  {ENV_API_KEY}        DeepSeek API key（--dry-run、交互会话与 --serve 除外；也接受 {ENV_API_KEY_ALT}）
  {ENV_MQTT_ADDR}      MQTT broker 地址，例如 127.0.0.1:1883（--link mqtt 批处理时必需）

可选环境变量：
  {ENV_BASE_URL}        OpenAI 兼容 base URL，默认 {DEFAULT_BASE_URL}
  {ENV_MODEL}           模型名，默认 {DEFAULT_MODEL}
  {ENV_LINK}            链路类型 mqtt/stdio/tcp，默认 mqtt
  {ENV_WORKER_BIN}      stdio 链路拉起的 Worker 程序，默认 {DEFAULT_WORKER_BIN}
  {ENV_LISTEN_ADDR}     tcp 链路监听地址，默认 {DEFAULT_LISTEN_ADDR}
  {ENV_MQTT_USERNAME}   MQTT 用户名
  {ENV_MQTT_PASSWORD}   MQTT 密码
  {ENV_TOPIC_ROOT}      主题根，默认 {DEFAULT_TOPIC_ROOT}
  {ENV_TRACE}           trace JSONL 路径
  {ENV_PROMPT_ADDR}     prompt ingress 监听地址（HOST:PORT 或 off），默认按运行形态
  {ENV_MAX_STEPS} / {ENV_DDL_MS} / {ENV_TIMEOUT_MS} / {ENV_WAIT_WORKER_MS} / {ENV_HTTP_TIMEOUT_MS}

选项：
  --goal <TEXT>         自然语言目标
  --goal-file <PATH>    从文件读取目标（UTF-8）
  --link <mqtt|stdio|tcp>
                        Worker 链路：mqtt=broker；stdio=拉起 sekai-worker 子进程；
                        tcp=监听端口等 sekai-worker connect 连入（默认 mqtt）
  --worker-bin <PATH>   stdio 链路的 Worker 程序（默认 {DEFAULT_WORKER_BIN}）
  --worker-arg <ARG>    追加到 Worker 的参数（可重复）
  --listen <HOST:PORT>  tcp 链路监听地址（默认 {DEFAULT_LISTEN_ADDR}）
  -i, --interactive     强制进入交互会话（与 --goal 互斥）
  --tui                 全屏 TUI 交互界面（蕴含 -i；非终端时回退行式 REPL）
  --worker <ID>         只允许操作指定Worker
  --model <NAME>        覆盖模型
  --base-url <URL>      覆盖 API base URL
  --max-steps <N>       agent 最大步数（1..=64，默认 {DEFAULT_MAX_STEPS}）
  --ddl-ms <N>          Worker端执行预算（默认 {DEFAULT_DDL_MS}）
  --timeout-ms <N>      控制端命令超时覆盖
  --wait-worker-ms <N>  等待Worker上线时间（默认 {DEFAULT_WAIT_WORKER_MS}）
  --allow-write         允许 gpio.config / gpio.write 等写操作
  --allow-tasks         允许下发Worker Task
  --allow-admin         允许 sekai.set_time / sekai.workers.set / sekai.cancel 等管理工具
  --prompt-addr <HOST:PORT|off>
                        prompt ingress 监听地址（Worker 经此把 prompt 交给
                        Router 调 LLM）；默认交互/--serve 模式
                        {DEFAULT_PROMPT_ADDR}、批处理模式 off；环境变量
                        {ENV_PROMPT_ADDR} 同义
  --json                stdout 输出单个 JSON 结果
  --dry-run             只连接链路并列出Worker，不调用 LLM
  --serve               无头 JSONL 服务模式（与 --goal/--goal-file/--tui/-i/--dry-run 互斥）
  --trace <PATH>        写脱敏 JSONL trace
  -h, --help            显示本帮助
  --version             显示版本

交互会话（/link tcp 后，另一终端运行 sekai-worker connect <地址> 即可接入）：
  /help /link /workers /wait /policy /exit；非 / 开头输入即自然语言目标。

退出码：0 成功；2 配置错误；3 Worker不可用；4 LLM/API 错误；5 达到最大步数；6 Worker命令最终失败。
"
    )
}

fn validate_base_url(value: &str) -> Result<(), ConfigError> {
    if value.trim().is_empty() {
        return Err(ConfigError::InvalidArg(String::from("base URL 不能为空")));
    }
    if value.starts_with("http://") || value.starts_with("https://") {
        Ok(())
    } else {
        Err(ConfigError::InvalidArg(format!(
            "base URL 必须以 http:// 或 https:// 开头：{value}"
        )))
    }
}

/// 校验 prompt ingress 地址：`SocketAddr`（含 `[IPv6]:port`）或
/// `主机名:端口`（端口 0 放行，运行时展开为实际端口）。
fn validate_prompt_addr(value: &str) -> Result<(), ConfigError> {
    if value.parse::<std::net::SocketAddr>().is_ok() {
        return Ok(());
    }
    match value.rsplit_once(':') {
        Some((host, port)) => {
            if !host.is_empty() && !host.contains(':') && port.parse::<u16>().is_ok() {
                Ok(())
            } else {
                Err(invalid_prompt_addr(value))
            }
        }
        None => Err(invalid_prompt_addr(value)),
    }
}

fn invalid_prompt_addr(value: &str) -> ConfigError {
    ConfigError::InvalidArg(format!(
        "--prompt-addr/SEKAI_PROMPT_ADDR 非法：{value}（应为 HOST:PORT 或 off）"
    ))
}

fn parse_u64(name: &str, value: &str) -> Result<u64, ConfigError> {
    value
        .parse::<u64>()
        .map_err(|_| ConfigError::InvalidArg(format!("{name} 不是合法整数：{value}")))
}

fn env_string(env: &HashMap<String, String>, names: &[&str]) -> Option<String> {
    for name in names {
        if let Some(value) = env.get(*name) {
            let value = value.trim();
            if !value.is_empty() {
                return Some(String::from(value));
            }
        }
    }
    None
}

fn env_u64(env: &HashMap<String, String>, name: &'static str) -> Result<Option<u64>, ConfigError> {
    match env.get(name) {
        Some(value) if !value.trim().is_empty() => {
            value
                .trim()
                .parse::<u64>()
                .map(Some)
                .map_err(|_| ConfigError::InvalidEnv {
                    name,
                    message: format!("不是合法整数：{value}"),
                })
        }
        _ => Ok(None),
    }
}

fn current_env() -> HashMap<String, String> {
    let mut map = HashMap::new();
    for (key, value) in std::env::vars() {
        map.insert(key, value);
    }
    map
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env() -> HashMap<String, String> {
        let mut map = HashMap::new();
        map.insert(String::from(ENV_MQTT_ADDR), String::from("127.0.0.1:1883"));
        map.insert(String::from(ENV_API_KEY), String::from("sk-test"));
        map
    }

    fn args(items: &[&str]) -> Vec<String> {
        items.iter().map(|item| String::from(*item)).collect()
    }

    #[test]
    fn defaults_are_applied() {
        let config = Config::from_env_map(&env()).unwrap();
        assert_eq!(config.model, DEFAULT_MODEL);
        assert_eq!(config.base_url, DEFAULT_BASE_URL);
        assert_eq!(config.max_steps, DEFAULT_MAX_STEPS);
        assert_eq!(config.ddl_ms, DEFAULT_DDL_MS);
        assert_eq!(config.topic_root.as_str(), DEFAULT_TOPIC_ROOT);
        assert_eq!(config.link, LinkMode::Mqtt);
        assert_eq!(config.mqtt.as_ref().expect("mqtt 端点").port, 1883);
        assert_eq!(config.worker_bin, DEFAULT_WORKER_BIN);
        assert_eq!(config.listen_addr, DEFAULT_LISTEN_ADDR);
        assert!(!config.allow_write);
        assert!(!config.dry_run);
        assert!(!config.interactive);
    }

    #[test]
    fn missing_mqtt_is_an_error_only_for_mqtt_batch() {
        let mut map = env();
        map.remove(ENV_MQTT_ADDR);
        // mqtt 批处理（含 dry-run）：仍要求 SEKAI_MQTT_ADDR。
        let mut config = Config::from_env_map(&map).unwrap();
        assert_eq!(
            config.apply_args(&args(&["--goal", "x"])).unwrap_err(),
            ConfigError::MissingEnv(ENV_MQTT_ADDR)
        );
        let mut config = Config::from_env_map(&map).unwrap();
        assert_eq!(
            config.apply_args(&args(&["--dry-run"])).unwrap_err(),
            ConfigError::MissingEnv(ENV_MQTT_ADDR)
        );
        // 直连链路：不需要 broker 地址。
        let mut config = Config::from_env_map(&map).unwrap();
        config
            .apply_args(&args(&["--link", "tcp", "--dry-run"]))
            .unwrap();
        assert_eq!(config.link, LinkMode::Tcp);
        // 交互模式：也不强制。
        let mut config = Config::from_env_map(&map).unwrap();
        config.apply_args(&args(&[])).unwrap();
        assert!(config.interactive);
    }

    #[test]
    fn link_and_direct_flags_are_parsed() {
        let mut config = Config::from_env_map(&env()).unwrap();
        config
            .apply_args(&args(&[
                "--link",
                "stdio",
                "--worker-bin",
                "./sekai-worker",
                "--worker-arg",
                "--family=stm32f1",
                "--worker-arg",
                "--boot-id",
                "--listen",
                "0.0.0.0:9000",
                "--dry-run",
            ]))
            .unwrap();
        assert_eq!(config.link, LinkMode::Stdio);
        assert_eq!(config.worker_bin, "./sekai-worker");
        assert_eq!(
            config.worker_args,
            vec![String::from("--family=stm32f1"), String::from("--boot-id")]
        );
        assert_eq!(config.listen_addr, "0.0.0.0:9000");

        // 非法取值。
        let mut config = Config::from_env_map(&env()).unwrap();
        assert!(
            config
                .apply_args(&args(&["--link", "grpc", "--dry-run"]))
                .is_err()
        );
        let mut config = Config::from_env_map(&env()).unwrap();
        assert!(
            config
                .apply_args(&args(&["--listen", "bad addr", "--dry-run"]))
                .is_err()
        );
        let mut config = Config::from_env_map(&env()).unwrap();
        assert!(
            config
                .apply_args(&args(&["--worker-bin", " ", "--dry-run"]))
                .is_err()
        );
    }

    #[test]
    fn interactive_mode_semantics() {
        // 无 goal 且非 dry-run：自动进入交互模式。
        let mut config = Config::from_env_map(&env()).unwrap();
        config.apply_args(&args(&["--link", "tcp"])).unwrap();
        assert!(config.interactive);
        assert!(config.goal.is_none());
        // 显式 -i 同样进入，且可无 API key。
        let mut map = env();
        map.remove(ENV_API_KEY);
        let mut config = Config::from_env_map(&map).unwrap();
        config.apply_args(&args(&["--link", "tcp", "-i"])).unwrap();
        assert!(config.interactive);
        // -i 与 --goal 冲突。
        let mut config = Config::from_env_map(&env()).unwrap();
        assert!(config.apply_args(&args(&["-i", "--goal", "x"])).is_err());
    }

    #[test]
    fn tui_flag_implies_interactive() {
        // --tui 蕴含 interactive，且可无 API key（与 -i 同语义）。
        let mut map = env();
        map.remove(ENV_API_KEY);
        let mut config = Config::from_env_map(&map).unwrap();
        config
            .apply_args(&args(&["--link", "tcp", "--tui"]))
            .unwrap();
        assert!(config.tui);
        assert!(config.interactive);
        // 默认不启用。
        let mut config = Config::from_env_map(&env()).unwrap();
        config.apply_args(&args(&["--link", "tcp"])).unwrap();
        assert!(!config.tui);
        // --tui 与 --goal 冲突。
        let mut config = Config::from_env_map(&env()).unwrap();
        assert!(config.apply_args(&args(&["--tui", "--goal", "x"])).is_err());
    }

    #[test]
    fn serve_mode_semantics() {
        // --serve：无 goal 合法、不进交互、不需要 API key 与 MQTT 地址。
        let mut map = env();
        map.remove(ENV_API_KEY);
        map.remove(ENV_MQTT_ADDR);
        let mut config = Config::from_env_map(&map).unwrap();
        config.apply_args(&args(&["--serve"])).unwrap();
        assert!(config.serve);
        assert!(!config.interactive);
        assert!(config.goal.is_none());
        assert!(!config.dry_run);
        // 链路默认值照常可配（serve 的 link 请求会用到）。
        let mut config = Config::from_env_map(&map).unwrap();
        config
            .apply_args(&args(&[
                "--serve",
                "--listen",
                "127.0.0.1:7500",
                "--worker",
                "host-gpio",
            ]))
            .unwrap();
        assert_eq!(config.listen_addr, "127.0.0.1:7500");
        assert_eq!(config.worker_filter.as_deref(), Some("host-gpio"));
    }

    #[test]
    fn serve_conflicts_with_other_frontend_flags() {
        for bad in [
            vec!["--serve", "--goal", "x"],
            vec!["--serve", "--dry-run"],
            vec!["--serve", "-i"],
            vec!["--serve", "--tui"],
        ] {
            let mut config = Config::from_env_map(&env()).unwrap();
            let error = config.apply_args(&args(&bad)).unwrap_err();
            assert!(
                matches!(error, ConfigError::Conflict(ref text) if text.contains("--serve")),
                "{bad:?} 应报 --serve 冲突，得到 {error:?}"
            );
        }
        // --goal-file 同样冲突（冲突在文件读取前判定）。
        let mut config = Config::from_env_map(&env()).unwrap();
        assert!(
            config
                .apply_args(&args(&["--serve", "--goal-file", "no-such-file.txt"]))
                .is_err()
        );
    }

    #[test]
    fn prompt_addr_defaults_by_frontend_mode() {
        // 批处理（--goal）：默认 off（进程短命）。
        let mut config = Config::from_env_map(&env()).unwrap();
        config.apply_args(&args(&["--goal", "x"])).unwrap();
        assert_eq!(config.prompt_addr, None);
        // dry-run（批处理形态）：同样 off。
        let mut config = Config::from_env_map(&env()).unwrap();
        config.apply_args(&args(&["--dry-run"])).unwrap();
        assert_eq!(config.prompt_addr, None);
        // 交互（默认 / -i / --tui）：默认 127.0.0.1:7422。
        let mut config = Config::from_env_map(&env()).unwrap();
        config.apply_args(&args(&[])).unwrap();
        assert_eq!(
            config.prompt_addr.as_deref(),
            Some(DEFAULT_PROMPT_ADDR),
            "交互模式应默认开启 prompt 通道"
        );
        let mut config = Config::from_env_map(&env()).unwrap();
        config.apply_args(&args(&["-i"])).unwrap();
        assert_eq!(config.prompt_addr.as_deref(), Some(DEFAULT_PROMPT_ADDR));
        let mut config = Config::from_env_map(&env()).unwrap();
        config.apply_args(&args(&["--tui"])).unwrap();
        assert_eq!(config.prompt_addr.as_deref(), Some(DEFAULT_PROMPT_ADDR));
        // serve：同样默认开启。
        let mut config = Config::from_env_map(&env()).unwrap();
        config.apply_args(&args(&["--serve"])).unwrap();
        assert_eq!(config.prompt_addr.as_deref(), Some(DEFAULT_PROMPT_ADDR));
    }

    #[test]
    fn prompt_addr_cli_and_env_override() {
        // CLI 显式地址。
        let mut config = Config::from_env_map(&env()).unwrap();
        config
            .apply_args(&args(&["--prompt-addr", "127.0.0.1:7500"]))
            .unwrap();
        assert_eq!(config.prompt_addr.as_deref(), Some("127.0.0.1:7500"));
        // 批处理模式也可显式开启。
        let mut config = Config::from_env_map(&env()).unwrap();
        config
            .apply_args(&args(&["--goal", "x", "--prompt-addr", "0.0.0.0:7422"]))
            .unwrap();
        assert_eq!(config.prompt_addr.as_deref(), Some("0.0.0.0:7422"));
        // off 显式关闭（覆盖默认）。
        let mut config = Config::from_env_map(&env()).unwrap();
        config.apply_args(&args(&["--prompt-addr", "off"])).unwrap();
        assert_eq!(config.prompt_addr, None);
        // 环境变量：地址与 off。
        let mut map = env();
        map.insert(
            String::from(ENV_PROMPT_ADDR),
            String::from("127.0.0.1:7501"),
        );
        let mut config = Config::from_env_map(&map).unwrap();
        config.apply_args(&args(&[])).unwrap();
        assert_eq!(config.prompt_addr.as_deref(), Some("127.0.0.1:7501"));
        let mut map = env();
        map.insert(String::from(ENV_PROMPT_ADDR), String::from("off"));
        let mut config = Config::from_env_map(&map).unwrap();
        config.apply_args(&args(&[])).unwrap();
        assert_eq!(config.prompt_addr, None);
        // CLI 覆盖环境变量。
        let mut map = env();
        map.insert(
            String::from(ENV_PROMPT_ADDR),
            String::from("127.0.0.1:7501"),
        );
        let mut config = Config::from_env_map(&map).unwrap();
        config.apply_args(&args(&["--prompt-addr", "off"])).unwrap();
        assert_eq!(config.prompt_addr, None);
    }

    #[test]
    fn prompt_addr_invalid_values_are_rejected() {
        for bad in ["127.0.0.1", "host:99999", "abc", ":7422"] {
            let mut config = Config::from_env_map(&env()).unwrap();
            let error = config
                .apply_args(&args(&["--prompt-addr", bad]))
                .unwrap_err();
            assert!(
                matches!(error, ConfigError::InvalidArg(ref text) if text.contains(bad)),
                "{bad} 应报非法地址，得到 {error:?}"
            );
        }
        // 主机名:端口 与 [IPv6]:端口 放行；端口 0 放行（运行时展开）。
        for good in ["localhost:7422", "[::1]:7422", "127.0.0.1:0"] {
            let mut config = Config::from_env_map(&env()).unwrap();
            config.apply_args(&args(&["--prompt-addr", good])).unwrap();
            assert_eq!(config.prompt_addr.as_deref(), Some(good));
        }
    }

    #[test]
    fn cli_overrides_and_policy_flags() {
        let mut config = Config::from_env_map(&env()).unwrap();
        config
            .apply_args(&args(&[
                "--goal",
                "read IO4",
                "--worker",
                "host-gpio",
                "--model",
                "deepseek-v4-pro",
                "--max-steps",
                "3",
                "--ddl-ms",
                "2000",
                "--timeout-ms",
                "30000",
                "--allow-write",
                "--allow-tasks",
                "--allow-admin",
                "--json",
                "--trace",
                "trace.jsonl",
            ]))
            .unwrap();
        assert_eq!(config.goal.as_deref(), Some("read IO4"));
        assert_eq!(config.worker_filter.as_deref(), Some("host-gpio"));
        assert_eq!(config.model, "deepseek-v4-pro");
        assert_eq!(config.max_steps, 3);
        assert_eq!(config.ddl_ms, 2000);
        assert_eq!(config.timeout_ms, Some(30_000));
        assert!(config.allow_write && config.allow_tasks && config.allow_admin);
        assert!(config.json);
        assert_eq!(
            config.trace_path.as_deref(),
            Some(std::path::Path::new("trace.jsonl"))
        );
    }

    #[test]
    fn goal_required_unless_dry_run_or_interactive() {
        // dry-run：无 goal 合法。
        let mut config = Config::from_env_map(&env()).unwrap();
        config.apply_args(&args(&["--dry-run"])).unwrap();
        assert!(config.dry_run);
        assert!(config.goal.is_none());
        assert!(!config.interactive);
    }

    #[test]
    fn api_key_required_unless_dry_run() {
        let mut map = env();
        map.remove(ENV_API_KEY);
        let mut config = Config::from_env_map(&map).unwrap();
        assert_eq!(
            config.apply_args(&args(&["--goal", "hi"])).unwrap_err(),
            ConfigError::ApiKeyRequired
        );
        let mut config = Config::from_env_map(&map).unwrap();
        config.apply_args(&args(&["--dry-run"])).unwrap();
    }

    #[test]
    fn api_key_can_come_from_alt_var() {
        let mut map = env();
        map.remove(ENV_API_KEY);
        map.insert(String::from(ENV_API_KEY_ALT), String::from("sk-alt"));
        let config = Config::from_env_map(&map).unwrap();
        assert_eq!(config.api_key.as_deref(), Some("sk-alt"));
    }

    #[test]
    fn unknown_and_malformed_args_are_rejected() {
        let mut config = Config::from_env_map(&env()).unwrap();
        assert!(config.apply_args(&args(&["--goal"])).is_err());
        assert!(config.apply_args(&args(&["--nope"])).is_err());
        assert!(
            config
                .apply_args(&args(&["--goal", "x", "--max-steps", "0"]))
                .is_err()
        );
        assert!(
            config
                .apply_args(&args(&["--goal", "x", "--worker", "bad id"]))
                .is_err()
        );
        assert!(
            config
                .apply_args(&args(&["--goal", "x", "--ddl-ms", "400000"]))
                .is_err()
        );
        assert!(
            config
                .apply_args(&args(&["--goal", "x", "--base-url", "ftp://x"]))
                .is_err()
        );
    }

    #[test]
    fn goal_file_is_loaded() {
        let path = std::env::temp_dir().join(format!(
            "sekai-router-goal-{}-{}.txt",
            std::process::id(),
            1
        ));
        std::fs::write(&path, "  read IO4  \n").unwrap();
        let mut config = Config::from_env_map(&env()).unwrap();
        config
            .apply_args(&args(&["--goal-file", path.to_str().unwrap()]))
            .unwrap();
        assert_eq!(config.goal.as_deref(), Some("read IO4"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn debug_redacts_secrets() {
        let mut map = env();
        map.insert(String::from(ENV_MQTT_PASSWORD), String::from("mqtt-secret"));
        let config = Config::from_env_map(&map).unwrap();
        let text = format!("{config:?}");
        assert!(!text.contains("sk-test"));
        assert!(!text.contains("mqtt-secret"));
        assert!(text.contains("redacted"));
    }

    #[test]
    fn parse_help_and_version_before_env() {
        assert!(matches!(parse(&args(&["--help"])), Ok(Parsed::Help)));
        assert!(matches!(parse(&args(&["--version"])), Ok(Parsed::Version)));
    }
}
