//! Tool 处理器与注册表。
//!
//! Tool 是**同步、短小**的能力调用：GPIO 读写、传感器读数、本地查询等。
//! 处理器必须在一次调用内返回；运行时的 DDL 检查发生在调用返回之后，
//! 无法抢占一个阻塞中的工具。需要长时间运行的工作应实现为 Task。

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;

use crate::sekai_device_list::DeviceList;
use crate::sekai_wire::{CommandError, MAX_TOOLS, validate_name};

/// Tool 调用上下文。
pub struct ToolContext<'a> {
    /// 工具参数；由处理器自行解释。
    pub args: &'a [u8],
    /// 当前单调毫秒。
    pub now_mono_ms: u64,
    /// 当前 Unix 毫秒；未校准为 `None`。
    pub now_unix_ms: Option<u64>,
    /// Worker列表快照；处理器可读取跳转目标。
    pub workers: &'a DeviceList,
}

/// 工具处理器。
pub trait ToolHandler {
    /// 执行一次工具调用；必须尽快返回。
    fn call(&mut self, ctx: &mut ToolContext<'_>) -> Result<Vec<u8>, CommandError>;
}

/// 注册表中存放的处理器类型。
///
/// 默认构建不要求 `Send`，与宿主单线程运行时一致；启用 `send` 特性后，
/// 注册表要求处理器可跨线程移动，使[`crate::sekai_runtime::Runtime`]能在
/// Embassy 异步任务中持有。
#[cfg(not(feature = "send"))]
pub type BoxedTool = Box<dyn ToolHandler>;

/// 注册表中存放的处理器类型；`send` 特性下的版本。
#[cfg(feature = "send")]
pub type BoxedTool = Box<dyn ToolHandler + Send>;

/// Tool 注册表。
#[derive(Default)]
pub struct ToolRegistry {
    tools: BTreeMap<String, BoxedTool>,
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

impl ToolRegistry {
    /// 创建空注册表。
    pub fn new() -> Self {
        Self::default()
    }

    /// 注册工具；名称必须合法且不能占用 `sekai.` 前缀。
    #[cfg(not(feature = "send"))]
    pub fn register<T>(&mut self, name: &str, tool: T) -> Result<(), RegistryError>
    where
        T: ToolHandler + 'static,
    {
        self.register_boxed(name, Box::new(tool))
    }

    /// 注册工具；`send` 特性下额外要求处理器可跨线程移动。
    #[cfg(feature = "send")]
    pub fn register<T>(&mut self, name: &str, tool: T) -> Result<(), RegistryError>
    where
        T: ToolHandler + Send + 'static,
    {
        self.register_boxed(name, Box::new(tool))
    }

    fn register_boxed(&mut self, name: &str, tool: BoxedTool) -> Result<(), RegistryError> {
        validate_name(name).map_err(RegistryError::InvalidName)?;
        if name.starts_with("sekai.") {
            return Err(RegistryError::Reserved);
        }
        if self.tools.contains_key(name) {
            return Err(RegistryError::Duplicate);
        }
        if self.tools.len() >= MAX_TOOLS {
            return Err(RegistryError::Full);
        }
        self.tools.insert(String::from(name), tool);
        Ok(())
    }

    /// 查找工具处理器。
    pub fn get_mut(&mut self, name: &str) -> Option<&mut BoxedTool> {
        self.tools.get_mut(name)
    }

    /// 是否已注册。
    pub fn contains(&self, name: &str) -> bool {
        self.tools.contains_key(name)
    }

    /// 已注册工具数。
    pub fn len(&self) -> usize {
        self.tools.len()
    }

    /// 是否为空。
    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }

    /// 按名称升序遍历已注册工具名。
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.tools.keys().map(String::as_str)
    }
}

#[cfg(test)]
mod tests {
    use super::{RegistryError, ToolContext, ToolHandler, ToolRegistry};
    use crate::sekai_device_list::DeviceList;
    use crate::sekai_wire::CommandError;
    use alloc::vec;
    use alloc::vec::Vec;

    struct EchoTool;

    impl ToolHandler for EchoTool {
        fn call(&mut self, ctx: &mut ToolContext<'_>) -> Result<Vec<u8>, CommandError> {
            Ok(ctx.args.to_vec())
        }
    }

    #[test]
    fn register_and_call() {
        let mut registry = ToolRegistry::new();
        registry.register("echo", EchoTool).unwrap();
        assert!(registry.contains("echo"));
        assert_eq!(registry.names().collect::<Vec<_>>(), vec!["echo"]);

        let workers = DeviceList::new();
        let mut ctx = ToolContext {
            args: b"hi",
            now_mono_ms: 1,
            now_unix_ms: None,
            workers: &workers,
        };
        let result = registry.get_mut("echo").unwrap().call(&mut ctx).unwrap();
        assert_eq!(result, b"hi");
    }

    #[test]
    fn register_rejects_bad_names() {
        let mut registry = ToolRegistry::new();
        assert!(matches!(
            registry.register("", EchoTool),
            Err(RegistryError::InvalidName(_))
        ));
        assert_eq!(
            registry.register("sekai.echo", EchoTool),
            Err(RegistryError::Reserved)
        );
        registry.register("echo", EchoTool).unwrap();
        assert_eq!(
            registry.register("echo", EchoTool),
            Err(RegistryError::Duplicate)
        );
    }

    #[test]
    fn registry_is_bounded() {
        let mut registry = ToolRegistry::new();
        for index in 0..16 {
            registry
                .register(&alloc::format!("tool{index}"), EchoTool)
                .unwrap();
        }
        assert_eq!(
            registry.register("overflow", EchoTool),
            Err(RegistryError::Full)
        );
    }
}
