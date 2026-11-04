//! Worker注册表：能力快照、在线状态与 (boot_id, seq) 去重。
//!
//! 注册表是纯数据结构，不读时钟、不做 I/O；所有方法接收调用方传入的
//! 单调毫秒或已解析的 Stdout 帧。

use std::collections::{HashMap, VecDeque};

use sekai::sekai_stdout::{ActiveTask, Hello, Phase, Stdout};

/// 默认去重窗口大小（每Worker）。
pub const DEFAULT_DEDUP_WINDOW: usize = 256;

/// Hello 中活跃任务的快照。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveTaskInfo {
    /// 原命令 id。
    pub cmd_id: u64,
    /// 任务名。
    pub name: String,
    /// 已运行毫秒。
    pub elapsed_ms: u64,
}

impl From<&ActiveTask> for ActiveTaskInfo {
    fn from(value: &ActiveTask) -> Self {
        Self {
            cmd_id: value.cmd_id,
            name: value.name.clone(),
            elapsed_ms: value.elapsed_ms,
        }
    }
}

/// Worker信息快照（对外只读）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerInfo {
    /// Worker id。
    pub worker_id: String,
    /// 是否在线。
    pub online: bool,
    /// 本次启动周期 id。
    pub boot_id: u64,
    /// 固件版本。
    pub firmware: String,
    /// Worker自身上报的 uptime。
    pub uptime_ms: u64,
    /// Worker Unix 毫秒；0 表示未校准。
    pub unix_ms: u64,
    /// 已通告工具（含 sekai.* 内置工具）。
    pub tools: Vec<String>,
    /// 已通告任务。
    pub tasks: Vec<String>,
    /// 跳转目标列表 (name, host, port)。
    pub workers: Vec<(String, String, u16)>,
    /// 活跃任务。
    pub active_tasks: Vec<ActiveTaskInfo>,
    /// 最近一次收到该Worker帧的单调毫秒。
    pub last_seen_mono_ms: u64,
    /// 最近一次 Hello 的 seq。
    pub hello_seq: u32,
}

impl WorkerInfo {
    /// 是否通告了某个工具。
    pub fn has_tool(&self, name: &str) -> bool {
        self.tools.iter().any(|tool| tool == name)
    }

    /// 是否通告了某个任务。
    pub fn has_task(&self, name: &str) -> bool {
        self.tasks.iter().any(|task| task == name)
    }
}

/// 一帧是否首次出现。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameAcceptance {
    /// 首次出现，应继续处理。
    New,
    /// 重复投递，应只更新 last_seen。
    Duplicate,
}

/// 注册表事件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegistryEvent {
    /// Worker信息变化（上线/能力更新/离线）。
    Updated(WorkerInfo),
    /// Worker重启（boot_id 变化）。
    Rebooted {
        /// Worker id。
        worker_id: String,
        /// 旧 boot_id。
        old_boot_id: u64,
        /// 新 boot_id。
        new_boot_id: u64,
    },
    /// Worker离线。
    Offline {
        /// Worker id。
        worker_id: String,
    },
}

/// Worker注册表错误。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkerError {
    /// Hello 内的 worker_id 与主题不一致。
    WorkerIdMismatch { topic: String, hello: String },
    /// Worker id 非法。
    InvalidWorkerId(String),
}

impl std::fmt::Display for WorkerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::WorkerIdMismatch { topic, hello } => {
                write!(
                    f,
                    "Hello Worker id 与主题不一致：topic={topic}, hello={hello}"
                )
            }
            Self::InvalidWorkerId(id) => write!(f, "Worker id 非法：{id}"),
        }
    }
}

impl std::error::Error for WorkerError {}

struct DedupWindow {
    boot_id: u64,
    seen: VecDeque<(u64, u32)>,
}

/// Worker注册表。
pub struct WorkerRegistry {
    dedup_window: usize,
    workers: HashMap<String, WorkerInfo>,
    dedup: HashMap<String, DedupWindow>,
}

impl WorkerRegistry {
    /// 创建空注册表。
    pub fn new(dedup_window: usize) -> Self {
        Self {
            dedup_window: dedup_window.max(1),
            workers: HashMap::new(),
            dedup: HashMap::new(),
        }
    }

    /// 所有Worker快照，按 worker_id 排序。
    pub fn workers(&self) -> Vec<WorkerInfo> {
        let mut list: Vec<WorkerInfo> = self.workers.values().cloned().collect();
        list.sort_by(|a, b| a.worker_id.cmp(&b.worker_id));
        list
    }

    /// 单个Worker快照。
    pub fn get(&self, worker_id: &str) -> Option<WorkerInfo> {
        self.workers.get(worker_id).cloned()
    }

    /// Worker是否在线。
    pub fn is_online(&self, worker_id: &str) -> bool {
        self.workers
            .get(worker_id)
            .map(|worker| worker.online)
            .unwrap_or(false)
    }

    /// 接受一帧的去重检查；重复帧只应更新 last_seen。
    pub fn accept_frame(&mut self, worker_id: &str, boot_id: u64, seq: u32) -> FrameAcceptance {
        let entry = self
            .dedup
            .entry(String::from(worker_id))
            .or_insert_with(|| DedupWindow {
                boot_id,
                seen: VecDeque::new(),
            });
        if entry.boot_id != boot_id {
            entry.boot_id = boot_id;
            entry.seen.clear();
        }
        if entry.seen.iter().any(|(b, s)| *b == boot_id && *s == seq) {
            return FrameAcceptance::Duplicate;
        }
        if entry.seen.len() >= self.dedup_window {
            entry.seen.pop_front();
        }
        entry.seen.push_back((boot_id, seq));
        FrameAcceptance::New
    }

    /// 清空去重窗口。
    ///
    /// 新的 MQTT 连接建立后调用：retained Hello/Offline 是当前权威状态，
    /// 断链前记录的 (boot_id, seq) 不应阻止它们被重新处理。同一连接内的
    /// QoS1 重投仍由窗口去重。
    pub fn reset_dedup(&mut self) {
        self.dedup.clear();
    }

    /// 更新 last_seen 单调毫秒。
    pub fn mark_seen(&mut self, worker_id: &str, now_mono_ms: u64) {
        if let Some(worker) = self.workers.get_mut(worker_id) {
            worker.last_seen_mono_ms = now_mono_ms;
        }
    }

    /// 应用 Hello。
    pub fn apply_hello(
        &mut self,
        topic_worker_id: &str,
        hello: &Hello,
        boot_id: u64,
        seq: u32,
        now_mono_ms: u64,
    ) -> Result<Vec<RegistryEvent>, WorkerError> {
        if hello.worker_id != topic_worker_id {
            return Err(WorkerError::WorkerIdMismatch {
                topic: String::from(topic_worker_id),
                hello: hello.worker_id.clone(),
            });
        }
        let mut events = Vec::new();
        let old_boot = self
            .workers
            .get(topic_worker_id)
            .map(|worker| worker.boot_id);
        let mut info = self
            .workers
            .get(topic_worker_id)
            .cloned()
            .unwrap_or_else(|| WorkerInfo {
                worker_id: String::from(topic_worker_id),
                online: false,
                boot_id,
                firmware: String::new(),
                uptime_ms: 0,
                unix_ms: 0,
                tools: Vec::new(),
                tasks: Vec::new(),
                workers: Vec::new(),
                active_tasks: Vec::new(),
                last_seen_mono_ms: now_mono_ms,
                hello_seq: seq,
            });
        if let Some(old_boot_id) = old_boot
            && old_boot_id != boot_id
        {
            events.push(RegistryEvent::Rebooted {
                worker_id: String::from(topic_worker_id),
                old_boot_id,
                new_boot_id: boot_id,
            });
        }
        info.online = true;
        info.boot_id = boot_id;
        info.firmware = hello.firmware.clone();
        info.uptime_ms = hello.uptime_ms;
        info.unix_ms = hello.unix_ms;
        info.tools = hello.tools.clone();
        info.tasks = hello.tasks.clone();
        info.workers = hello
            .workers
            .iter()
            .map(|worker| (worker.name.clone(), worker.ip.clone(), worker.port))
            .collect();
        info.active_tasks = hello
            .active_tasks
            .iter()
            .map(ActiveTaskInfo::from)
            .collect();
        info.last_seen_mono_ms = now_mono_ms;
        info.hello_seq = seq;
        self.workers
            .insert(String::from(topic_worker_id), info.clone());
        events.push(RegistryEvent::Updated(info));
        Ok(events)
    }

    /// 应用 Offline。
    pub fn apply_offline(
        &mut self,
        worker_id: &str,
        boot_id: u64,
        seq: u32,
        now_mono_ms: u64,
    ) -> Result<Vec<RegistryEvent>, WorkerError> {
        let mut events = Vec::new();
        if let Some(worker) = self.workers.get_mut(worker_id) {
            worker.online = false;
            worker.boot_id = boot_id;
            worker.last_seen_mono_ms = now_mono_ms;
            worker.hello_seq = seq;
            events.push(RegistryEvent::Updated(worker.clone()));
            events.push(RegistryEvent::Offline {
                worker_id: String::from(worker_id),
            });
        }
        Ok(events)
    }

    /// 链路断开时把所有在线Worker标记为离线。
    pub fn mark_all_offline(&mut self, now_mono_ms: u64) -> Vec<RegistryEvent> {
        let mut events = Vec::new();
        let ids: Vec<String> = self.workers.keys().cloned().collect();
        for id in ids {
            if let Some(worker) = self.workers.get_mut(&id)
                && worker.online
            {
                worker.online = false;
                worker.last_seen_mono_ms = now_mono_ms;
                events.push(RegistryEvent::Updated(worker.clone()));
                events.push(RegistryEvent::Offline { worker_id: id });
            }
        }
        events
    }
}

impl Default for WorkerRegistry {
    fn default() -> Self {
        Self::new(DEFAULT_DEDUP_WINDOW)
    }
}

/// 辅助：把 Stdout 阶段转为显示字符串。
pub fn phase_name(phase: Phase) -> &'static str {
    match phase {
        Phase::Accepted => "Accepted",
        Phase::Started => "Started",
        Phase::Progress => "Progress",
        Phase::Completed => "Completed",
        Phase::Failed => "Failed",
        Phase::Hello => "Hello",
        Phase::Offline => "Offline",
    }
}

/// 辅助：判断 Stdout 帧是否携带命令事件（用于 control 分派）。
pub fn is_command_phase(phase: Phase) -> bool {
    matches!(
        phase,
        Phase::Accepted | Phase::Started | Phase::Progress | Phase::Completed | Phase::Failed
    )
}

/// 辅助：判断一帧 Stdout 是否应被忽略（未使用的阶段/未知语义）。
pub fn should_ignore(_stdout: &Stdout) -> bool {
    false
}
