//! Worker 侧宿主适配层：把 [`Runtime`]（MQTT 报文驱动）接到直连 [`Link`]（V1 帧）上。
//!
//! ## 原理
//!
//! [`sekai::sekai_runtime::Runtime`] 的 Stdin 处理、boot_id/seq、去重、DDL、
//! Hello retained 等全部语义都在 MQTT 客户端状态机内部。协议文档 §1.2 明确
//! 「MQTT 只是一种载体……换成其他具备同等语义的可靠消息通道时，帧格式不变」。
//! 直连链路就是这样的替代载体：本模块在 Worker 进程内充当一个**最小的虚拟
//! broker 适配器**，让 Runtime 的状态机原样运转，而链路上只出现 V1 帧：
//!
//! - 下行（链路帧 → Runtime）：收到的 V1 帧被包装成一条发给
//!   `{root}/dev/{worker_id}/in` 主题的 QoS1 PUBLISH 喂给 `Runtime::feed`，
//!   Runtime 完成 Stdin 解码、去重、时效/DDL 校验与调度；
//! - 上行（Runtime → 链路帧）：`Runtime::next_outgoing` 吐出的 PUBLISH
//!   （`out`/`state` 主题）载荷本身就是 V1 Stdout 帧，剥出后直接写链路；
//!   CONNECT/SUBSCRIBE/PINGREQ 由适配器就地回 CONNACK/SUBACK/PINGRESP，
//!   PUBACK 同样就地确认，因此这些 MQTT 报文**从不出现在链路上**。
//!
//! 这样 Hello 在每次链路建立后作为首帧发出（对应 retained state 语义），
//! seq 在 boot 内跨重连单调递增，出站事件在重连时按 `(boot_id, seq)` 重放，
//! 与 MQTT 承载时完全一致。

use sekai::mqtt::packet::{Ack, Connack, Packet, Publish, Suback};
use sekai::mqtt::property::Properties;
use sekai::mqtt::qos::QoS;
use sekai::mqtt::reason::ReasonCode;
use sekai::mqtt::topic::TopicName;
use sekai::sekai_clock::Clock;
use sekai::sekai_device_list::{DeviceList, Endpoint};
use sekai::sekai_runtime::{
    CONTENT_TYPE_STDIN, LinkAction, Runtime, RuntimeConfig, RuntimeError, RuntimeStatus,
};
use sekai::sekai_task::TaskRegistry;
use sekai::sekai_tool::ToolRegistry;

/// 宿主适配层错误。
#[derive(Debug)]
pub enum HostError {
    /// 运行时错误。
    Runtime(RuntimeError),
    /// 输入主题构造失败。
    Topic(&'static str),
}

impl core::fmt::Display for HostError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Runtime(error) => write!(f, "运行时错误：{error}"),
            Self::Topic(message) => write!(f, "主题错误：{message}"),
        }
    }
}

impl std::error::Error for HostError {}

impl From<RuntimeError> for HostError {
    fn from(error: RuntimeError) -> Self {
        Self::Runtime(error)
    }
}

/// 把 [`Runtime`] 接到直连链路上的宿主驱动。
///
/// 用法（与传输层解耦，链路由调用方持有）：
///
/// 1. 链路建立后 [`on_link_up`](WorkerHost::on_link_up)，把返回的帧写入链路
///    （通常为空；Hello 在随后的 poll 中产出）；
/// 2. 主循环里：链路上收到帧就 [`on_frame`](WorkerHost::on_frame)；
///    空闲 tick（读超时）就 [`on_poll`](WorkerHost::on_poll)；
///    两者返回的帧都写回链路；
/// 3. 链路断开调 [`on_link_lost`](WorkerHost::on_link_lost)（意外）或
///    [`on_link_closed`](WorkerHost::on_link_closed)（主动），重连后回到 1。
pub struct WorkerHost<C: Clock> {
    runtime: Runtime<C>,
    input_topic: TopicName,
    next_packet_id: u16,
    disconnect_requested: bool,
}

impl<C: Clock> WorkerHost<C> {
    /// 用与 `Runtime::new` 相同的入参构造宿主驱动。
    ///
    /// `config.topic_root` 与 `config.worker_id` 同时用于推导输入主题。
    pub fn new(
        config: RuntimeConfig,
        clock: C,
        workers: DeviceList,
        tasks: TaskRegistry,
        tools: ToolRegistry,
    ) -> Result<Self, HostError> {
        let input_topic =
            TopicName::new(format!("{}/dev/{}/in", config.topic_root, config.worker_id))
                .map_err(HostError::Topic)?;
        let runtime = Runtime::new(config, clock, workers, tasks, tools)?;
        Ok(Self {
            runtime,
            input_topic,
            next_packet_id: 1,
            disconnect_requested: false,
        })
    }

    /// 链路建立后调用：发起 Runtime 的连接流程（CONNACK 由适配器就地合成）。
    pub fn on_link_up(&mut self, endpoint: Endpoint) -> Result<Vec<Vec<u8>>, HostError> {
        self.runtime.link_up(endpoint)?;
        self.drain()
    }

    /// 链路上收到一帧（Router 下发的 V1 Stdin 帧）。
    ///
    /// 帧合法性由 Runtime 的 Stdin 解码负责；非法帧按协议回 `Failed` 或计数拒绝。
    pub fn on_frame(&mut self, frame: &[u8]) -> Result<Vec<Vec<u8>>, HostError> {
        let packet_id = self.alloc_packet_id();
        let mut properties = Properties::new();
        properties.content_type = Some(String::from(CONTENT_TYPE_STDIN));
        let publish = Publish {
            dup: false,
            qos: QoS::AtLeastOnce,
            retain: false,
            topic: Some(self.input_topic.clone()),
            packet_id: Some(packet_id),
            properties,
            payload: frame.to_vec(),
        };
        self.runtime.feed(Packet::Publish(publish))?;
        self.drain()
    }

    /// 空闲 tick：推进定时器、收件箱、任务与发件箱。
    pub fn on_poll(&mut self) -> Result<Vec<Vec<u8>>, HostError> {
        self.drain()
    }

    /// 链路意外断开（EOF/读写错误）。未确认的出站事件保留，重连后重放。
    pub fn on_link_lost(&mut self) {
        self.runtime.link_lost();
    }

    /// 链路主动关闭（不触发 Jump 回退）。
    pub fn on_link_closed(&mut self) {
        self.runtime.link_closed();
    }

    /// Runtime 是否请求关闭当前链路（Jump / 服务器迁移）。
    pub fn disconnect_requested(&self) -> bool {
        self.disconnect_requested
    }

    /// 取走一个链路动作（Jump 切换目标端点）。
    pub fn take_link_action(&mut self) -> Option<LinkAction> {
        self.runtime.take_link_action()
    }

    /// 运行时状态快照。
    pub fn status(&self) -> RuntimeStatus {
        self.runtime.status()
    }

    /// Runtime 只读访问。
    pub fn runtime(&self) -> &Runtime<C> {
        &self.runtime
    }

    /// Runtime 可变访问。
    pub fn runtime_mut(&mut self) -> &mut Runtime<C> {
        &mut self.runtime
    }

    /// 下一次需要 poll 的单调毫秒（透传 Runtime 的调度信息）。
    pub fn next_deadline(&self) -> Option<u64> {
        self.runtime.next_deadline()
    }

    /// 推进 Runtime 并把出站方向收敛为 V1 帧列表。
    ///
    /// poll 两轮：第一轮喂事件（CONNACK/SUBACK 的回喂会触发订阅与 Hello 入队），
    /// 第二轮冲刷发件箱，保证一次调用就能带回本次产生的全部帧。
    fn drain(&mut self) -> Result<Vec<Vec<u8>>, HostError> {
        let mut frames = Vec::new();
        for _ in 0..2 {
            self.runtime.poll()?;
            self.pump_outgoing(&mut frames)?;
        }
        Ok(frames)
    }

    /// 消费 Runtime 的全部出站 MQTT 报文，收敛为 V1 帧。
    fn pump_outgoing(&mut self, frames: &mut Vec<Vec<u8>>) -> Result<(), HostError> {
        while let Some(packet) = self.runtime.next_outgoing() {
            match packet {
                // 连接握手：就地合成 CONNACK（总是新会话 → 每次重连重新订阅 +
                // 重新发布 Hello，对应 retained state 的直连语义）。
                Packet::Connect(_) => {
                    let connack = Connack {
                        session_present: false,
                        reason_code: ReasonCode::Success,
                        properties: Properties::new(),
                    };
                    self.runtime.feed(Packet::Connack(connack))?;
                }
                // 订阅确认：全部授予 QoS1。
                Packet::Subscribe(subscribe) => {
                    let reason_codes = subscribe
                        .subscriptions
                        .iter()
                        .map(|_| ReasonCode::GrantedQos1)
                        .collect();
                    let suback = Suback {
                        packet_id: subscribe.packet_id,
                        properties: Properties::new(),
                        reason_codes,
                    };
                    self.runtime.feed(Packet::Suback(suback))?;
                }
                // 出站 Stdout：剥出 V1 帧写链路，并就地 PUBACK 清理发件箱。
                Packet::Publish(publish) => {
                    let packet_id = publish.packet_id;
                    frames.push(publish.payload);
                    if let Some(packet_id) = packet_id {
                        let puback = Ack {
                            packet_id,
                            reason_code: ReasonCode::Success,
                            properties: Properties::new(),
                        };
                        self.runtime.feed(Packet::Puback(puback))?;
                    }
                }
                // 保活：就地回应。
                Packet::Pingreq => {
                    self.runtime.feed(Packet::Pingresp)?;
                }
                // Runtime 确认我们注入的 Stdin PUBLISH，不需要外发。
                Packet::Puback(_) => {}
                // Runtime 主动断开（Jump / 服务器迁移）：链路应关闭。
                Packet::Disconnect(_) => {
                    self.disconnect_requested = true;
                }
                // 其余报文（UNSUBSCRIBE/AUTH 等）在直连承载下不应出现，忽略。
                _ => {}
            }
        }
        Ok(())
    }

    /// 分配合成 PUBLISH 的包 id（1..=65535 循环，跳过 0）。
    fn alloc_packet_id(&mut self) -> u16 {
        let id = self.next_packet_id;
        self.next_packet_id = if id == u16::MAX { 1 } else { id + 1 };
        id
    }
}
