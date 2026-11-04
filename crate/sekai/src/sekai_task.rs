//! Task 处理器与注册表。
//!
//! Task 是Worker端**协作式**的可执行单元：处理器实现 [`TaskHandler::poll`]，
//! 运行时在每个周期推进一次；返回 [`TaskPoll::Pending`] 让出 CPU，
//! 返回 `Done`/`Failed` 结束。这样无需抢占式调度器，也能在 DDL 到期时
//! 丢弃任务并上报超时。
//!
//! 处理器**不得长时间阻塞**：运行时的 DDL 检查发生在 poll 之间，
//! 无法中断一次已经进入的 poll 调用。

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;

use crate::sekai_device_list::DeviceList;
use crate::sekai_wire::{CommandError, MAX_TASKS, validate_name};

/// Task 处理器每次推进时可见的上下文。
pub struct TaskContext<'a> {
    /// 当前单调毫秒。
    pub now_mono_ms: u64,
    /// 本次执行预算的截止单调时刻。
    pub deadline_mono_ms: u64,
    /// 当前 Unix 毫秒；未校准为 `None`。
    pub now_unix_ms: Option<u64>,
    /// Worker列表快照；处理器可读取跳转目标。
    pub workers: &'a DeviceList,
}

impl TaskContext<'_> {
    /// 距离 DDL 还剩多少毫秒；已过期返回 0。
    pub fn remaining_ms(&self) -> u64 {
        self.deadline_mono_ms.saturating_sub(self.now_mono_ms)
    }

    /// 是否已经到达/超过 DDL。
    pub fn is_expired(&self) -> bool {
        self.now_mono_ms >= self.deadline_mono_ms
    }
}

/// 一次 poll 的返回值。
#[derive(Debug)]
pub enum TaskPoll {
    /// 尚未完成；`wake_after_ms` 后再次 poll（0 表示下个周期）。
    Pending {
        /// 建议的下次 poll 延迟（毫秒）；运行时会把上限截到剩余 DDL。
        wake_after_ms: u32,
    },
    /// 成功完成，携带结果字节。
    Done(Vec<u8>),
    /// 失败，携带错误。
    Failed(CommandError),
}

/// 任务处理器。
pub trait TaskHandler {
    /// 推进一步；实现必须尽快返回。
    fn poll(&mut self, ctx: &mut TaskContext<'_>) -> TaskPoll;
}

/// 注册表中存放的任务处理器类型。
///
/// 默认构建不要求 `Send`；启用 `send` 特性后要求可跨线程移动，
/// 使[`crate::sekai_runtime::Runtime`]能在 Embassy 异步任务中持有。
#[cfg(not(feature = "send"))]
pub type BoxedTask = Box<dyn TaskHandler>;

/// 注册表中存放的任务处理器类型；`send` 特性下的版本。
#[cfg(feature = "send")]
pub type BoxedTask = Box<dyn TaskHandler + Send>;

/// 根据参数创建任务处理器。
///
/// 工厂在任务真正开始时调用一次；参数已从 Stdin 载荷中剥离。
#[cfg(not(feature = "send"))]
pub type TaskFactory = Box<dyn Fn(&[u8]) -> Result<BoxedTask, CommandError>>;

/// 根据参数创建任务处理器；`send` 特性下的版本。
#[cfg(feature = "send")]
pub type TaskFactory = Box<dyn Fn(&[u8]) -> Result<BoxedTask, CommandError> + Send>;

/// Task 注册表。
#[derive(Default)]
pub struct TaskRegistry {
    tasks: BTreeMap<String, TaskFactory>,
}

/// 注册表错误。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegistryError {
    /// 名称不符合 `[a-z0-9._-]{1,48}`。
    InvalidName(&'static str),
    /// 名称已注册。
    Duplicate,
    /// 注册表已满（最多 16 项）。
    Full,
    /// 使用了保留前缀 `sekai.`。
    Reserved,
}

impl fmt::Display for RegistryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidName(message) => write!(f, "名称非法：{message}"),
            Self::Duplicate => write!(f, "名称已注册"),
            Self::Full => write!(f, "注册表已满"),
            Self::Reserved => write!(f, "名称使用保留前缀 sekai."),
        }
    }
}

impl core::error::Error for RegistryError {}

impl TaskRegistry {
    /// 创建空注册表。
    pub fn new() -> Self {
        Self::default()
    }

    /// 注册任务；名称必须合法且不能占用 `sekai.` 前缀。
    #[cfg(not(feature = "send"))]
    pub fn register<F>(&mut self, name: &str, factory: F) -> Result<(), RegistryError>
    where
        F: Fn(&[u8]) -> Result<BoxedTask, CommandError> + 'static,
    {
        self.register_boxed(name, Box::new(factory))
    }

    /// 注册任务；`send` 特性下额外要求工厂可跨线程移动。
    #[cfg(feature = "send")]
    pub fn register<F>(&mut self, name: &str, factory: F) -> Result<(), RegistryError>
    where
        F: Fn(&[u8]) -> Result<BoxedTask, CommandError> + Send + 'static,
    {
        self.register_boxed(name, Box::new(factory))
    }

    fn register_boxed(&mut self, name: &str, factory: TaskFactory) -> Result<(), RegistryError> {
        validate_name(name).map_err(RegistryError::InvalidName)?;
        if name.starts_with("sekai.") {
            return Err(RegistryError::Reserved);
        }
        if self.tasks.contains_key(name) {
            return Err(RegistryError::Duplicate);
        }
        if self.tasks.len() >= MAX_TASKS {
            return Err(RegistryError::Full);
        }
        self.tasks.insert(String::from(name), factory);
        Ok(())
    }

    /// 查找任务工厂。
    pub fn get(&self, name: &str) -> Option<&TaskFactory> {
        self.tasks.get(name)
    }

    /// 是否已注册。
    pub fn contains(&self, name: &str) -> bool {
        self.tasks.contains_key(name)
    }

    /// 已注册任务数。
    pub fn len(&self) -> usize {
        self.tasks.len()
    }

    /// 是否为空。
    pub fn is_empty(&self) -> bool {
        self.tasks.is_empty()
    }

    /// 按名称升序遍历已注册任务名。
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.tasks.keys().map(String::as_str)
    }
}

#[cfg(test)]
mod tests {
    use super::{RegistryError, TaskContext, TaskHandler, TaskPoll, TaskRegistry};
    use crate::sekai_device_list::DeviceList;
    use crate::sekai_wire::CommandError;
    use alloc::boxed::Box;
    use alloc::vec;
    use alloc::vec::Vec;

    struct EchoTask {
        args: Vec<u8>,
        polled: bool,
    }

    impl TaskHandler for EchoTask {
        fn poll(&mut self, _ctx: &mut TaskContext<'_>) -> TaskPoll {
            if self.polled {
                return TaskPoll::Pending { wake_after_ms: 1 };
            }
            self.polled = true;
            TaskPoll::Done(self.args.clone())
        }
    }

    fn context<'a>(workers: &'a DeviceList) -> TaskContext<'a> {
        TaskContext {
            now_mono_ms: 100,
            deadline_mono_ms: 200,
            now_unix_ms: None,
            workers,
        }
    }

    #[test]
    fn register_and_lookup() {
        let mut registry = TaskRegistry::new();
        registry
            .register("echo", |args| {
                Ok(Box::new(EchoTask {
                    args: args.to_vec(),
                    polled: false,
                }))
            })
            .unwrap();
        assert!(registry.contains("echo"));
        assert_eq!(registry.len(), 1);
        assert_eq!(registry.names().collect::<Vec<_>>(), vec!["echo"]);

        let factory = registry.get("echo").unwrap();
        let mut task = factory(b"hi").unwrap();
        let workers = DeviceList::new();
        let mut ctx = context(&workers);
        match task.poll(&mut ctx) {
            TaskPoll::Done(result) => assert_eq!(result, b"hi"),
            _ => panic!("expected Done"),
        }
    }

    #[test]
    fn register_rejects_bad_names() {
        let mut registry = TaskRegistry::new();
        assert!(matches!(
            registry.register("", |_| Err(CommandError::from(
                crate::sekai_wire::ErrorCode::Internal
            ))),
            Err(RegistryError::InvalidName(_))
        ));
        assert_eq!(
            registry.register("sekai.echo", |_| unreachable!()),
            Err(RegistryError::Reserved)
        );
        registry
            .register("echo", |_| {
                Ok(Box::new(EchoTask {
                    args: Vec::new(),
                    polled: false,
                }))
            })
            .unwrap();
        assert_eq!(
            registry.register("echo", |_| unreachable!()),
            Err(RegistryError::Duplicate)
        );
    }

    #[test]
    fn registry_is_bounded() {
        let mut registry = TaskRegistry::new();
        for index in 0..16 {
            registry
                .register(&alloc::format!("task{index}"), |_| {
                    Ok(Box::new(EchoTask {
                        args: Vec::new(),
                        polled: false,
                    }))
                })
                .unwrap();
        }
        assert_eq!(
            registry.register("overflow", |_| unreachable!()),
            Err(RegistryError::Full)
        );
    }
}
