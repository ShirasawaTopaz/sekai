//! 链路构造共享层：tcp / stdio / mqtt 三种网关的装配参数与构造函数。
//!
//! REPL `/link`（[`crate::session`]）、serve `link` 方法（[`crate::serve`]）
//! 与 CLI `--link` 批处理（bin）共用这一层，避免三处重复拼装
//! `ControlPlaneConfig` / `DirectGatewayConfig` / `MqttGatewayConfig`。
//! 构造函数返回具体网关类型（[`DirectGateway`] / [`MqttGateway`]），
//! 由调用方决定装箱为 `Box<dyn WorkerGateway>` 或再包 [`GatewayHandle`]。

use std::sync::{Arc, Mutex};

use sekai::sekai_device_list::Endpoint;

use crate::control::ControlPlaneConfig;
use crate::direct_gateway::{DirectGateway, DirectGatewayConfig, DirectMode};
use crate::gateway::{GatewayError, GatewayHandle, MqttGateway, MqttGatewayConfig, WorkerGateway};
use crate::topic::TopicRoot;

/// 链路构造参数（REPL `/link`、serve `link`、CLI `--link` 批处理共用）。
#[derive(Debug, Clone)]
pub struct LinkOptions {
    /// 主题根。
    pub topic_root: TopicRoot,
    /// 默认 DDL（毫秒；Worker 侧执行预算）。
    pub default_ddl_ms: u32,
    /// Tool 超时覆盖（绝对毫秒）。
    pub tool_timeout_ms: Option<u64>,
    /// Task 超时覆盖（绝对毫秒）。
    pub task_timeout_ms: Option<u64>,
    /// 端点预设 Worker id（首帧 Hello 前的兜底，仅影响主题包装）。
    pub worker_id_hint: String,
    /// MQTT 用户名。
    pub mqtt_username: Option<String>,
    /// MQTT 密码。
    pub mqtt_password: Option<String>,
    /// MQTT client id 覆盖；None 用网关默认（sekai-router-{pid}-{unix_ms}）。
    pub mqtt_client_id: Option<String>,
}

impl LinkOptions {
    /// 控制平面配置（client id 带进程号与 Unix 毫秒保证唯一）。
    fn plane_config(&self, tag: &str) -> ControlPlaneConfig {
        let unix_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_millis() as u64)
            .unwrap_or(0);
        let mut plane = ControlPlaneConfig::new(
            format!("sekai-router-{tag}-{}-{unix_ms}", std::process::id()),
            self.topic_root.clone(),
        );
        // 每 Worker 允许 4 条在途命令：prompt 内层工具调用（`pc.ask` 之外的
        // 普通 Tool）可在长 Task 在途时复用同一 Worker（B5）；每条命令的
        // 超时/取消各自独立计数，互不顶替。
        plane.max_inflight_per_worker = 4;
        plane.default_ddl_ms = self.default_ddl_ms.max(1);
        plane
    }

    /// 直连网关配置（stdio / tcp listen 共用形态）。
    fn direct_config(&self, tag: &str) -> DirectGatewayConfig {
        DirectGatewayConfig {
            plane: self.plane_config(tag),
            worker_id_hint: self.worker_id_hint.clone(),
            default_ddl_ms: self.default_ddl_ms,
            tool_timeout_ms: self.tool_timeout_ms,
            task_timeout_ms: self.task_timeout_ms,
        }
    }
}

/// 监听 `listen_addr` 等 Worker 连入（支持端口 0 自动分配）。
///
/// 成功返回（网关, 实际监听地址）——实际地址取自监听器（端口 0 已展开、
/// 默认端口已补全），可直接提示对端连接。
pub fn listen_tcp(
    options: &LinkOptions,
    listen_addr: &str,
) -> Result<(DirectGateway, String), GatewayError> {
    let gateway = DirectGateway::listen_tcp(listen_addr, options.direct_config("tcp"))?;
    let actual = match gateway.mode() {
        DirectMode::TcpListen { addr } => addr.clone(),
        _ => listen_addr.to_string(),
    };
    Ok((gateway, actual))
}

/// 拉起 stdio Worker 子进程（自动前置 `--mode stdio`）。
///
/// `extra_args` 为追加到 Worker 的参数；成功返回（网关, 程序名）。
pub fn spawn_stdio(
    options: &LinkOptions,
    program: &str,
    extra_args: &[String],
) -> Result<(DirectGateway, String), GatewayError> {
    let mut args: Vec<String> = vec![String::from("--mode"), String::from("stdio")];
    args.extend(extra_args.iter().cloned());
    let gateway = DirectGateway::spawn_stdio(program, &args, options.direct_config("stdio"))?;
    Ok((gateway, program.to_string()))
}

/// 连接 MQTT broker；成功返回（网关, 展示端点 `host:port`）。
pub fn connect_mqtt(
    options: &LinkOptions,
    endpoint: &Endpoint,
) -> Result<(MqttGateway, String), GatewayError> {
    let mut config = MqttGatewayConfig::new(endpoint.clone(), options.topic_root.clone());
    config.username = options.mqtt_username.clone();
    config.password = options.mqtt_password.clone();
    config.default_ddl_ms = options.default_ddl_ms;
    config.tool_timeout_ms = options.tool_timeout_ms;
    config.task_timeout_ms = options.task_timeout_ms;
    config.client_id = options.mqtt_client_id.clone();
    let display = endpoint.to_string();
    Ok((MqttGateway::connect(config)?, display))
}

/// 装箱网关为共享句柄（REPL 会话与 serve dispatcher 共用同一把锁）。
pub fn handle_of(gateway: impl WorkerGateway + 'static) -> GatewayHandle {
    Arc::new(Mutex::new(Box::new(gateway) as Box<dyn WorkerGateway>))
}
