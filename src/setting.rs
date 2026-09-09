use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

use ratatui::Frame;
use ratatui::crossterm::ExecutableCommand;
use ratatui::crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::ui::sunset_gradient;

/// 亮度改动后等待多久再真正写硬件（毫秒级，滑动时只应用最后一次）
const BRIGHT_DEBOUNCE: Duration = Duration::from_millis(120);
/// 熄屏/睡眠改动后等待多久再写配置并重启 hypridle
const TIME_DEBOUNCE: Duration = Duration::from_millis(500);

const SCREEN_OFF_MIN: u32 = 1;
const SCREEN_OFF_MAX: u32 = 240;
const SLEEP_MIN: u32 = 1;
const SLEEP_MAX: u32 = 720;

/// 主题强调色，与磁盘表的“落日余晖”同系
const ACCENT: Color = Color::Rgb(255, 184, 0);
const TRACK: Color = Color::Rgb(60, 60, 60);

/// 启动交互式系统设置界面，退出时恢复终端
pub fn run() -> io::Result<()> {
    let mut terminal = ratatui::init();
    let _ = io::stdout().execute(EnableMouseCapture);
    let mut app = App::new();
    let result = run_app(&mut app, &mut terminal);
    let _ = io::stdout().execute(DisableMouseCapture);
    ratatui::restore();
    result
}

// ==================== 亮度后端（DDC/CI） ====================

/// 外接显示器亮度后端：记录 ddcutil 的 i2c 总线号，避免每次都重新探测
#[derive(Clone, Copy)]
struct Brightness {
    bus: u32,
}

impl Brightness {
    /// 探测第一台支持 DDC/CI 的显示器（跳过内建屏的 "Invalid display" 段）
    fn detect() -> Option<Brightness> {
        let out = Command::new("ddcutil")
            .args(["detect", "--brief"])
            .output()
            .ok()?;
        let text = String::from_utf8_lossy(&out.stdout);
        Some(Brightness {
            bus: parse_ddc_bus(&text)?,
        })
    }

    /// 读取当前亮度，返回 (当前值, 最大值)
    fn read(&self) -> Option<(u32, u32)> {
        let bus = self.bus.to_string();
        let out = Command::new("ddcutil")
            .args([
                "--bus",
                bus.as_str(),
                "getvcp",
                "10",
                "--terse",
                "--sleep-multiplier",
                "0.1",
            ])
            .output()
            .ok()?;
        parse_vcp_terse(&String::from_utf8_lossy(&out.stdout))
    }

    fn set(&self, value: u32) -> io::Result<()> {
        let bus = self.bus.to_string();
        let val = value.to_string();
        let status = Command::new("ddcutil")
            .args([
                "--bus",
                bus.as_str(),
                "setvcp",
                "10",
                val.as_str(),
                "--sleep-multiplier",
                "0.1",
                "--noverify",
            ])
            .status()?;
        if status.success() {
            Ok(())
        } else {
            Err(io::Error::other(format!("ddcutil 退出码 {status}")))
        }
    }
}

/// 从 `ddcutil detect --brief` 输出里取第一台有效显示器的 i2c 总线号。
/// 形如 `I2C bus: /dev/i2c-13`；"Invalid display" 段（内建屏不支持 DDC/CI）跳过。
fn parse_ddc_bus(text: &str) -> Option<u32> {
    let mut invalid = false;
    for line in text.lines() {
        let t = line.trim();
        if t.eq_ignore_ascii_case("Invalid display") {
            invalid = true;
        } else if t.starts_with("Display ") {
            invalid = false;
        } else if !invalid
            && let Some(rest) = t.strip_prefix("I2C bus:")
            && let Some(n) = rest.trim().rsplit('-').next()
            && let Ok(v) = n.parse::<u32>()
        {
            return Some(v);
        }
    }
    None
}

/// 解析 `ddcutil getvcp 10 --terse` 的输出，形如 `VCP 10 C 7 100`
fn parse_vcp_terse(text: &str) -> Option<(u32, u32)> {
    for line in text.lines() {
        let f: Vec<&str> = line.split_whitespace().collect();
        if f.len() >= 5 && f[0] == "VCP" && f[1] == "10" && f[2] == "C" {
            let cur = f[3].parse().ok()?;
            let max = f[4].parse().ok()?;
            return Some((cur, max));
        }
    }
    None
}

// ==================== 熄屏 / 睡眠后端（hypridle 配置） ====================

/// hypridle 配置文件里的一段 listener
#[derive(Debug, PartialEq)]
struct Listener {
    /// `listener {` 所在行
    start: usize,
    /// 对应的 `}` 所在行
    end: usize,
    /// 块内 `timeout = ...` 所在行
    timeout_line: Option<usize>,
    /// `on-timeout` 的命令文本
    on_timeout: String,
}

/// 扫描出所有 listener 块
fn parse_listeners(lines: &[&str]) -> Vec<Listener> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        if lines[i].trim().starts_with("listener") && lines[i].contains('{') {
            let start = i;
            let mut end = lines.len().saturating_sub(1);
            for (j, line) in lines.iter().enumerate().skip(start + 1) {
                if line.trim() == "}" {
                    end = j;
                    break;
                }
            }
            let mut timeout_line = None;
            let mut on_timeout = String::new();
            for (k, line) in lines.iter().enumerate().take(end).skip(start + 1) {
                let t = line.trim();
                if t.starts_with("timeout") {
                    timeout_line = Some(k);
                } else if let Some(rest) = t.strip_prefix("on-timeout")
                    && let Some((_, v)) = rest.split_once('=')
                {
                    on_timeout = v.trim().to_string();
                }
            }
            out.push(Listener {
                start,
                end,
                timeout_line,
                on_timeout,
            });
            i = end + 1;
        } else {
            i += 1;
        }
    }
    out
}

/// 把包含 `needle` 的 listener 的 timeout 改成 `secs`；找不到返回 false
fn set_listener_timeout(lines: &mut Vec<String>, needle: &str, secs: u32) -> bool {
    let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
    let Some(l) = parse_listeners(&refs)
        .into_iter()
        .find(|l| l.on_timeout.contains(needle))
    else {
        return false;
    };
    let indent = l
        .timeout_line
        .and_then(|k| lines.get(k))
        .map(|s| " ".repeat(s.len() - s.trim_start().len()))
        .unwrap_or_else(|| "    ".to_string());
    let new_line = format!("{indent}timeout = {secs}");
    match l.timeout_line {
        Some(k) => lines[k] = new_line,
        None => lines.insert(l.start + 1, new_line),
    }
    true
}

/// 追加一段熄屏 listener（用 niri 的 DPMS 动作）
fn append_screen_off(lines: &mut Vec<String>, secs: u32) {
    if lines.last().is_some_and(|l| !l.trim().is_empty()) {
        lines.push(String::new());
    }
    lines.push("# 熄屏（由 info-setting 管理）".to_string());
    lines.push("listener {".to_string());
    lines.push(format!("    timeout = {secs}"));
    lines.push("    on-timeout = niri msg action power-off-monitors".to_string());
    lines.push("    on-resume = niri msg action power-on-monitors".to_string());
    lines.push("}".to_string());
}

/// 生成新的配置文本：改睡眠、改/加熄屏，其余内容（含注释）原样保留
fn rewrite_idle_config(text: &str, screen_off_secs: u32, sleep_secs: u32) -> String {
    let mut lines: Vec<String> = text.lines().map(String::from).collect();
    if !set_listener_timeout(&mut lines, "power-off-monitors", screen_off_secs) {
        append_screen_off(&mut lines, screen_off_secs);
    }
    set_listener_timeout(&mut lines, "systemctl suspend", sleep_secs);
    let mut out = lines.join("\n");
    out.push('\n');
    out
}

/// 读出包含 `needle` 的 listener 的 timeout（秒）
fn read_listener_secs(text: &str, needle: &str) -> Option<u32> {
    let lines: Vec<&str> = text.lines().collect();
    let l = parse_listeners(&lines)
        .into_iter()
        .find(|l| l.on_timeout.contains(needle))?;
    let k = l.timeout_line?;
    lines[k].trim().split_once('=')?.1.trim().parse().ok()
}

/// hypridle 配置：读改写 + 重启守护进程
#[derive(Clone)]
struct IdleConfig {
    path: PathBuf,
}

impl IdleConfig {
    fn new() -> IdleConfig {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_default();
        IdleConfig {
            path: home.join(".config/hypr/hypridle.conf"),
        }
    }

    fn load(&self) -> Option<String> {
        std::fs::read_to_string(&self.path).ok()
    }

    /// 备份原文件（仅在首次）、原子写入新配置，然后重启 hypridle
    fn apply(&self, screen_off_secs: u32, sleep_secs: u32) -> io::Result<()> {
        let text = std::fs::read_to_string(&self.path)?;
        let new_text = rewrite_idle_config(&text, screen_off_secs, sleep_secs);
        if new_text == text {
            return Ok(());
        }
        let bak = PathBuf::from(format!("{}.bak", self.path.display()));
        if !bak.exists() {
            std::fs::copy(&self.path, &bak)?;
        }
        let dir = self.path.parent().unwrap_or_else(|| Path::new("."));
        let tmp = dir.join(".hypridle.conf.info-setting.tmp");
        std::fs::write(&tmp, &new_text)?;
        std::fs::rename(&tmp, &self.path)?;
        restart_hypridle()
    }
}

/// 杀掉旧的 hypridle，再用 niri 拉起一个新的，并确认它起来了
fn restart_hypridle() -> io::Result<()> {
    let _ = Command::new("pkill").args(["-x", "hypridle"]).status();
    for _ in 0..40 {
        if !hypridle_running() {
            break;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    Command::new("niri")
        .args(["msg", "action", "spawn", "--", "hypridle"])
        .status()?;
    for _ in 0..40 {
        if hypridle_running() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    Err(io::Error::other("hypridle 未能重新启动"))
}

fn hypridle_running() -> bool {
    Command::new("pgrep")
        .args(["-x", "hypridle"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

// ==================== 界面状态 ====================

/// 后台线程回传的状态消息
enum Msg {
    Info(String),
    Error(String),
}

struct App {
    /// 亮度后端；None 表示没找到可用的 DDC/CI 显示器
    brightness: Option<Brightness>,
    bright_cur: u32,
    bright_max: u32,
    screen_off_min: u32,
    sleep_min: u32,
    /// 当前焦点：0 亮度 / 1 熄屏 / 2 睡眠
    focus: usize,
    /// 正在拖动哪条滑条
    dragging: Option<usize>,
    /// 三条滑条的条形区域，用于鼠标命中
    slider_rects: [Rect; 3],

    bright_tx: Sender<u32>,
    bright_dirty: Option<Instant>,
    idle_tx: Sender<(u32, u32)>,
    idle_dirty: Option<Instant>,

    status: String,
    status_is_error: bool,
    status_rx: Receiver<Msg>,
}

impl App {
    fn new() -> App {
        let brightness = Brightness::detect();
        let (bright_cur, bright_max) = brightness
            .as_ref()
            .and_then(Brightness::read)
            .unwrap_or((0, 100));

        let idle = IdleConfig::new();
        let idle_text = idle.load();
        let screen_off_min = idle_text
            .as_deref()
            .and_then(|t| read_listener_secs(t, "power-off-monitors"))
            .map(|s| s / 60)
            .unwrap_or(10);
        let sleep_min = idle_text
            .as_deref()
            .and_then(|t| read_listener_secs(t, "systemctl suspend"))
            .map(|s| s / 60)
            .unwrap_or(30);

        let (status_tx, status_rx) = mpsc::channel();

        // 亮度工作线程：合并只保留最后一次目标值，避免拖动时排队
        let (bright_tx, bright_rx) = mpsc::channel::<u32>();
        let bright_status = status_tx.clone();
        std::thread::spawn(move || {
            while let Ok(first) = bright_rx.recv() {
                let mut latest = first;
                while let Ok(v) = bright_rx.try_recv() {
                    latest = v;
                }
                let Some(backend) = brightness else {
                    continue;
                };
                let msg = match backend.set(latest) {
                    Ok(()) => Msg::Info(format!("亮度已设为 {latest}%")),
                    Err(e) => Msg::Error(format!("亮度设置失败：{e}")),
                };
                let _ = bright_status.send(msg);
            }
        });

        // 熄屏/睡眠工作线程：写配置 + 重启 hypridle 比较慢，放到后台
        let (idle_tx, idle_rx) = mpsc::channel::<(u32, u32)>();
        let idle_status = status_tx;
        std::thread::spawn(move || {
            while let Ok(first) = idle_rx.recv() {
                let mut latest = first;
                while let Ok(v) = idle_rx.try_recv() {
                    latest = v;
                }
                let (screen, sleep) = latest;
                let msg = match idle.apply(screen * 60, sleep * 60) {
                    Ok(()) => Msg::Info(format!("已应用：熄屏 {screen} 分钟 · 睡眠 {sleep} 分钟")),
                    Err(e) => Msg::Error(format!("写入 hypridle 配置失败：{e}")),
                };
                let _ = idle_status.send(msg);
            }
        });

        let status = if brightness.is_none() {
            "未找到支持 DDC/CI 的显示器，亮度不可调".to_string()
        } else {
            String::new()
        };

        App {
            brightness,
            bright_cur,
            bright_max: bright_max.max(1),
            screen_off_min,
            sleep_min,
            focus: 0,
            dragging: None,
            slider_rects: [Rect::new(0, 0, 0, 0); 3],
            bright_tx,
            bright_dirty: None,
            idle_tx,
            idle_dirty: None,
            status,
            status_is_error: false,
            status_rx,
        }
    }

    /// 把后台线程的状态消息取出来显示
    fn poll_status(&mut self) {
        while let Ok(msg) = self.status_rx.try_recv() {
            match msg {
                Msg::Info(s) => {
                    self.status = s;
                    self.status_is_error = false;
                }
                Msg::Error(s) => {
                    self.status = s;
                    self.status_is_error = true;
                }
            }
        }
    }

    /// 防抖到期后，把改动真正写到系统
    fn flush_debounced(&mut self) {
        if self
            .bright_dirty
            .is_some_and(|t| t.elapsed() >= BRIGHT_DEBOUNCE)
        {
            self.bright_dirty = None;
            let _ = self.bright_tx.send(self.bright_cur);
        }
        if self
            .idle_dirty
            .is_some_and(|t| t.elapsed() >= TIME_DEBOUNCE)
        {
            self.idle_dirty = None;
            let _ = self.idle_tx.send((self.screen_off_min, self.sleep_min));
        }
    }

    fn adjust_focused(&mut self, delta: i64) {
        match self.focus {
            0 => self.adjust_brightness(delta),
            1 => {
                self.screen_off_min =
                    shift(self.screen_off_min, delta, SCREEN_OFF_MIN, SCREEN_OFF_MAX);
                self.idle_dirty = Some(Instant::now());
            }
            2 => {
                self.sleep_min = shift(self.sleep_min, delta, SLEEP_MIN, SLEEP_MAX);
                self.idle_dirty = Some(Instant::now());
            }
            _ => {}
        }
    }

    fn adjust_brightness(&mut self, delta: i64) {
        if self.brightness.is_none() {
            return;
        }
        self.bright_cur = shift(self.bright_cur, delta, 0, self.bright_max);
        self.bright_dirty = Some(Instant::now());
    }

    fn set_focused_extreme(&mut self, high: bool) {
        match self.focus {
            0 if self.brightness.is_some() => {
                self.bright_cur = if high { self.bright_max } else { 0 };
                self.bright_dirty = Some(Instant::now());
            }
            1 => {
                self.screen_off_min = if high { SCREEN_OFF_MAX } else { SCREEN_OFF_MIN };
                self.idle_dirty = Some(Instant::now());
            }
            2 => {
                self.sleep_min = if high { SLEEP_MAX } else { SLEEP_MIN };
                self.idle_dirty = Some(Instant::now());
            }
            _ => {}
        }
    }

    fn handle_key(&mut self, key: KeyEvent) -> bool {
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => return true,
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => return true,
            KeyCode::Tab | KeyCode::Down | KeyCode::Char('j') => self.focus = (self.focus + 1) % 3,
            KeyCode::BackTab | KeyCode::Up | KeyCode::Char('k') => {
                self.focus = (self.focus + 2) % 3
            }
            KeyCode::Left | KeyCode::Char('h') => self.adjust_focused(-1),
            KeyCode::Right | KeyCode::Char('l') => self.adjust_focused(1),
            KeyCode::PageUp => self.adjust_focused(-10),
            KeyCode::PageDown => self.adjust_focused(10),
            KeyCode::Home => self.set_focused_extreme(false),
            KeyCode::End => self.set_focused_extreme(true),
            _ => {}
        }
        false
    }

    fn handle_mouse(&mut self, mouse: MouseEvent) {
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some(idx) = self.slider_at(mouse.column, mouse.row) {
                    self.focus = idx;
                    self.dragging = Some(idx);
                    self.set_slider_from_x(idx, mouse.column);
                }
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                if let Some(idx) = self.dragging {
                    self.set_slider_from_x(idx, mouse.column);
                }
            }
            MouseEventKind::Up(MouseButton::Left) => self.dragging = None,
            _ => {}
        }
    }

    fn slider_at(&self, x: u16, y: u16) -> Option<usize> {
        self.slider_rects.iter().position(|r| {
            r.width > 0 && y >= r.y && y < r.y + r.height && x >= r.x && x < r.x + r.width
        })
    }

    fn set_slider_from_x(&mut self, idx: usize, x: u16) {
        let r = self.slider_rects[idx];
        if r.width == 0 {
            return;
        }
        let span = (r.width - 1).max(1) as f64;
        let rel = x.saturating_sub(r.x).min(r.width - 1) as f64;
        let frac = rel / span;
        match idx {
            0 if self.brightness.is_some() => {
                self.bright_cur = (frac * self.bright_max as f64).round() as u32;
                self.bright_dirty = Some(Instant::now());
            }
            1 => {
                self.screen_off_min = lerp(frac, SCREEN_OFF_MIN, SCREEN_OFF_MAX);
                self.idle_dirty = Some(Instant::now());
            }
            2 => {
                self.sleep_min = lerp(frac, SLEEP_MIN, SLEEP_MAX);
                self.idle_dirty = Some(Instant::now());
            }
            _ => {}
        }
    }

    fn draw(&mut self, frame: &mut Frame) {
        let area = frame.area();
        let [
            title_area,
            bright_area,
            screen_area,
            sleep_area,
            _spacer,
            footer_area,
        ] = Layout::vertical([
            Constraint::Length(2),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(0),
            Constraint::Length(2),
        ])
        .areas(area);

        let title = Line::from(vec![
            Span::raw("  "),
            Span::styled(
                "系统设置",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
        ]);
        frame.render_widget(Paragraph::new(title), title_area);

        let bright_frac = self.bright_cur as f64 / self.bright_max as f64;
        let bright_text = format!(
            "{}%",
            (self.bright_cur as f64 / self.bright_max as f64 * 100.0).round() as u32
        );
        self.slider_rects[0] = self.render_slider(
            frame,
            bright_area,
            0,
            "亮度",
            bright_frac,
            &bright_text,
            self.brightness.is_some(),
        );

        self.slider_rects[1] = self.render_slider(
            frame,
            screen_area,
            1,
            "熄屏时间",
            frac_of(self.screen_off_min, SCREEN_OFF_MIN, SCREEN_OFF_MAX),
            &fmt_minutes(self.screen_off_min),
            true,
        );

        self.slider_rects[2] = self.render_slider(
            frame,
            sleep_area,
            2,
            "睡眠时间",
            frac_of(self.sleep_min, SLEEP_MIN, SLEEP_MAX),
            &fmt_minutes(self.sleep_min),
            true,
        );

        let status_style = if self.status_is_error {
            Style::default().fg(Color::Red)
        } else {
            Style::default().fg(Color::Green)
        };
        let footer = Line::from(vec![
            Span::raw("  "),
            Span::styled(self.status.clone(), status_style),
        ]);
        let hints = Line::from(vec![
            Span::raw("  "),
            Span::styled(
                "[Tab/↑↓] 切换   [←→/hl] 调整   [PgUp/PgDn] ±10   [拖动] 鼠标   [q] 退出",
                Style::default().fg(Color::DarkGray),
            ),
        ]);
        frame.render_widget(Paragraph::new(vec![footer, hints]), footer_area);
    }

    /// 渲染一条滑条，返回条形区域供鼠标命中
    #[allow(clippy::too_many_arguments)]
    fn render_slider(
        &self,
        frame: &mut Frame,
        area: Rect,
        idx: usize,
        label: &str,
        frac: f64,
        value_text: &str,
        enabled: bool,
    ) -> Rect {
        let [label_area, bar_area, value_area] = Layout::horizontal([
            Constraint::Length(14),
            Constraint::Min(10),
            Constraint::Length(12),
        ])
        .areas(area);

        let focused = self.focus == idx;
        let style = if !enabled {
            Style::default().fg(Color::DarkGray)
        } else if focused {
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::Gray)
        };
        let label_line = Line::from(vec![
            Span::styled(if focused { "▸ " } else { "  " }, style),
            Span::styled(label.to_string(), style),
        ]);
        frame.render_widget(Paragraph::new(label_line), label_area);

        let width = bar_area.width as usize;
        let filled = ((frac.clamp(0.0, 1.0)) * width as f64).round() as usize;
        let mut spans = Vec::with_capacity(width);
        for i in 0..width {
            if !enabled {
                spans.push(Span::styled("─", Style::default().fg(Color::DarkGray)));
            } else if i < filled {
                let (r, g, b) = sunset_gradient(i as f64 / width.max(1) as f64 * 100.0);
                spans.push(Span::styled("━", Style::default().fg(Color::Rgb(r, g, b))));
            } else {
                spans.push(Span::styled("─", Style::default().fg(TRACK)));
            }
        }
        frame.render_widget(Paragraph::new(Line::from(spans)), bar_area);

        let value_line = Line::from(Span::styled(format!("  {value_text}"), style));
        frame.render_widget(Paragraph::new(value_line), value_area);

        bar_area
    }
}

fn run_app(app: &mut App, terminal: &mut ratatui::DefaultTerminal) -> io::Result<()> {
    loop {
        app.poll_status();
        app.flush_debounced();
        terminal.draw(|frame| app.draw(frame))?;

        if !event::poll(Duration::from_millis(50))? {
            continue;
        }
        match event::read()? {
            Event::Key(key) if key.kind == KeyEventKind::Press => {
                if app.handle_key(key) {
                    return Ok(());
                }
            }
            Event::Mouse(mouse) => app.handle_mouse(mouse),
            _ => {}
        }
    }
}

// ==================== 小工具 ====================

/// 把 `value` 平移 `delta` 并夹在 [min, max]
fn shift(value: u32, delta: i64, min: u32, max: u32) -> u32 {
    (value as i64 + delta).clamp(min as i64, max as i64) as u32
}

/// 按比例取 [min, max] 内的值
fn lerp(frac: f64, min: u32, max: u32) -> u32 {
    let v = min as f64 + frac.clamp(0.0, 1.0) * (max - min) as f64;
    (v.round() as u32).clamp(min, max)
}

fn frac_of(value: u32, min: u32, max: u32) -> f64 {
    if max <= min {
        0.0
    } else {
        (value.saturating_sub(min) as f64 / (max - min) as f64).clamp(0.0, 1.0)
    }
}

/// 分钟数排版：不足 1 小时显示“N 分钟”，否则“H小时M分”
fn fmt_minutes(minutes: u32) -> String {
    if minutes < 60 {
        format!("{minutes} 分钟")
    } else {
        let (h, m) = (minutes / 60, minutes % 60);
        if m == 0 {
            format!("{h} 小时")
        } else {
            format!("{h}小时{m}分")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ddc_bus_skips_invalid_display() {
        let text = "\
Invalid display
   I2C bus:          /dev/i2c-10
   DRM connector:    card1-eDP-1

Display 1
   I2C bus:          /dev/i2c-13
   DRM connector:    card2-DP-2
";
        assert_eq!(parse_ddc_bus(text), Some(13));
    }

    #[test]
    fn ddc_bus_none_when_only_invalid() {
        let text = "Invalid display\n   I2C bus: /dev/i2c-10\n";
        assert_eq!(parse_ddc_bus(text), None);
    }

    #[test]
    fn vcp_terse_parses_continuous_value() {
        assert_eq!(parse_vcp_terse("VCP 10 C 7 100\n"), Some((7, 100)));
        assert_eq!(parse_vcp_terse("noise\nVCP 10 C 42 100\n"), Some((42, 100)));
        assert_eq!(parse_vcp_terse("VCP 60 C 3\n"), None);
    }

    const SAMPLE: &str = "\
general {
    lock_cmd = pidof hyprlock || hyprlock
    before_sleep_cmd = loginctl lock-session
}

# 10 分钟无操作：触发锁屏
listener {
    timeout = 10000
    on-timeout = loginctl lock-session
}

# 30 分钟无操作：系统挂起休眠
listener {
    timeout = 20000
    on-timeout = systemctl suspend
}
";

    #[test]
    fn parse_listeners_classifies() {
        let lines: Vec<&str> = SAMPLE.lines().collect();
        let ls = parse_listeners(&lines);
        assert_eq!(ls.len(), 2);
        assert_eq!(ls[0].on_timeout, "loginctl lock-session");
        assert_eq!(ls[1].on_timeout, "systemctl suspend");
        assert_eq!(read_listener_secs(SAMPLE, "systemctl suspend"), Some(20000));
    }

    #[test]
    fn rewrite_updates_sleep_and_appends_screen_off() {
        let out = rewrite_idle_config(SAMPLE, 600, 1800);
        // 睡眠被改为 1800，注释保留
        assert!(out.contains("# 30 分钟无操作：系统挂起休眠"));
        assert!(out.contains("timeout = 1800"));
        assert!(!out.contains("timeout = 20000"));
        // 锁屏 listener 不受影响
        assert!(out.contains("timeout = 10000"));
        // 追加了熄屏 listener
        assert!(out.contains("power-off-monitors"));
        assert!(out.contains("power-on-monitors"));
        assert!(out.contains("timeout = 600"));
        assert_eq!(read_listener_secs(&out, "power-off-monitors"), Some(600));
        assert_eq!(read_listener_secs(&out, "systemctl suspend"), Some(1800));
    }

    #[test]
    fn rewrite_is_idempotent_and_updates_existing_screen_off() {
        let once = rewrite_idle_config(SAMPLE, 600, 1800);
        let twice = rewrite_idle_config(&once, 900, 1800);
        // 不再重复追加
        assert_eq!(twice.matches("power-off-monitors").count(), 1);
        assert_eq!(read_listener_secs(&twice, "power-off-monitors"), Some(900));
    }

    #[test]
    fn helpers_round_trip() {
        assert_eq!(shift(5, -10, 1, 240), 1);
        assert_eq!(shift(239, 10, 1, 240), 240);
        assert_eq!(lerp(0.0, 1, 240), 1);
        assert_eq!(lerp(1.0, 1, 240), 240);
        assert_eq!(fmt_minutes(45), "45 分钟");
        assert_eq!(fmt_minutes(120), "2 小时");
        assert_eq!(fmt_minutes(166), "2小时46分");
    }
}
