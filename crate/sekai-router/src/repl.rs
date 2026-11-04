//! 行式 REPL 前端：提示符 + `read_line` 循环，渲染 [`Session`] 的输出。
//!
//! 前端刻意极薄（全部逻辑在 [`crate::session::Session`]），后续全屏 TUI
//! 直接替换这一层。
//!
//! ## 后台执行与可响应性（B3）
//!
//! 自然语言目标经 [`Session::submit_line`] 放到后台线程执行，主循环用
//! stdin 读取线程 + 事件通道保持可响应：等待输入期间以 100ms 节拍排空
//! [`Session::poll`] 的异步更新（Worker 上线、后台目标完成），目标执行中
//! 仍可输入 `/cancel`。
//!
//! 管道输入（非 TTY）同样可用：逐行读入执行，EOF 退出，便于脚本化冒烟。

use std::io::{BufRead, Write};
use std::sync::mpsc;
use std::time::Duration;

use crate::session::{Session, SessionMessage};

/// 提示符。
const PROMPT: &str = "sekai> ";

/// 空闲时排空异步更新的节拍（毫秒）。
const TICK_MS: u64 = 100;

/// 输出一条会话消息；`Exit` 返回 true。
fn render(message: SessionMessage) -> bool {
    match message {
        SessionMessage::Line(text) => {
            println!("{text}");
            false
        }
        SessionMessage::Exit => true,
    }
}

/// 运行 REPL 直到 `/exit` 或 EOF；返回进程退出码。
pub fn run<C: Clone + Send + 'static + crate::llm::ChatClient>(mut session: Session<C>) -> i32 {
    println!("sekai-router 交互会话 — /help 查看命令，/exit 或 Ctrl-D(Ctrl-Z) 退出");
    println!("快速开始：/start 引导建立链路");
    // stdin 读取线程：主线程只消费事件，因此后台目标运行期间仍能处理输入。
    let (line_sender, line_receiver) = mpsc::channel::<Option<String>>();
    let reader = std::thread::Builder::new()
        .name(String::from("sekai-router-repl-stdin"))
        .spawn(move || {
            let stdin = std::io::stdin();
            for line in stdin.lock().lines() {
                let eof = line.is_err();
                let payload = line.ok();
                if line_sender.send(payload).is_err() {
                    break;
                }
                if eof {
                    break;
                }
            }
        });
    if let Err(error) = reader {
        eprintln!("sekai-router: 无法启动输入线程：{error}");
        return 1;
    }

    let mut need_prompt = true;
    loop {
        // 先排空异步更新（Worker 上线 / 后台目标完成）。
        let updates = session.poll();
        let mut updated = false;
        for message in updates {
            updated = true;
            if render(message) {
                return 0;
            }
        }
        if updated {
            need_prompt = true;
        }
        if need_prompt {
            print!("{PROMPT}");
            let _ = std::io::stdout().flush();
            need_prompt = false;
        }
        match line_receiver.recv_timeout(Duration::from_millis(TICK_MS)) {
            Ok(Some(line)) => {
                let messages = session.submit_line(&line);
                for message in messages {
                    if render(message) {
                        return 0;
                    }
                }
                need_prompt = true;
            }
            Ok(None) => {
                // EOF：正常退出。
                println!();
                return 0;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                println!();
                return 0;
            }
        }
    }
}
