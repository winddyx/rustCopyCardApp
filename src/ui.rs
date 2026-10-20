//! 四行自适应终端界面：进度条、阶段与计数、状态行、当前文件。
//!
//! 宽度不足时按固定优先级丢弃字段；非 tty 或 `--quiet` 时不绘制进度条。

use std::io::{self, IsTerminal, Write};
use std::time::{Duration, Instant};

use unicode_width::UnicodeWidthStr;

use crate::stats::{eta_seconds, human_bytes, human_duration, human_speed, SpeedMeter, Stats};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Scan,
    Copy,
    Commit,
    Verify,
}

impl Phase {
    pub fn label(self) -> &'static str {
        match self {
            Phase::Scan => "SCAN",
            Phase::Copy => "COPY",
            Phase::Commit => "CHCK",
            Phase::Verify => "VRFY",
        }
    }
}

/// 一次绘制所需的全部信息。
pub struct Snapshot {
    pub phase: Phase,
    pub percent: f64,
    pub files_done: u64,
    pub files_total: u64,
    pub bytes_done: u64,
    pub bytes_total: u64,
    pub read_speed: f64,
    pub write_speed: f64,
    pub eta: Option<u64>,
    pub ok: u64,
    pub fail: u64,
    pub skip: u64,
    pub elapsed: Duration,
    pub buffer_used: u64,
    pub buffer_bytes: u64,
    pub current: String,
    pub verify_ok: u64,
    pub verify_bad: u64,
}

/// 纯函数渲染，便于单元测试。返回值固定为 4 行。
pub fn render(snapshot: &Snapshot, width: usize) -> Vec<String> {
    let width = width.max(20);
    let percent = snapshot.percent.clamp(0.0, 1.0);
    let bar_width = bar_width(width);
    let filled = (bar_width as f64 * percent).round() as usize;

    let mut bar = String::with_capacity(bar_width + 2);
    for i in 0..bar_width {
        if i < filled {
            bar.push('=');
        } else if i == filled {
            bar.push('>');
        } else {
            bar.push('-');
        }
    }
    let line1 = truncate_end(&format!("[{bar}] {:>5.1}%", percent * 100.0), width);

    let line2 = truncate_end(&render_progress_line(snapshot, width), width);
    let line3 = truncate_end(&render_status_line(snapshot, width), width);

    let path_prefix = "> ";
    let available = width.saturating_sub(path_prefix.width());
    let line4 = format!(
        "{path_prefix}{}",
        truncate_start(&snapshot.current, available)
    );

    vec![line1, line2, line3, line4]
}

fn bar_width(width: usize) -> usize {
    if width >= 88 {
        60
    } else {
        // 行内容为 "[" + bar + "] " + " 50.0%" 共 9 个额外字符。
        width.saturating_sub(9).max(10)
    }
}

fn render_progress_line(snapshot: &Snapshot, width: usize) -> String {
    let phase = snapshot.phase.label();
    let counts = if snapshot.files_total > 0 {
        format!("{}/{} files", snapshot.files_done, snapshot.files_total)
    } else {
        format!("{} files", snapshot.files_done)
    };
    let has_bytes = matches!(snapshot.phase, Phase::Copy | Phase::Verify);
    let bytes = format!(
        "{}/{}",
        human_bytes(snapshot.bytes_done),
        human_bytes(snapshot.bytes_total)
    );
    let read = format!("R {}", human_speed(snapshot.read_speed));
    let write = format!("W {}", human_speed(snapshot.write_speed));
    let eta = snapshot
        .eta
        .map(|s| format!("ETA {}", human_duration(s)))
        .unwrap_or_default();

    let mut parts: Vec<String> = vec![phase.to_string()];
    if width >= 44 {
        parts.push(counts);
    }
    if width >= 60 && has_bytes {
        parts.push(bytes);
    }
    if width >= 72 {
        parts.push(read);
    }
    if width >= 90 {
        parts.push(write);
    }
    if width >= 110 && !eta.is_empty() {
        parts.push(eta);
    }
    parts.join("  ")
}

fn render_status_line(snapshot: &Snapshot, width: usize) -> String {
    let elapsed = format!("elapsed {}", human_duration(snapshot.elapsed.as_secs()));
    match snapshot.phase {
        Phase::Verify => {
            let mut parts = vec![
                format!("match {}", snapshot.verify_ok),
                format!("mismatch {}", snapshot.verify_bad),
            ];
            if width >= 60 {
                parts.push(elapsed);
            }
            parts.join("  ")
        }
        _ => {
            let mut parts = Vec::new();
            if width >= 72 {
                parts.push(format!(
                    "buf {}/{}",
                    human_bytes(snapshot.buffer_used),
                    human_bytes(snapshot.buffer_bytes)
                ));
            }
            parts.push(format!("ok {}", snapshot.ok));
            parts.push(format!("fail {}", snapshot.fail));
            parts.push(format!("skip {}", snapshot.skip));
            if width >= 60 {
                parts.push(elapsed);
            }
            parts.join("  ")
        }
    }
}

fn truncate_end(s: &str, max_width: usize) -> String {
    if s.width() <= max_width {
        return s.to_string();
    }
    let mut out = String::new();
    let mut used = 0;
    for ch in s.chars() {
        let w = ch.to_string().width();
        if used + w > max_width.saturating_sub(1) {
            break;
        }
        used += w;
        out.push(ch);
    }
    out.push('…');
    out
}

/// 保留尾部（文件名与末级目录更重要），左侧补省略号。
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

/// 一次运行的界面状态与绘制器。
pub struct Ui {
    painter: Painter,
    read_meter: SpeedMeter,
    write_meter: SpeedMeter,
    started: Instant,
    last_draw: Instant,
    phase: Phase,
    files_total: u64,
    files_done: u64,
    bytes_total: u64,
    bytes_done: u64,
    buffer_bytes: u64,
    quiet: bool,
    verbose: bool,
    enabled: bool,
    last_current: String,
}

impl Ui {
    pub fn new(quiet: bool, verbose: bool) -> Self {
        let interactive = io::stdout().is_terminal() && !quiet;
        let color = interactive && std::env::var_os("NO_COLOR").is_none();
        Self {
            painter: Painter::new(interactive, color),
            read_meter: SpeedMeter::new(),
            write_meter: SpeedMeter::new(),
            started: Instant::now(),
            last_draw: Instant::now() - Duration::from_secs(1),
            phase: Phase::Scan,
            files_total: 0,
            files_done: 0,
            bytes_total: 0,
            bytes_done: 0,
            buffer_bytes: 0,
            quiet,
            verbose,
            enabled: interactive,
            last_current: String::new(),
        }
    }

    pub fn interactive(&self) -> bool {
        self.enabled
    }

    pub fn verbose(&self) -> bool {
        self.verbose
    }

    pub fn quiet(&self) -> bool {
        self.quiet
    }

    pub fn started(&self) -> Instant {
        self.started
    }

    pub fn set_phase(&mut self, phase: Phase, files_total: u64, bytes_total: u64) {
        self.phase = phase;
        self.files_total = files_total;
        self.bytes_total = bytes_total;
        self.files_done = 0;
        self.bytes_done = 0;
        self.last_draw = Instant::now() - Duration::from_secs(1);
    }

    /// 阶段 1 与阶段 3 的完成量由调用方直接推进（不经过统计计数）。
    pub fn set_progress(&mut self, files_done: u64, bytes_done: u64) {
        self.files_done = files_done;
        self.bytes_done = bytes_done;
    }

    pub fn set_buffer_bytes(&mut self, bytes: u64) {
        self.buffer_bytes = bytes;
    }

    /// 刷新界面（内部按 10 Hz 节流，非交互模式直接返回）。
    pub fn tick(&mut self, stats: &Stats) {
        self.tick_inner(stats, false);
    }

    /// 强制重绘（阶段结束、终态展示时使用）。
    pub fn tick_force(&mut self, stats: &Stats) {
        self.tick_inner(stats, true);
    }

    fn tick_inner(&mut self, stats: &Stats, force_draw: bool) {
        if !self.enabled {
            return;
        }
        let now = Instant::now();
        let force = force_draw || stats.current() != self.last_current;
        if !force && now.duration_since(self.last_draw) < Duration::from_millis(100) {
            return;
        }
        self.last_draw = now;
        self.last_current = stats.current();

        let read_speed = self.read_meter.sample(
            stats.read_bytes.load(std::sync::atomic::Ordering::Relaxed),
            now,
        );
        let write_speed = self.write_meter.sample(
            stats
                .written_bytes
                .load(std::sync::atomic::Ordering::Relaxed),
            now,
        );

        let (bytes_done, bytes_total, files_done) = match self.phase {
            Phase::Copy => (
                stats
                    .written_bytes
                    .load(std::sync::atomic::Ordering::Relaxed),
                self.bytes_total,
                stats.files_ok.load(std::sync::atomic::Ordering::Relaxed) as u64
                    + stats
                        .files_failed
                        .load(std::sync::atomic::Ordering::Relaxed) as u64,
            ),
            Phase::Verify => (
                stats
                    .hashed_bytes
                    .load(std::sync::atomic::Ordering::Relaxed),
                self.bytes_total,
                stats.verify_ok.load(std::sync::atomic::Ordering::Relaxed) as u64
                    + stats.verify_bad.load(std::sync::atomic::Ordering::Relaxed) as u64,
            ),
            Phase::Scan | Phase::Commit => (self.bytes_done, self.bytes_total, self.files_done),
        };

        let percent = match self.phase {
            Phase::Scan | Phase::Commit => {
                if self.files_total > 0 {
                    files_done as f64 / self.files_total as f64
                } else {
                    0.0
                }
            }
            _ => {
                if bytes_total > 0 {
                    bytes_done as f64 / bytes_total as f64
                } else {
                    1.0
                }
            }
        };

        let eta = if matches!(self.phase, Phase::Copy | Phase::Verify) {
            eta_seconds(bytes_total.saturating_sub(bytes_done), write_speed)
        } else {
            None
        };

        let snapshot = Snapshot {
            phase: self.phase,
            percent,
            files_done: if self.files_total > 0 {
                files_done.min(self.files_total)
            } else {
                files_done
            },
            files_total: self.files_total,
            bytes_done,
            bytes_total,
            read_speed,
            write_speed,
            eta,
            ok: stats.files_ok.load(std::sync::atomic::Ordering::Relaxed) as u64,
            fail: stats
                .files_failed
                .load(std::sync::atomic::Ordering::Relaxed) as u64,
            skip: stats
                .files_skipped
                .load(std::sync::atomic::Ordering::Relaxed) as u64,
            elapsed: now.duration_since(self.started),
            buffer_used: stats
                .in_flight_bytes
                .load(std::sync::atomic::Ordering::Relaxed),
            buffer_bytes: self.buffer_bytes,
            current: stats.current(),
            verify_ok: stats.verify_ok.load(std::sync::atomic::Ordering::Relaxed) as u64,
            verify_bad: stats.verify_bad.load(std::sync::atomic::Ordering::Relaxed) as u64,
        };

        let width = terminal_width().unwrap_or(80);
        self.painter.draw(&render(&snapshot, width));
    }

    pub fn finish(&mut self) {
        self.painter.finish();
    }

    pub fn color(&self) -> bool {
        self.painter.color
    }
}

fn terminal_width() -> Option<usize> {
    terminal_size::terminal_size().map(|(w, _)| w.0 as usize)
}

struct Painter {
    enabled: bool,
    color: bool,
    drawn: usize,
}

impl Painter {
    fn new(enabled: bool, color: bool) -> Self {
        Self {
            enabled,
            color,
            drawn: 0,
        }
    }

    fn draw(&mut self, lines: &[String]) {
        if !self.enabled {
            return;
        }
        let mut out = io::stdout().lock();
        if self.drawn == 0 {
            let _ = write!(out, "\x1b[?25l");
        } else {
            let _ = write!(out, "\x1b[{}A", self.drawn);
        }
        for line in lines {
            let _ = write!(out, "\r\x1b[2K{line}\n");
        }
        let _ = out.flush();
        self.drawn = lines.len();
    }

    fn finish(&mut self) {
        if !self.enabled || self.drawn == 0 {
            return;
        }
        let mut out = io::stdout().lock();
        let _ = write!(out, "\x1b[{}A", self.drawn);
        for _ in 0..self.drawn {
            let _ = write!(out, "\r\x1b[2K\n");
        }
        let _ = write!(out, "\x1b[{}A\x1b[?25h", self.drawn);
        let _ = out.flush();
        self.drawn = 0;
        let _ = self.color;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(phase: Phase, percent: f64) -> Snapshot {
        Snapshot {
            phase,
            percent,
            files_done: 12,
            files_total: 24,
            bytes_done: 42 * 1024 * 1024 * 1024,
            bytes_total: 96 * 1024 * 1024 * 1024,
            read_speed: 410.0 * 1024.0 * 1024.0,
            write_speed: 385.0 * 1024.0 * 1024.0,
            eta: Some(52),
            ok: 12,
            fail: 0,
            skip: 0,
            elapsed: Duration::from_secs(14),
            buffer_used: 42 * 1024 * 1024,
            buffer_bytes: 128 * 1024 * 1024,
            current: "100MEDIA/camera_A001.MOV".to_string(),
            verify_ok: 8,
            verify_bad: 0,
        }
    }

    #[test]
    fn renders_exactly_four_lines() {
        let lines = render(&snapshot(Phase::Copy, 0.66), 120);
        assert_eq!(lines.len(), 4);
        assert!(lines[0].contains("66.0%"));
        assert!(lines[1].starts_with("COPY"));
        assert!(lines[2].contains("buf"));
        assert!(lines[3].starts_with("> 100MEDIA/"));
    }

    #[test]
    fn degrades_fields_as_width_shrinks() {
        let wide = render(&snapshot(Phase::Copy, 0.5), 120).join("\n");
        assert!(wide.contains("ETA"));
        assert!(wide.contains("W "));
        assert!(wide.contains("GiB"));

        let mid = render(&snapshot(Phase::Copy, 0.5), 95).join("\n");
        assert!(!mid.contains("ETA"));

        let narrow = render(&snapshot(Phase::Copy, 0.5), 65).join("\n");
        assert!(!narrow.contains("R "));

        let tiny = render(&snapshot(Phase::Copy, 0.5), 30).join("\n");
        assert!(!tiny.contains("GiB"));
        assert!(tiny.contains('%'));
    }

    #[test]
    fn every_line_fits_the_width() {
        for width in [20usize, 30, 44, 60, 72, 90, 110, 200] {
            let lines = render(&snapshot(Phase::Copy, 0.42), width);
            for line in lines {
                assert!(
                    line.width() <= width,
                    "宽度 {width} 时行宽 {} 超出：{line}",
                    line.width()
                );
            }
        }
    }

    #[test]
    fn cjk_paths_are_truncated_by_display_width() {
        let mut snap = snapshot(Phase::Copy, 0.1);
        snap.current = "素材目录/第一期/相机A_0001.MOV".repeat(4);
        let lines = render(&snap, 40);
        assert!(lines[3].width() <= 40);
        assert!(lines[3].ends_with(".MOV"));
    }

    #[test]
    fn full_percent_has_no_cursor_marker() {
        let lines = render(&snapshot(Phase::Copy, 1.0), 100);
        assert!(!lines[0].contains('>'));
    }
}
