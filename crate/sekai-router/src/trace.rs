//! 脱敏 JSONL trace。
//!
//! trace 只记录Worker事件、命令摘要与 LLM 用量；不记录 Authorization、API key、
//! MQTT 密码、请求头。写入失败不影响主流程（打印 warning 后继续）。
//!
//! 每条记录是一行紧凑 JSON：
//!
//! ```json
//! {"ts_unix_ms":1700000000000,"event":"llm_response",...}
//! ```
//!
//! 字段顺序稳定（先 ts_unix_ms，再 event，再调用方字段），便于测试与人工阅读。

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::json::Json;

/// 内部可变状态；Clone 经外层 `Arc` 共享（File 本身不可克隆）。
#[derive(Debug, Default)]
struct TraceInner {
    file: Option<File>,
    path: Option<PathBuf>,
}

/// trace 写入器；路径为 None 时是 no-op。
///
/// [`Clone`] 共享底层文件（内部 `Arc<Mutex>`），供 REPL 会话在多轮
/// Agent 之间复用同一 trace 流。
#[derive(Debug, Clone, Default)]
pub struct TraceWriter {
    inner: Arc<Mutex<TraceInner>>,
}

impl TraceWriter {
    /// 按路径开关 trace；Some 时以追加（create + append）方式打开。
    ///
    /// 打开失败（父目录不存在、无权限）返回 io 错误，由 CLI 决定报错退出还是
    /// 降级为 no-op。
    pub fn new(path: Option<&Path>) -> Result<Self, std::io::Error> {
        match path {
            None => Ok(Self::disabled()),
            Some(path) => {
                let file = OpenOptions::new().create(true).append(true).open(path)?;
                Ok(Self {
                    inner: Arc::new(Mutex::new(TraceInner {
                        file: Some(file),
                        path: Some(path.to_path_buf()),
                    })),
                })
            }
        }
    }

    /// no-op writer。
    pub fn disabled() -> Self {
        Self::default()
    }

    /// 是否启用。
    pub fn is_enabled(&self) -> bool {
        self.inner
            .lock()
            .map(|inner| inner.file.is_some())
            .unwrap_or(false)
    }

    /// trace 路径。
    pub fn path(&self) -> Option<PathBuf> {
        self.inner.lock().ok().and_then(|inner| inner.path.clone())
    }

    /// 写一条事件。
    ///
    /// 调用方字段中的 ts_unix_ms / event 会被忽略，避免覆盖固定字段。
    /// 序列化与写入错误只打印 warning，不 panic、不中断 agent 循环。
    pub fn event(&self, event: &str, fields: Vec<(&str, Json)>) {
        let Ok(mut inner) = self.inner.lock() else {
            return;
        };
        if inner.file.is_none() {
            return;
        }
        let mut record = Json::object(vec![
            ("ts_unix_ms", Json::from(now_unix_ms())),
            ("event", Json::string(event)),
        ]);
        for (key, value) in fields {
            if key == "ts_unix_ms" || key == "event" {
                continue;
            }
            record.set(key, value);
        }
        let mut line = record.to_compact_string();
        line.push('\n');
        let path_text = inner
            .path
            .as_ref()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| String::from("<unknown>"));
        if let Some(file) = inner.file.as_mut()
            && let Err(error) = file.write_all(line.as_bytes()).and_then(|()| file.flush())
        {
            eprintln!("sekai-router: 写 trace 失败（{path_text}）：{error}");
        }
    }
}

/// 当前 Unix 毫秒；时钟异常时返回 0。
fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(name: &str) -> PathBuf {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "sekai-router-trace-{}-{}-{name}.jsonl",
            std::process::id(),
            now_unix_ms()
        ));
        path
    }

    /// Clone 共享同一文件：两个 writer 写入互相可见。
    #[test]
    fn clone_shares_underlying_file() {
        let path = temp_path("clone");
        let writer = TraceWriter::new(Some(&path)).unwrap();
        let clone = writer.clone();
        writer.event("first", Vec::new());
        clone.event("second", Vec::new());
        drop(writer);
        drop(clone);
        let text = std::fs::read_to_string(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        assert_eq!(text.lines().count(), 2);
    }

    #[test]
    fn disabled_writer_is_noop() {
        let writer = TraceWriter::disabled();
        assert!(!writer.is_enabled());
        assert!(writer.path().is_none());
        writer.event("anything", vec![("a", Json::num(1))]);
    }

    #[test]
    fn writes_one_json_object_per_line() {
        let path = temp_path("lines");
        let writer = TraceWriter::new(Some(&path)).unwrap();
        assert!(writer.is_enabled());
        assert_eq!(writer.path(), Some(path.clone()));
        writer.event("llm_request", vec![("step", Json::num(0))]);
        writer.event(
            "tool_result",
            vec![
                ("event", Json::string("should-be-ignored")),
                ("ts_unix_ms", Json::num(7)),
                ("ok", Json::Bool(true)),
            ],
        );
        drop(writer);
        let text = std::fs::read_to_string(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2);
        for line in &lines {
            let value = Json::parse(line).unwrap();
            assert!(value.get("ts_unix_ms").and_then(Json::as_u64).is_some());
            assert!(value.get("event").and_then(Json::as_str).is_some());
        }
        let first = Json::parse(lines[0]).unwrap();
        assert_eq!(
            first.get("event").and_then(Json::as_str),
            Some("llm_request")
        );
        assert_eq!(first.get("step").and_then(Json::as_u64), Some(0));
        let second = Json::parse(lines[1]).unwrap();
        assert_eq!(
            second.get("event").and_then(Json::as_str),
            Some("tool_result")
        );
        assert_eq!(second.get("ok").and_then(Json::as_bool), Some(true));
    }

    #[test]
    fn appends_to_existing_file() {
        let path = temp_path("append");
        {
            let writer = TraceWriter::new(Some(&path)).unwrap();
            writer.event("first", Vec::new());
        }
        {
            let writer = TraceWriter::new(Some(&path)).unwrap();
            writer.event("second", Vec::new());
        }
        let text = std::fs::read_to_string(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        assert_eq!(text.lines().count(), 2);
    }

    #[test]
    fn missing_parent_directory_is_an_error() {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "sekai-router-trace-missing-{}-dir",
            std::process::id()
        ));
        path.push("trace.jsonl");
        assert!(TraceWriter::new(Some(&path)).is_err());
    }
}
