//! 客户端配置类型。

use alloc::string::String;
use alloc::vec::Vec;

use crate::mqtt::packet::Will;
use crate::mqtt::property::Properties;
use crate::mqtt::qos::QoS;

/// 客户端配置。
///
/// 所有字段都是纯配置数据，构造 [`Client`](super::Client) 后仍可通过
/// [`ClientConfig`] 的默认值语义表达「不携带该属性」。
/// 非法组合不会被构造时拒绝，而是在 [`Client::connect`](super::Client::connect) 时校验。
#[derive(Debug, Clone, Default)]
pub struct ClientConfig {
    /// 客户端标识符。Clean Start 为假时不能为空；若留空请在 CONNECT 中置 `clean_start`。
    pub client_id: String,
    /// 是否要求服务端丢弃旧会话并新建会话（Clean Start）。
    pub clean_start: bool,
    /// Keep Alive 间隔（秒），0 表示关闭 Keep Alive。
    pub keep_alive: u16,
    /// 等待 CONNACK 的超时时间（毫秒）。
    pub connect_timeout: u64,
    /// 会话过期时间（秒）；`None` 表示不携带该属性（协议默认值 0）。
    pub session_expiry_interval: Option<u32>,
    /// 客户端可同时接收的未确认 QoS>0 报文数上限；`None` 表示不携带该属性。
    pub receive_maximum: Option<u16>,
    /// 客户端可接收的最大报文长度；`None` 表示不携带该属性。
    pub maximum_packet_size: Option<u32>,
    /// 客户端可接收的主题别名数量上限；`None` 表示不携带该属性。
    pub topic_alias_maximum: Option<u16>,
    /// 是否请求服务端返回响应信息。
    pub request_response_information: Option<bool>,
    /// 是否请求服务端在失败时返回问题信息。
    pub request_problem_information: Option<bool>,
    /// CONNECT 报文中的用户属性。
    pub user_properties: Vec<(String, String)>,
    /// 遗嘱消息。
    pub will: Option<Will>,
    /// 用户名。
    pub username: Option<String>,
    /// 密码。
    pub password: Option<Vec<u8>>,
    /// 增强认证方法名。
    pub authentication_method: Option<String>,
    /// 增强认证初始数据。
    pub authentication_data: Option<Vec<u8>>,
    /// 是否启用出站主题别名；仍需服务端在 CONNACK 中声明 Topic Alias Maximum > 0 才生效。
    pub use_topic_alias: bool,
}

/// 单条发布（PUBLISH）的选项。
#[derive(Debug, Clone)]
pub struct PublishOptions {
    /// 服务质量等级。
    pub qos: QoS,
    /// 是否保留消息。
    pub retain: bool,
    /// PUBLISH 属性（主题别名由客户端内部管理，调用方不得在此设置）。
    pub properties: Properties,
}

impl Default for PublishOptions {
    fn default() -> Self {
        Self {
            qos: QoS::AtMostOnce,
            retain: false,
            properties: Properties::new(),
        }
    }
}
