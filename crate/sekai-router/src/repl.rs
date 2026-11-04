//! 行式 REPL 前端：提示符 + `read_line` 循环，渲染 [`Session`] 的输出。
//!
//! 前端刻意极薄（全部逻辑在 [`crate::session::Session`]），后续全屏 TUI
//! 直接替换这一层。每条命令执行后排空 [`Session::poll`] 的异步更新
//! （Worker 上线等），直到为空；异步消息仍可能与提示符交叠——TUI 阶段
//! 再彻底解决。
//!
//! 管道输入（非 TTY）同样可用：逐行读入执行，EOF 退出，便于脚本化冒烟。

use std::io::{BufRead, Write};

use crate::session::{Session, SessionMessage};

/// 提示符。
const PROMPT: &str = "sekai> ";

/// 每条命令后排空 [`Session::poll`] 的轮数上限（防止饥饿）。
const DRAIN_MAX_ROUNDS: usize = 16;

/// 运行 REPL 直到 `/exit` 或 EOF；返回进程退出码。
pub fn run<C: Clone + crate::llm::ChatClient>(mut session: Session<C>) -> i32 {
    println!("sekai-router 交互会话 — /help 查看命令，/exit 或 Ctrl-D(Ctrl-Z) 退出");
    println!("快速开始：/start 引导建立链路");
    let stdin = std::io::stdin();
    let mut lines = stdin.lock().lines();
    loop {
        print!("{PROMPT}");
        let _ = std::io::stdout().flush();
        let Some(line) = lines.next() else {
            // EOF：正常退出。
            println!();
            return 0;
        };
        let line = match line {
            Ok(line) => line,
            Err(error) => {
                eprintln!("sekai-router: 读取输入失败：{error}");
                return 1;
            }
        };
        for message in session.execute(&line) {
            match message {
                SessionMessage::Line(text) => println!("{text}"),
                SessionMessage::Exit => return 0,
            }
        }
        // 排空异步更新（Worker 上线等），直到为空；封顶轮数防止饥饿。
        for _ in 0..DRAIN_MAX_ROUNDS {
            let updates = session.poll();
            if updates.is_empty() {
                break;
            }
            for message in updates {
                match message {
                    SessionMessage::Line(text) => println!("{text}"),
                    SessionMessage::Exit => return 0,
                }
            }
        }
    }
}
