use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::Duration;

use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::{Alignment, Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{List, ListItem, ListState, Paragraph};
use ratatui::Frame;

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::scan::{self, FileEntry};
use crate::ui::format_bytes;

/// 交互式浏览器入口：从 start 路径进入事件循环，直到用户退出
pub fn run(start: &Path) -> std::io::Result<()> {
    let mut app = App::new(start);
    let mut terminal = ratatui::init();
    let result = run_app(&mut app, &mut terminal);
    ratatui::restore();
    result
}

/// 浏览器的内部状态
struct App {
    /// 当前显示（或正在进入）的目录，标题行展示它
    target: PathBuf,
    /// 能返回的最高层级（启动时的目录），向上翻页不会越过它
    root: PathBuf,
    entries: Vec<FileEntry>,
    selected: usize,
    /// 是否正在后台扫描
    loading: bool,
    /// 后台扫描线程回传结果的通道
    rx: Option<Receiver<Vec<FileEntry>>>,
}

impl App {
    fn new(start: &Path) -> Self {
        // 统一成绝对路径；若起点是文件，则上移到其父目录
        let start = std::fs::canonicalize(start).unwrap_or_else(|_| start.to_path_buf());
        let target = if start.is_dir() {
            start
        } else {
            start.parent().map(Path::to_path_buf).unwrap_or(start)
        };

        let mut app = App {
            root: target.clone(),
            target,
            entries: Vec::new(),
            selected: 0,
            loading: false,
            rx: None,
        };
        app.start_scan();
        app
    }

    /// 在后台线程里扫描当前目标目录，期间界面保持响应
    fn start_scan(&mut self) {
        self.loading = true;
        self.entries = Vec::new();
        self.selected = 0;

        let target = self.target.clone();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let entries = scan::list_directory(&target);
            let _ = tx.send(entries);
        });
        self.rx = Some(rx);
    }

    /// 检查后台扫描是否完成，完成后把结果装进界面
    fn poll_result(&mut self) {
        let Some(rx) = self.rx.as_ref() else {
            return;
        };
        match rx.try_recv() {
            Ok(entries) => {
                self.entries = entries;
                self.loading = false;
                self.rx = None;
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => {
                self.loading = false;
                self.rx = None;
            }
        }
    }

    /// 上/下移动选中项，到边界时循环
    fn move_cursor(&mut self, delta: isize) {
        if self.entries.is_empty() {
            return;
        }
        let len = self.entries.len() as isize;
        let sel = (self.selected as isize + delta).rem_euclid(len);
        self.selected = sel as usize;
    }

    /// Enter：进入选中的子目录
    fn descend(&mut self) {
        if self.loading {
            return;
        }
        if let Some(entry) = self.entries.get(self.selected)
            && entry.is_dir
        {
            self.target = entry.path.clone();
            self.start_scan();
        }
    }

    /// Esc/Backspace/Left：返回上级目录（也能中断正在进行的扫描），不越过启动时的层级
    fn ascend(&mut self) {
        if self.target == self.root {
            return;
        }
        if let Some(parent) = self.target.parent() {
            self.target = parent.to_path_buf();
            self.start_scan();
        }
    }
}

/// 事件循环：处理按键并持续重绘
fn run_app(app: &mut App, terminal: &mut ratatui::DefaultTerminal) -> std::io::Result<()> {
    loop {
        app.poll_result();
        terminal.draw(|frame| app.draw(frame))?;

        if event::poll(Duration::from_millis(100))?
            && let Event::Key(key) = event::read()?
        {
            if key.kind != KeyEventKind::Press {
                continue;
            }
            match key.code {
                KeyCode::Char('q') => return Ok(()),
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    return Ok(());
                }
                KeyCode::Esc | KeyCode::Backspace | KeyCode::Left => app.ascend(),
                KeyCode::Enter | KeyCode::Right => app.descend(),
                KeyCode::Down | KeyCode::Char('j') => app.move_cursor(1),
                KeyCode::Up | KeyCode::Char('k') => app.move_cursor(-1),
                _ => {}
            }
        }
    }
}

impl App {
    fn draw(&mut self, frame: &mut Frame) {
        let [header_area, body_area, footer_area] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(0),
            Constraint::Length(1),
        ])
        .areas(frame.area());

        // 标题行：当前目录
        let header = Line::from(vec![
            Span::raw("当前目录: "),
            Span::styled(
                self.target.display().to_string(),
                Style::default()
                    .fg(Color::Rgb(255, 184, 0))
                    .add_modifier(Modifier::BOLD),
            ),
        ]);
        frame.render_widget(Paragraph::new(header), header_area);

        // 主体：扫描中提示，或条目列表
        if self.loading {
            frame.render_widget(
                Paragraph::new("正在扫描占用，请稍候…")
                    .alignment(Alignment::Center),
                body_area,
            );
        } else if self.entries.is_empty() {
            frame.render_widget(
                Paragraph::new("(目录为空或无法读取)")
                    .alignment(Alignment::Center),
                body_area,
            );
        } else {
            // 名称列统一定宽（按显示宽度，含全角），保证大小列始终对齐
            let name_width = self
                .entries
                .iter()
                .map(display_name_width)
                .max()
                .unwrap_or(0)
                .clamp(1, 40);
            let items: Vec<ListItem> = self
                .entries
                .iter()
                .map(|entry| render_entry(entry, name_width))
                .collect();
            let list = List::new(items)
                .highlight_style(Style::default().bg(Color::Rgb(45, 45, 45)))
                .highlight_symbol("");
            let mut state = ListState::default();
            state.select(Some(self.selected));
            frame.render_stateful_widget(list, body_area, &mut state);
        }

        // 底部：状态与按键提示
        let footer = if self.loading {
            "[Esc] 取消  [q] 退出".to_string()
        } else {
            let total_size: u64 = self.entries.iter().map(|e| e.size).sum();
            format!(
                "{} 项 · 合计 {:>8}     [↑↓/jk]移动  [Enter]进入  [Esc]返回  [q]退出",
                self.entries.len(),
                format_bytes(total_size),
            )
        };
        frame.render_widget(Paragraph::new(footer), footer_area);
    }
}

/// 条目显示名：目录以 / 结尾
fn display_name(entry: &FileEntry) -> String {
    if entry.is_dir {
        format!("{}/", entry.name)
    } else {
        entry.name.clone()
    }
}

/// 条目显示名的终端格子宽度（全角字符按 2 格算）
fn display_name_width(entry: &FileEntry) -> usize {
    UnicodeWidthStr::width(display_name(entry).as_str())
}

/// 按显示宽度把字符串截断到 max_width 格以内
fn truncate_to_width(s: &str, max_width: usize) -> String {
    let mut out = String::new();
    let mut width = 0;
    for ch in s.chars() {
        let w = ch.width().unwrap_or(0);
        if width + w > max_width {
            break;
        }
        out.push(ch);
        width += w;
    }
    out
}

/// 把单个条目渲染成一行：名称（定宽截断） + 大小
fn render_entry(entry: &FileEntry, name_width: usize) -> ListItem<'static> {
    let mut name = display_name(entry);
    // 超出 name_width 的用 … 截断，保证大小列对齐
    if UnicodeWidthStr::width(name.as_str()) > name_width {
        let keep = name_width.saturating_sub(1);
        name = truncate_to_width(&name, keep) + "…";
    }
    let pad = " ".repeat(name_width - UnicodeWidthStr::width(name.as_str()));
    let text = format!("  {name}{pad}  {:>10}", format_bytes(entry.size));
    ListItem::new(Line::from(text))
}
