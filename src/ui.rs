//! 终端上的单行进度：只显示阶段与当前文件，不画进度条。
//!
//! 复制与校验各用一条同款的行，原地刷新。进度写 stderr，且只在 stderr 是 TTY 时绘制；
//! stdout 只保留汇总。

use std::io::{self, IsTerminal, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use unicode_width::UnicodeWidthStr;

/// 界面上出现的阶段，标签同时用于对齐（两行标签宽度一致，文件名列不会跳）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Copy,
    Verify,
}

impl Phase {
    pub fn label(self) -> &'static str {
        match self {
            Phase::Copy => "COPY",
            Phase::Verify => "VERIFY",
        }
    }
}

/// 标签占用的列宽，两行都按它补空格。
const LABEL_W: usize = 6;
const SEP: usize = 2;

/// 绘制风格：是否上色。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Style {
    pub color: bool,
}

impl Style {
    /// 纯文本（测试与管道输出使用）。
    pub const PLAIN: Self = Self { color: false };
}

const SGR_DIM: &str = "2";

/// 一行进度：`阶段  当前文件`。文件名超宽时从头截断，保留尾部。
pub fn render_line(label: &str, current: &str, width: usize, style: Style) -> String {
    let available = width.saturating_sub(LABEL_W + SEP);
    let shown = truncate_start(current, available);
    format!(
        "{}{}{}",
        paint(&truncate_end(label, LABEL_W), SGR_DIM, style),
        " ".repeat(LABEL_W + SEP - truncate_end(label, LABEL_W).width()),
        shown
    )
}

fn paint(text: &str, code: &str, style: Style) -> String {
    if !style.color || text.is_empty() {
        return text.to_string();
    }
    format!("\x1b[{code}m{text}\x1b[0m")
}

fn truncate_end(s: &str, max_width: usize) -> String {
    if s.width() <= max_width {
        return s.to_string();
    }
    if max_width <= 1 {
        return "…".to_string();
    }
    let mut out = String::new();
    let mut used = 0;
    for ch in s.chars() {
        let w = ch.to_string().width();
        if used + w > max_width - 1 {
            break;
        }
        used += w;
        out.push(ch);
    }
    out.push('…');
    out
}

fn truncate_start(s: &str, max_width: usize) -> String {
    if s.width() <= max_width {
        return s.to_string();
    }
    if max_width <= 3 {
        return "…".to_string();
    }
    let target = max_width - 1;
    let mut tail: Vec<char> = Vec::new();
    let mut used = 0;
    for ch in s.chars().rev() {
        let w = ch.to_string().width();
        if used + w > target {
            break;
        }
        used += w;
        tail.push(ch);
    }
    tail.reverse();
    format!("…{}", tail.into_iter().collect::<String>())
}

/// 当前处理中的文件：由复制回调或校验循环写入，界面线程读取。
#[derive(Clone, Default)]
pub struct ProgressSlot(Arc<(Mutex<Option<String>>, Condvar)>);

impl ProgressSlot {
    /// 记录当前文件名，并立刻唤醒界面线程（不必等到下一次轮询）。
    pub fn set(&self, name: impl Into<String>) {
        let (lock, cvar) = &*self.0;
        if let Ok(mut slot) = lock.lock() {
            *slot = Some(name.into());
        }
        cvar.notify_all();
    }

    fn notify(&self) {
        self.0 .1.notify_all();
    }

    fn current(&self) -> Option<String> {
        self.0 .0.lock().ok().and_then(|slot| slot.clone())
    }

    /// 最多等待 `timeout`，期间若有新文件名写入会提前返回。
    fn wait(&self, timeout: Duration) {
        let (lock, cvar) = &*self.0;
        if let Ok(guard) = lock.lock() {
            let _ = cvar.wait_timeout(guard, timeout);
        }
    }
}

/// 单行进度的绘制器：后台线程把当前文件画在同一行，结束时擦掉。
pub struct ProgressLine {
    slot: ProgressSlot,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl ProgressLine {
    /// `enabled` 为假（stderr 非 TTY 或 --quiet）时不绘制，也不开线程。
    pub fn start(label: &'static str, slot: ProgressSlot, enabled: bool, style: Style) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let handle = if enabled {
            let stop_flag = Arc::clone(&stop);
            let ticker_slot = slot.clone();
            Some(std::thread::spawn(move || {
                let mut drawn = false;
                let mut last: Option<String> = None;
                while !stop_flag.load(Ordering::Relaxed) {
                    if let Some(name) = ticker_slot.current() {
                        if last.as_deref() != Some(name.as_str()) {
                            let width = terminal_width().unwrap_or(80);
                            let line = render_line(label, &name, width, style);
                            let mut out = io::stderr().lock();
                            let _ = write!(out, "\r\x1b[2K{line}");
                            let _ = out.flush();
                            drawn = true;
                            last = Some(name);
                        }
                    }
                    ticker_slot.wait(Duration::from_millis(100));
                }
                if drawn {
                    let mut out = io::stderr().lock();
                    let _ = write!(out, "\r\x1b[2K");
                    let _ = out.flush();
                }
            }))
        } else {
            None
        };
        Self { slot, stop, handle }
    }

    /// 停止刷新并擦掉这一行；阶段结束立刻调用，避免与下一阶段的输出抢同一行。
    pub fn finish(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.slot.notify();
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for ProgressLine {
    fn drop(&mut self) {
        self.finish();
    }
}

fn terminal_width() -> Option<usize> {
    terminal_size::terminal_size().map(|(w, _)| w.0 as usize)
}

/// 界面是否启用：stderr 是 TTY 且未 --quiet。
pub fn interactive(quiet: bool) -> bool {
    io::stderr().is_terminal() && !quiet
}

/// 颜色策略：显式开关优先，其次 NO_COLOR、TERM=dumb 与非 TTY。
pub fn color_enabled(choice: crate::cli::ColorChoice) -> bool {
    use crate::cli::ColorChoice;
    match choice {
        ColorChoice::Always => true,
        ColorChoice::Never => false,
        ColorChoice::Auto => {
            if std::env::var_os("NO_COLOR").is_some() {
                return false;
            }
            if matches!(std::env::var("TERM").as_deref(), Ok("dumb")) {
                return false;
            }
            io::stderr().is_terminal()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_shows_label_and_current_file() {
        let line = render_line(
            Phase::Copy.label(),
            "A001_06141325_C026.mov",
            60,
            Style::PLAIN,
        );
        assert!(line.starts_with("COPY  "), "{line}");
        assert!(line.ends_with("A001_06141325_C026.mov"), "{line}");
        assert!(line.width() <= 60, "{line}");
    }

    #[test]
    fn both_phases_align_the_file_name() {
        let copy = render_line(Phase::Copy.label(), "x.mov", 40, Style::PLAIN);
        let verify = render_line(Phase::Verify.label(), "x.mov", 40, Style::PLAIN);
        let column = |line: &str| line[..line.find('x').unwrap()].width();
        assert_eq!(column(&copy), column(&verify), "文件名列应对齐");
        assert!(verify.starts_with("VERIFY"), "{verify}");
    }

    #[test]
    fn long_names_keep_their_tail() {
        let line = render_line(
            Phase::Verify.label(),
            "素材目录_第一期_相机A_0001.MOV",
            24,
            Style::PLAIN,
        );
        assert!(line.width() <= 24, "{line}");
        assert!(line.ends_with("_0001.MOV"), "保留扩展名：{line}");
    }

    #[test]
    fn every_line_fits_the_width() {
        for width in [12usize, 20, 30, 60, 120, 200] {
            let line = render_line(
                Phase::Verify.label(),
                "A001_06141325_C026.mov",
                width,
                Style::PLAIN,
            );
            assert!(line.width() <= width, "宽度 {width} 时超宽：{line}");
        }
    }

    #[test]
    fn color_style_emits_sgr_codes() {
        let plain = render_line(Phase::Copy.label(), "x.mov", 40, Style::PLAIN);
        assert!(!plain.contains('\x1b'), "纯文本模式不应带 SGR");

        let colored = render_line(Phase::Copy.label(), "x.mov", 40, Style { color: true });
        assert!(colored.contains("\x1b[2m"), "标签用暗色");
        assert!(colored.ends_with("x.mov"), "{colored}");
    }

    #[test]
    fn slot_hands_the_name_to_the_renderer() {
        let slot = ProgressSlot::default();
        assert!(slot.current().is_none());
        slot.set("A.mov");
        assert_eq!(slot.current().as_deref(), Some("A.mov"));
        slot.set(String::from("B.mov"));
        assert_eq!(slot.current().as_deref(), Some("B.mov"));
    }
}
