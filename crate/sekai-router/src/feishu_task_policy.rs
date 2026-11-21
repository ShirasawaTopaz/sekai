//! 飞书动态 Task 授权：单 Router 独占一个文件，commit 后才更新内存快照。

use std::collections::{BTreeSet, VecDeque};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::json::Json;
use crate::tools::{MAX_ALLOWED_TASKS, TaskAccess, normalize_task_names};

const VERSION: u64 = 1;
const MAX_FILE_BYTES: u64 = 256 * 1024;
const MAX_ID_BYTES: usize = 128;
/// 历史按提交顺序保留最近 1024 个 `/task grant` 或 `/task revoke` 消息 ID。
const MAX_RECENT_MESSAGES: usize = 1_024;
static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Copy)]
pub(crate) enum Mutation<'a> {
    Grant(&'a str),
    Revoke(&'a str),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MutationResult {
    Changed,
    Unchanged,
    Duplicate,
    Limit,
    InvalidMessageId,
}

#[derive(Clone, Default)]
struct SavedPolicy {
    grants: BTreeSet<String>,
    recent_message_ids: VecDeque<String>,
}

pub(crate) struct TaskPolicyStore {
    path: PathBuf,
    app_id: String,
    saved: SavedPolicy,
}

impl TaskPolicyStore {
    pub(crate) fn load(path: &Path, app_id: &str, base: &TaskAccess) -> io::Result<Self> {
        if !path.is_absolute() || path.file_name().is_none() || !valid_id(app_id) {
            return Err(invalid("策略文件路径或应用 ID 非法"));
        }
        let saved = match fs::symlink_metadata(path) {
            Ok(metadata) if metadata.is_file() => {
                if metadata.len() > MAX_FILE_BYTES {
                    return Err(invalid("策略文件过大"));
                }
                let mut text = String::new();
                File::open(path)?
                    .take(MAX_FILE_BYTES + 1)
                    .read_to_string(&mut text)?;
                if text.len() as u64 > MAX_FILE_BYTES {
                    return Err(invalid("策略文件过大"));
                }
                SavedPolicy::parse(&text, app_id)?
            }
            Ok(_) => return Err(invalid("策略路径不是普通文件")),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                // A missing state file is normal. A missing parent directory is not.
                if !fs::metadata(path.parent().expect("absolute file has parent"))?.is_dir() {
                    return Err(invalid("策略路径的父目录无效"));
                }
                SavedPolicy::default()
            }
            Err(error) => return Err(error),
        };
        if exact_count(base, &saved.grants) > MAX_ALLOWED_TASKS {
            return Err(invalid("启动授权与动态授权的并集超限"));
        }
        Ok(Self {
            path: path.to_path_buf(),
            app_id: app_id.to_string(),
            saved,
        })
    }

    pub(crate) fn grants(&self) -> &BTreeSet<String> {
        &self.saved.grants
    }

    pub(crate) fn fits_base(&self, base: &TaskAccess) -> bool {
        exact_count(base, self.grants()) <= MAX_ALLOWED_TASKS
    }

    pub(crate) fn effective(&self, base: &TaskAccess) -> TaskAccess {
        match base {
            TaskAccess::All => TaskAccess::All,
            _ => TaskAccess::from_names(
                base.names()
                    .into_iter()
                    .chain(self.grants().iter().cloned()),
            ),
        }
    }

    /// 保存授权与消息 ID 的同一快照。调用方持锁，成功后才对未来运行可见。
    pub(crate) fn mutate(
        &mut self,
        base: &TaskAccess,
        message_id: &str,
        operation: Mutation<'_>,
    ) -> io::Result<MutationResult> {
        if !valid_id(message_id) {
            return Ok(MutationResult::InvalidMessageId);
        }
        if self
            .saved
            .recent_message_ids
            .iter()
            .any(|id| id == message_id)
        {
            return Ok(MutationResult::Duplicate);
        }
        let mut next = self.saved.clone();
        let result = match operation {
            Mutation::Grant(name) if next.grants.insert(name.to_string()) => {
                if exact_count(base, &next.grants) > MAX_ALLOWED_TASKS {
                    next.grants.remove(name);
                    MutationResult::Limit
                } else {
                    MutationResult::Changed
                }
            }
            Mutation::Revoke(name) if next.grants.remove(name) => MutationResult::Changed,
            _ => MutationResult::Unchanged,
        };
        if next.recent_message_ids.len() == MAX_RECENT_MESSAGES {
            next.recent_message_ids.pop_front();
        }
        next.recent_message_ids.push_back(message_id.to_string());
        atomic_replace(&self.path, &next.encode(&self.app_id))?;
        self.saved = next;
        Ok(result)
    }
}

impl SavedPolicy {
    fn parse(text: &str, app_id: &str) -> io::Result<Self> {
        let json = Json::parse(text).map_err(|_| invalid("策略 JSON 损坏"))?;
        let fields = json
            .as_object()
            .ok_or_else(|| invalid("策略 JSON 必须是对象"))?;
        if fields.len() != 4 || json.get("version").and_then(Json::as_u64) != Some(VERSION) {
            return Err(invalid("策略格式版本或字段无效"));
        }
        if json.get("app_id").and_then(Json::as_str) != Some(app_id) {
            return Err(invalid("策略应用 ID 与当前配置不符"));
        }
        let grants = string_array(&json, "allowed_tasks", MAX_ALLOWED_TASKS)?;
        let normalized = normalize_task_names(grants.iter().copied())
            .map_err(|_| invalid("策略授权名称无效"))?;
        if normalized.len() != grants.len() || grants.iter().any(|name| name.trim() != *name) {
            return Err(invalid("策略授权名称重复或无效"));
        }
        let recent = string_array(&json, "applied_message_ids", MAX_RECENT_MESSAGES)?;
        let unique: BTreeSet<_> = recent.iter().copied().collect();
        if unique.len() != recent.len() || recent.iter().any(|id| !valid_id(id)) {
            return Err(invalid("策略消息去重记录重复或无效"));
        }
        Ok(Self {
            grants: normalized.into_iter().collect(),
            recent_message_ids: recent.into_iter().map(str::to_string).collect(),
        })
    }

    fn encode(&self, app_id: &str) -> Vec<u8> {
        let json = crate::json!({
            "version": VERSION,
            "app_id": app_id,
            "allowed_tasks": Json::array(self.grants.iter().map(|s| Json::from(s.as_str())).collect()),
            "applied_message_ids": Json::array(self.recent_message_ids.iter().map(|s| Json::from(s.as_str())).collect()),
        });
        let mut text = json.to_pretty_string();
        text.push('\n');
        text.into_bytes()
    }
}

fn string_array<'a>(json: &'a Json, key: &str, max_len: usize) -> io::Result<Vec<&'a str>> {
    let array = json
        .get(key)
        .and_then(Json::as_array)
        .ok_or_else(|| invalid("策略缺少必要数组字段"))?;
    if array.len() > max_len {
        return Err(invalid("策略数组超限"));
    }
    array
        .iter()
        .map(|item| item.as_str().ok_or_else(|| invalid("策略数组包含非字符串")))
        .collect()
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_ID_BYTES
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
}

fn exact_count(base: &TaskAccess, dynamic: &BTreeSet<String>) -> usize {
    match base {
        TaskAccess::Only(names) => names.union(dynamic).count(),
        _ => dynamic.len(),
    }
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn atomic_replace(path: &Path, contents: &[u8]) -> io::Result<()> {
    // Do not overwrite a symlink or a directory placed at the configured path.
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_file() => {}
        Ok(_) => return Err(invalid("策略路径不是普通文件")),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let parent = path.parent().expect("absolute file has parent");
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    // Unique names prevent stale temporary files from blocking a restart. Limit retries.
    let (temp, mut file) = (0..32)
        .find_map(|_| {
            let seq = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
            let temp = parent.join(format!(
                ".sekai-task-policy-{}-{seq}.tmp",
                std::process::id()
            ));
            match options.open(&temp) {
                Ok(file) => Some(Ok((temp, file))),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => None,
                Err(error) => Some(Err(error)),
            }
        })
        .ok_or_else(|| io::Error::new(io::ErrorKind::AlreadyExists, "无法创建策略临时文件"))??;
    let written = file.write_all(contents).and_then(|_| file.sync_all());
    drop(file);
    // std::fs::rename provides same-directory replacement of an existing file on Windows and Unix.
    let result = written.and_then(|_| fs::rename(&temp, path));
    if result.is_err() {
        let _ = fs::remove_file(temp);
    }
    result
}
