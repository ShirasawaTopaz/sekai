//! 全屏 TUI 前端：ratatui 渲染 + crossterm 事件，驱动 [`crate::session::Session`]。
//!
//! 与行式 [`crate::repl`] 同层互换：会话逻辑全在 [`Session`]，本模块只做
//! 渲染、输入编辑与终端卫生。布局自上而下：
//!
//! ```text
//! ┌状态栏──链路 ｜ 就绪 ｜ Worker 数 ｜ 退出键─────────────────┐
//! │ 输出窗格（边框标题「输出」，PgUp/PgDn 滚动，默认跟随底部） │
//! │ …                                                         │
//! ├输入行──sekai> <输入缓冲>▌────────────────────────────────┤
//! │ 帮助行：Enter 提交 ｜ PgUp/PgDn 滚动 ｜ F10/Ctrl-Q 退出   │
//! └───────────────────────────────────────────────────────────┘
//! ```
//!
//! ## 按键
//!
//! | 按键 | 动作 |
//! | --- | --- |
//! | Enter | 提交输入（`/` 命令与自然语言目标都走 [`Session::execute`]） |
//! | 常规字符 / Backspace | 编辑输入（按字符处理，中文按字退格） |
//! | Esc | 清空输入缓冲 |
//! | PgUp / PgDn | 翻页滚动输出（Ctrl+U / Ctrl+D 半页，↑/↓ 单行，Home/End 首尾） |
//! | F10 / Ctrl-Q | 退出（退出码 0，同 `/exit`） |
//! | Ctrl-C | 有后台目标时取消目标；空闲时干净退出（恢复终端后退出，退出码 130） |
//!
//! 上滚暂停自动跟随，滚回底部（End 或连续 PgDn）恢复跟随。
//!
//! ## 事件循环
//!
//! 每轮循环先排空 [`Session::poll`] 的异步更新（Worker 上线、后台目标
//! 完成事件等）再绘制；[`crossterm::event::poll`] 以 200ms 超时等待按键，
//! 超时空转触发下一轮重绘。自然语言目标经 [`Session::submit_line`] 在
//! 后台线程执行，因此 Agent 运行期间界面持续重绘、按键即时响应
//! （Ctrl-C 取消）。状态栏每帧直接读 [`Session::link_desc`] /
//! [`Session::gateway_ready`] / [`Session::worker_count`] /
//! [`Session::goal_running`]，始终最新。
//!
//! ## 终端卫生与非 TTY
//!
//! 进入时 raw mode + 备用屏 + 隐藏光标（自绘光标块）；[`TerminalGuard`]
//! 的 Drop 负责恢复终端（覆盖 panic 展开路径）。stdin/stdout 任一不是
//! 终端时不进全屏，返回 [`EXIT_NOT_TTY`]（64），由调用方回退行式 REPL。

use std::collections::VecDeque;
use std::io::{self, IsTerminal, Write};
use std::time::Duration;

use crossterm::cursor::Show;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::{Frame, Terminal};

use crate::session::{Session, SessionMessage};

/// 提示符（与行式 REPL 一致）。
const PROMPT: &str = "sekai> ";

/// 事件等待超时（毫秒）：决定 [`Session::poll`] 排空与重绘的最大延迟。
const TICK_MS: u64 = 200;

/// 非 TTY 退出码（sysexits 的 EX_NOINPUT）：调用方可据此回退行式 REPL。
pub const EXIT_NOT_TTY: i32 = 64;

/// Ctrl-C 退出码（非 0，区别于 `/exit` 的 0；128+SIGINT 惯例）。
const EXIT_INTERRUPT: i32 = 130;

/// 日志环形缓冲上限（行）：超出丢最旧。
const LOG_MAX_LINES: usize = 2000;

/// 单行最大字符数：超长行（如失控输出）截断加省略号，防撑爆内存。
const MAX_LINE_CHARS: usize = 4000;

/// 帮助行文本。
const HELP_LINE: &str =
    " Enter 提交 ｜ PgUp/PgDn·↑↓ 滚动 ｜ Esc 清空输入 ｜ Ctrl-C 取消目标 ｜ F10/Ctrl-Q 退出";

/// 运行 TUI 直到退出（`/exit`、F10/Ctrl-Q 或 Ctrl-C）；返回进程退出码。
///
/// stdin/stdout 任一不是终端时不进入全屏，eprintln 一行提示并返回
/// [`EXIT_NOT_TTY`]，由调用方决定回退（行式 REPL）。
pub fn run<C: crate::llm::ChatClient + Clone + Send + 'static>(session: Session<C>) -> i32 {
    if !is_interactive_tty(|| io::stdin().is_terminal(), || io::stdout().is_terminal()) {
        eprintln!("标准输入/输出不是终端，无法启动 TUI");
        return EXIT_NOT_TTY;
    }
    match run_fullscreen(session) {
        Ok(code) => code,
        Err(error) => {
            eprintln!("sekai-router: TUI 运行失败：{error}");
            1
        }
    }
}

/// 两个 tty 判定注入为闭包，便于单元测试。
fn is_interactive_tty(stdin_tty: impl Fn() -> bool, stdout_tty: impl Fn() -> bool) -> bool {
    stdin_tty() && stdout_tty()
}

/// 终端恢复守卫：进入时 raw mode + 备用屏；Drop 时恢复（覆盖 panic 展开路径）。
struct TerminalGuard;

impl TerminalGuard {
    fn enter() -> io::Result<Self> {
        enable_raw_mode()?;
        if let Err(error) = execute!(io::stdout(), EnterAlternateScreen) {
            // 失败也要把已开的 raw mode 还原，再上抛。
            let _ = disable_raw_mode();
            return Err(error);
        }
        Ok(Self)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
        let _ = execute!(io::stdout(), Show);
        let _ = io::stdout().flush();
    }
}

/// 全屏主循环（前置 tty 检查已通过）。
fn run_fullscreen<C: crate::llm::ChatClient + Clone + Send + 'static>(
    session: Session<C>,
) -> io::Result<i32> {
    let _guard = TerminalGuard::enter()?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    terminal.hide_cursor()?;
    let mut app = App::new(session);
    app.bootstrap();
    loop {
        // 先排空异步更新（Worker 上线通知等）再绘制，消息立即可见。
        app.drain_poll();
        if app.exit_code.is_some() {
            break;
        }
        let mut pane_height = 1;
        terminal.draw(|frame| pane_height = render(frame, &app))?;
        app.scroll.page = pane_height.max(1);
        if event::poll(Duration::from_millis(TICK_MS))?
            && let Event::Key(key) = event::read()?
            && key.kind != KeyEventKind::Release
        {
            app.on_key(key);
        }
        if app.exit_code.is_some() {
            break;
        }
    }
    Ok(app.exit_code.unwrap_or(0))
}

/// TUI 应用状态：会话 + 输入缓冲 + 日志 + 滚动。
struct App<C: crate::llm::ChatClient + Clone + Send + 'static> {
    session: Session<C>,
    input: InputBuffer,
    log: LogBuffer,
    scroll: ScrollState,
    /// `Some(code)` 表示应当退出主循环。
    exit_code: Option<i32>,
}

impl<C: crate::llm::ChatClient + Clone + Send + 'static> App<C> {
    fn new(session: Session<C>) -> Self {
        Self {
            session,
            input: InputBuffer::default(),
            log: LogBuffer::new(LOG_MAX_LINES),
            scroll: ScrollState::new(),
            exit_code: None,
        }
    }

    /// 启动横幅。
    fn bootstrap(&mut self) {
        self.append_line("sekai-router TUI — /help 查看命令，/exit 或 F10/Ctrl-Q 退出");
    }

    /// 追加一行日志：已上滚时保持视口内容不动（见 [`scroll_on_append`]）。
    fn append_line(&mut self, line: impl Into<String>) {
        scroll_on_append(&mut self.scroll);
        self.log.push(line);
    }

    /// 排空 [`Session::poll`] 的异步更新（Worker 上线通知等）。
    fn drain_poll(&mut self) {
        for message in self.session.poll() {
            self.absorb(message);
        }
    }

    /// 吸收一条会话消息到日志 / 退出状态。
    fn absorb(&mut self, message: SessionMessage) {
        match message {
            SessionMessage::Line(text) => self.append_line(text),
            SessionMessage::Exit => self.exit_code = Some(0),
        }
    }

    /// 提交当前输入：回显 → [`Session::submit_line`] → 输出进日志。
    ///
    /// 自然语言目标在后台线程执行（TUI 保持可响应），完成事件由
    /// [`App::drain_poll`] 在后续帧吸收；斜杠命令与向导输入同步返回。
    fn submit(&mut self) {
        let line = self.input.take();
        if line.trim().is_empty() {
            return;
        }
        self.append_line(format!("{PROMPT}{line}"));
        for message in self.session.submit_line(&line) {
            self.absorb(message);
            if self.exit_code.is_some() {
                return;
            }
        }
    }

    /// Ctrl-C：有后台目标时取消目标（界面继续，事件通道交付取消结果）；
    /// 空闲时保持既有语义——干净退出（退出码 130）。
    fn on_ctrl_c(&mut self) {
        if self.session.goal_running() {
            if self.session.cancel_goal() {
                self.append_line("已请求取消当前目标（在途命令将下发 sekai.cancel）");
            }
            return;
        }
        self.exit_code = Some(EXIT_INTERRUPT);
    }

    /// 处理一次按键（Press/Repeat）。
    fn on_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Enter => self.submit(),
            KeyCode::Esc => self.input.clear(),
            KeyCode::Backspace => {
                self.input.backspace();
            }
            KeyCode::PageUp => self.scroll.up(self.scroll.page),
            KeyCode::PageDown => self.scroll.down(self.scroll.page),
            KeyCode::Up => self.scroll.up(1),
            KeyCode::Down => self.scroll.down(1),
            KeyCode::Home => self.scroll.jump_top(self.log.len()),
            KeyCode::End => self.scroll.jump_bottom(),
            KeyCode::F(10) => self.exit_code = Some(0),
            KeyCode::Char(c) if key.modifiers.contains(KeyModifiers::CONTROL) => {
                match c.to_ascii_lowercase() {
                    'q' => self.exit_code = Some(0),
                    'c' => self.on_ctrl_c(),
                    'u' => self.scroll.up(self.scroll.page / 2),
                    'd' => self.scroll.down(self.scroll.page / 2),
                    _ => {}
                }
            }
            // Alt/Super 组合键不作为文本输入，其余（含 Shift）按字符插入。
            KeyCode::Char(c)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::ALT | KeyModifiers::SUPER) =>
            {
                self.input.insert(c);
            }
            _ => {}
        }
    }

    /// 输出窗格可见行：按滚动偏移取「距底部 offset 行」的窗口。
    fn visible_lines(&self, height: usize) -> Vec<Line<'static>> {
        visible_lines(&self.log, &self.scroll, height)
    }
}

/// 从日志按滚动偏移取可见窗口：视口底沿锚定在 `日志末尾 - offset` 处。
fn visible_lines(log: &LogBuffer, scroll: &ScrollState, height: usize) -> Vec<Line<'static>> {
    let end = log.len().saturating_sub(scroll.offset);
    let start = end.saturating_sub(height);
    log.lines
        .range(start..end)
        .map(|line| Line::from(line.clone()))
        .collect()
}

/// 输入缓冲：按字符编辑（UTF-8 安全，中文按字插入/退格），光标恒在末尾。
#[derive(Debug, Default, Clone)]
struct InputBuffer {
    chars: Vec<char>,
}

impl InputBuffer {
    fn insert(&mut self, c: char) {
        self.chars.push(c);
    }

    /// 退格删一个字符；缓冲为空时无操作。
    fn backspace(&mut self) -> bool {
        self.chars.pop().is_some()
    }

    fn clear(&mut self) {
        self.chars.clear();
    }

    fn text(&self) -> String {
        self.chars.iter().collect()
    }

    /// 提交：取出文本并清空。
    fn take(&mut self) -> String {
        let text = self.text();
        self.clear();
        text
    }

    /// 取可见尾部（budget 为文本可用列数，光标块另占 1 列）：
    /// 超宽时丢头部保留尾部，保证光标处的输入始终可见。
    fn visible_tail(&self, budget: usize) -> String {
        let mut kept: Vec<char> = Vec::new();
        let mut width = 0;
        for &c in self.chars.iter().rev() {
            let w = char_width(c);
            if width + w > budget {
                break;
            }
            kept.push(c);
            width += w;
        }
        kept.reverse();
        kept.iter().collect()
    }
}

/// 日志环形缓冲：超过上限丢最旧；超长行截断（见 [`MAX_LINE_CHARS`]）。
struct LogBuffer {
    lines: VecDeque<String>,
    cap: usize,
}

impl LogBuffer {
    fn new(cap: usize) -> Self {
        Self {
            lines: VecDeque::new(),
            cap,
        }
    }

    fn push(&mut self, line: impl Into<String>) {
        if self.cap == 0 {
            return;
        }
        let mut line = line.into();
        if line.chars().count() > MAX_LINE_CHARS {
            line = truncate_chars(&line, MAX_LINE_CHARS);
        }
        if self.lines.len() == self.cap {
            self.lines.pop_front();
        }
        self.lines.push_back(line);
    }

    fn len(&self) -> usize {
        self.lines.len()
    }
}

/// 滚动状态：`offset` 是视口底沿之下被隐藏的行数，`0` 即跟随底部
/// （自动滚动）。
///
/// 未跟随时新日志行由 [`scroll_on_append`] 把 offset 加一，视口内容
/// 保持不动；日志裁剪最旧行不影响「距底部」语义。
#[derive(Debug, Clone)]
struct ScrollState {
    offset: usize,
    /// 最近一帧输出窗格高度（行），翻页滚动的步长。
    page: usize,
}

impl ScrollState {
    fn new() -> Self {
        Self { offset: 0, page: 1 }
    }

    /// 是否自动跟随底部。
    fn following(&self) -> bool {
        self.offset == 0
    }

    /// 上滚 delta 行（至少 1 行）：暂停跟随。
    fn up(&mut self, delta: usize) {
        self.offset = self.offset.saturating_add(delta.max(1));
    }

    /// 下滚 delta 行（至少 1 行）：滚回底部即恢复跟随。
    fn down(&mut self, delta: usize) {
        self.offset = self.offset.saturating_sub(delta.max(1));
    }

    /// 跳到日志顶部（`total` 为当前日志行数）。
    fn jump_top(&mut self, total: usize) {
        self.offset = total.saturating_sub(1);
    }

    /// 回到底部并恢复跟随。
    fn jump_bottom(&mut self) {
        self.offset = 0;
    }
}

/// 新日志行到达时的滚动调整：已上滚（未跟随）时 offset 加一，
/// 视口内容保持不动；跟随时保持 offset=0 被新行推着走。
fn scroll_on_append(scroll: &mut ScrollState) {
    if !scroll.following() {
        scroll.offset = scroll.offset.saturating_add(1);
    }
}

/// 状态栏整行文本（纯函数，便于测试）：链路 + 就绪 + Worker 数 +
/// 后台目标运行时长 + 退出键。
fn status_bar_text(link: &str, ready: bool, workers: usize, goal_ms: Option<u64>) -> String {
    let goal = match goal_ms {
        Some(elapsed) => format!(" ｜ 执行中 {}ms（Ctrl-C 取消）", elapsed),
        None => String::new(),
    };
    format!(
        " 链路：{link} ｜ 就绪：{} ｜ Worker：{workers}{goal} ｜ F10/Ctrl-Q 退出",
        if ready { "是" } else { "否" }
    )
}

/// 渲染一帧；返回输出窗格内部高度（行），供滚动分页用。
fn render<C: crate::llm::ChatClient + Clone + Send + 'static>(
    frame: &mut Frame,
    app: &App<C>,
) -> usize {
    let [status_area, output_area, input_area] = Layout::vertical([
        Constraint::Length(1), // 状态栏
        Constraint::Min(3),    // 输出窗格（含边框）
        Constraint::Length(2), // 输入行 + 帮助行
    ])
    .areas(frame.area());

    // 状态栏：反色整行（补空格铺满宽度）；每帧直读 Session，始终最新。
    let goal_ms = if app.session.goal_running() {
        Some(app.session.goal_elapsed_ms())
    } else {
        None
    };
    let status_text = pad_to_width(
        &status_bar_text(
            app.session.link_desc(),
            app.session.gateway_ready(),
            app.session.worker_count(),
            goal_ms,
        ),
        status_area.width as usize,
    );
    frame.render_widget(
        Paragraph::new(status_text).style(Style::new().add_modifier(Modifier::REVERSED)),
        status_area,
    );

    // 输出窗格：边框 + 标题；上滚时标题带偏移提示。
    let title = if app.scroll.following() {
        String::from(" 输出 ")
    } else {
        format!(" 输出（已上滚 {} 行，End 回底） ", app.scroll.offset)
    };
    let block = Block::new().borders(Borders::ALL).title(title);
    let inner = block.inner(output_area);
    frame.render_widget(block, output_area);
    let height = inner.height as usize;
    frame.render_widget(Paragraph::new(app.visible_lines(height)), inner);

    // 输入行：提示符 + 输入尾部 + 自绘光标块（真光标已隐藏）。
    let [input_row, help_row] =
        Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).areas(input_area);
    let budget = (input_row.width as usize).saturating_sub(display_width(PROMPT) + 1);
    let input_line = Line::from(vec![
        Span::from(PROMPT),
        Span::from(app.input.visible_tail(budget)),
        Span::styled(" ", Style::new().add_modifier(Modifier::REVERSED)),
    ]);
    frame.render_widget(Paragraph::new(input_line), input_row);

    // 帮助行：暗色。
    frame.render_widget(
        Paragraph::new(Span::styled(
            HELP_LINE,
            Style::new().add_modifier(Modifier::DIM),
        )),
        help_row,
    );

    height
}

/// 终端显示宽度（列数）：宽字符 2 列，其余 1 列。
fn display_width(text: &str) -> usize {
    text.chars().map(char_width).sum()
}

/// 单字符显示宽度：覆盖常用 CJK/全角区块的简化判定（未引入 unicode-width）。
fn char_width(c: char) -> usize {
    let cp = c as u32;
    let wide = (0x1100..=0x115F).contains(&cp) // 谚文兼容字母
        || (0x2E80..=0x303E).contains(&cp) // CJK 部首/符号
        || (0x3041..=0x33FF).contains(&cp) // 假名/注音/兼容
        || (0x3400..=0x4DBF).contains(&cp) // CJK 扩展 A
        || (0x4E00..=0x9FFF).contains(&cp) // CJK 基本
        || (0xAC00..=0xD7A3).contains(&cp) // 谚文音节
        || (0xF900..=0xFAFF).contains(&cp) // CJK 兼容表意
        || (0xFE30..=0xFE4F).contains(&cp) // CJK 兼容形式
        || (0xFF00..=0xFF60).contains(&cp) // 全角形式
        || (0xFFE0..=0xFFE6).contains(&cp)
        || (0x20000..=0x3FFFD).contains(&cp); // CJK 扩展 B 及以后
    if wide { 2 } else { 1 }
}

/// 按字符数截断并加省略号（UTF-8 安全）。
fn truncate_chars(line: &str, max: usize) -> String {
    let mut out: String = line.chars().take(max).collect();
    out.push('…');
    out
}

/// 右侧补空格到指定显示宽度（已超宽则原样返回）。
fn pad_to_width(text: &str, width: usize) -> String {
    let current = display_width(text);
    if current >= width {
        text.to_string()
    } else {
        format!("{text}{}", " ".repeat(width - current))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- 输入缓冲 ----

    #[test]
    fn input_insert_backspace_take_clear() {
        let mut input = InputBuffer::default();
        assert!(input.text().is_empty());
        for c in "abc".chars() {
            input.insert(c);
        }
        assert_eq!(input.text(), "abc");
        assert!(input.backspace());
        assert_eq!(input.text(), "ab");
        assert_eq!(input.take(), "ab");
        assert!(input.text().is_empty());
        // 空缓冲退格无操作。
        assert!(!input.backspace());
        input.insert('x');
        input.clear();
        assert!(input.text().is_empty());
    }

    #[test]
    fn input_chinese_edit_by_char() {
        let mut input = InputBuffer::default();
        for c in "你好世界".chars() {
            input.insert(c);
        }
        assert_eq!(input.text(), "你好世界");
        // 中文按字符计：4 个字符、8 列。
        assert_eq!(input.text().chars().count(), 4);
        assert_eq!(display_width(&input.text()), 8);
        input.backspace();
        input.backspace();
        assert_eq!(input.text(), "你好");
        assert_eq!(display_width(&input.text()), 4);
    }

    #[test]
    fn input_visible_tail_keeps_tail_within_budget() {
        let mut input = InputBuffer::default();
        for c in "abcdefgh".chars() {
            input.insert(c);
        }
        assert_eq!(input.visible_tail(3), "fgh");
        assert_eq!(input.visible_tail(100), "abcdefgh");
        assert_eq!(input.visible_tail(0), "");
        // 中文：预算 8 列放四个字，5 列放两个字，3 列只放一个（不拆半字）。
        let mut cjk = InputBuffer::default();
        for c in "你好世界".chars() {
            cjk.insert(c);
        }
        assert_eq!(cjk.visible_tail(8), "你好世界");
        assert_eq!(cjk.visible_tail(5), "世界");
        assert_eq!(cjk.visible_tail(4), "世界");
        assert_eq!(cjk.visible_tail(3), "界");
    }

    // ---- 显示宽度 ----

    #[test]
    fn display_width_mixed_text() {
        assert_eq!(display_width("abc"), 3);
        assert_eq!(display_width("中文"), 4);
        assert_eq!(display_width("a中b"), 4);
        assert_eq!(display_width(""), 0);
    }

    // ---- 日志环形缓冲 ----

    #[test]
    fn log_trims_oldest_beyond_cap() {
        let mut log = LogBuffer::new(3);
        for text in ["a", "b", "c", "d", "e"] {
            log.push(text);
        }
        assert_eq!(log.len(), 3);
        let lines: Vec<&String> = log.lines.iter().collect();
        assert_eq!(
            lines,
            [&String::from("c"), &String::from("d"), &String::from("e")]
        );
        assert_ne!(log.len(), 0);
    }

    #[test]
    fn log_zero_cap_holds_nothing() {
        let mut log = LogBuffer::new(0);
        log.push("x");
        assert_eq!(log.len(), 0);
    }

    #[test]
    fn log_truncates_overlong_line() {
        let mut log = LogBuffer::new(10);
        let long = "x".repeat(MAX_LINE_CHARS + 500);
        log.push(long);
        let stored = log.lines.back().expect("行已入缓冲");
        assert!(stored.chars().count() <= MAX_LINE_CHARS + 1);
        assert!(stored.ends_with('…'));
        assert_eq!(stored.chars().count(), MAX_LINE_CHARS + 1);
    }

    // ---- 滚动 / 跟随 ----

    #[test]
    fn scroll_follows_bottom_by_default_and_pauses_on_up() {
        let mut scroll = ScrollState::new();
        assert!(scroll.following());
        scroll.up(10);
        assert!(!scroll.following());
        assert_eq!(scroll.offset, 10);
        scroll.down(4);
        assert_eq!(scroll.offset, 6);
        assert!(!scroll.following());
        // 滚回底部恢复跟随，且钳制在 0。
        scroll.down(100);
        assert_eq!(scroll.offset, 0);
        assert!(scroll.following());
    }

    #[test]
    fn scroll_moves_at_least_one_line() {
        let mut scroll = ScrollState::new();
        scroll.up(0);
        assert_eq!(scroll.offset, 1);
        scroll.down(0);
        assert_eq!(scroll.offset, 0);
    }

    #[test]
    fn scroll_to_top_uses_total_and_bottom_resumes() {
        let mut scroll = ScrollState::new();
        scroll.jump_top(10);
        assert_eq!(scroll.offset, 9);
        assert!(!scroll.following());
        // 日志极短时回到底部等价于跟随。
        scroll.jump_top(1);
        assert_eq!(scroll.offset, 0);
        assert!(scroll.following());
        scroll.jump_top(10);
        scroll.jump_bottom();
        assert!(scroll.following());
    }

    #[test]
    fn visible_lines_respects_offset_anchor() {
        fn pick(log: &LogBuffer, scroll: &ScrollState) -> Vec<String> {
            visible_lines(log, scroll, 3)
                .into_iter()
                .map(|line| line.to_string())
                .collect()
        }
        let mut log = LogBuffer::new(100);
        for i in 0..10 {
            log.push(format!("行{i}"));
        }
        let mut scroll = ScrollState::new();
        // 跟随：末尾 3 行。
        assert_eq!(pick(&log, &scroll), ["行7", "行8", "行9"]);
        // 上滚 2 行：视口上移。
        scroll.up(2);
        assert_eq!(pick(&log, &scroll), ["行5", "行6", "行7"]);
        // 新行到达（App::append_line 的完整动作）：视口内容不动。
        scroll_on_append(&mut scroll);
        log.push("行10");
        assert_eq!(pick(&log, &scroll), ["行5", "行6", "行7"]);
        // 跟随时则被新行推着走。
        scroll.jump_bottom();
        assert_eq!(pick(&log, &scroll), ["行8", "行9", "行10"]);
    }

    // ---- 状态栏 ----

    #[test]
    fn status_bar_text_shows_link_ready_workers() {
        let text = status_bar_text("tcp 监听（127.0.0.1:7421）", true, 2, None);
        assert!(text.contains("链路：tcp 监听"));
        assert!(text.contains("就绪：是"));
        assert!(text.contains("Worker：2"));
        assert!(!text.contains("执行中"));
        let text = status_bar_text("未连接", false, 0, None);
        assert!(text.contains("就绪：否"));
        assert!(text.contains("Worker：0"));
        assert!(text.contains("F10/Ctrl-Q 退出"));
        // 后台目标运行中：状态栏展示时长与 Ctrl-C 提示。
        let text = status_bar_text("未连接", false, 0, Some(1_234));
        assert!(text.contains("执行中 1234ms"), "{text}");
        assert!(text.contains("Ctrl-C 取消"), "{text}");
    }

    // ---- 非 TTY 判定 ----

    #[test]
    fn tty_check_requires_both_stdin_and_stdout() {
        assert!(is_interactive_tty(|| true, || true));
        assert!(!is_interactive_tty(|| true, || false));
        assert!(!is_interactive_tty(|| false, || true));
        assert!(!is_interactive_tty(|| false, || false));
    }

    // ---- 杂项 ----

    #[test]
    fn pad_to_width_pads_and_never_shrinks() {
        assert_eq!(pad_to_width("ab", 5), "ab   ");
        assert_eq!(pad_to_width("中文", 8), "中文    ");
        assert_eq!(pad_to_width("abcdef", 3), "abcdef");
        assert_eq!(pad_to_width("", 2), "  ");
    }

    #[test]
    fn exit_codes_are_distinct() {
        assert_eq!(EXIT_NOT_TTY, 64);
        assert_ne!(EXIT_INTERRUPT, 0);
    }
}
