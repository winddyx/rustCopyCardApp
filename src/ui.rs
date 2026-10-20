//! 四行自适应终端界面：进度条、阶段与速率、当前文件、状态。
//!
//! 阶段行与状态行按固定列宽渲染成表格，数值增长不会移动列位置；
//! 终端宽度不足时按优先级丢弃整列，单元格内容超长时在单元格内截断。

use std::io::{self, IsTerminal, Write};
use std::time::{Duration, Instant};

use unicode_width::UnicodeWidthStr;

use crate::stats::{eta_seconds, human_bytes, human_duration, speed_parts, SpeedMeter, Stats};

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

/// 全程进度：总工作量为源清单字节数的两倍，复制完成占一半，校验完成占另一半。
///
/// 校验阶段两条流各读一遍，因此按已读字节的一半计入工作量。
pub fn overall_percent(phase: Phase, total_bytes: u64, written: u64, hashed: u64) -> f64 {
    if total_bytes == 0 {
        return match phase {
            Phase::Scan => 0.0,
            Phase::Commit => 0.5,
            Phase::Copy | Phase::Verify => 0.0,
        };
    }
    let denominator = (total_bytes as f64) * 2.0;
    let worked = match phase {
        Phase::Scan => 0.0,
        Phase::Copy => written as f64,
        Phase::Commit => total_bytes as f64,
        Phase::Verify => total_bytes as f64 + hashed as f64 / 2.0,
    };
    (worked / denominator).clamp(0.0, 1.0)
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

/// 单元格对齐方式。
#[derive(Clone, Copy)]
enum Align {
    Left,
    Right,
}

/// 固定宽度的表格单元格：列位置不随内容长度变化，内容超长时在单元格内截断。
struct Cell {
    text: String,
    width: usize,
    align: Align,
    /// 宽度不足时优先丢弃优先级低的列。
    priority: u8,
}

impl Cell {
    fn left(text: String, width: usize, priority: u8) -> Self {
        Self {
            text,
            width,
            align: Align::Left,
            priority,
        }
    }

    fn right(text: String, width: usize, priority: u8) -> Self {
        Self {
            text,
            width,
            align: Align::Right,
            priority,
        }
    }
}

/// 把单元格内容裁剪或补齐到固定显示宽度。
fn fit_cell(cell: &Cell) -> String {
    let truncated = truncate_end(&cell.text, cell.width);
    let pad = cell.width.saturating_sub(truncated.width());
    match cell.align {
        Align::Left => format!("{truncated}{}", " ".repeat(pad)),
        Align::Right => format!("{}{truncated}", " ".repeat(pad)),
    }
}

/// 依次拼接单元格；宽度不足时按优先级丢弃整列，仍超宽则整行截断。
fn build_row(mut cells: Vec<Cell>, max_width: usize) -> String {
    if cells.is_empty() {
        return String::new();
    }
    loop {
        let line = cells.iter().map(fit_cell).collect::<Vec<_>>().join("  ");
        if line.width() <= max_width || cells.len() == 1 {
            return truncate_end(&line, max_width);
        }
        let lowest = cells
            .iter()
            .enumerate()
            .min_by_key(|(_, cell)| cell.priority)
            .map(|(index, _)| index)
            .unwrap_or(cells.len() - 1);
        cells.remove(lowest);
    }
}

/// 纯函数渲染，便于单元测试。返回值固定为 4 行：
/// 进度条、阶段与速率（表格）、当前文件、状态（表格）。
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

    let line2 = render_progress_line(snapshot, width);

    let path_prefix = "> ";
    let available = width.saturating_sub(path_prefix.width());
    let line3 = format!(
        "{path_prefix}{}",
        truncate_start(&snapshot.current, available)
    );

    let line4 = render_status_line(snapshot, width);

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
    let counts = if snapshot.files_total > 0 {
        format!("{}/{} files", snapshot.files_done, snapshot.files_total)
    } else {
        format!("{} files", snapshot.files_done)
    };

    let mut cells = vec![
        Cell::left(snapshot.phase.label().to_string(), 4, 100),
        Cell::right(counts, 20, 80),
    ];

    if matches!(snapshot.phase, Phase::Copy | Phase::Verify) {
        cells.push(Cell::right(
            format!(
                "{}/{}",
                human_bytes(snapshot.bytes_done),
                human_bytes(snapshot.bytes_total)
            ),
            19,
            60,
        ));
    }

    // 速度只在复制与校验阶段有意义；写速只在复制阶段出现。
    if matches!(snapshot.phase, Phase::Copy | Phase::Verify) {
        let (read_value, read_unit) = speed_parts(snapshot.read_speed);
        cells.push(Cell::left(
            format!("R {read_value:>6.1} {read_unit:>3}/s"),
            14,
            40,
        ));
    }
    if snapshot.phase == Phase::Copy {
        let (write_value, write_unit) = speed_parts(snapshot.write_speed);
        cells.push(Cell::left(
            format!("W {write_value:>6.1} {write_unit:>3}/s"),
            14,
            30,
        ));
    }

    if let Some(eta) = snapshot.eta {
        cells.push(Cell::left(
            format!("ETA {:>8}", human_duration(eta)),
            12,
            20,
        ));
    }

    build_row(cells, width)
}

fn render_status_line(snapshot: &Snapshot, width: usize) -> String {
    let elapsed = format!("elapsed {:>9}", human_duration(snapshot.elapsed.as_secs()));

    let cells = match snapshot.phase {
        Phase::Verify => vec![
            Cell::left(format!("match {:>6}", snapshot.verify_ok), 12, 100),
            Cell::left(format!("mismatch {:>6}", snapshot.verify_bad), 15, 100),
            Cell::left(elapsed, 17, 60),
        ],
        _ => vec![
            Cell::left(
                format!(
                    "buf {:>4}/{:>4} MiB",
                    snapshot.buffer_used / (1024 * 1024),
                    snapshot.buffer_bytes / (1024 * 1024)
                ),
                17,
                40,
            ),
            Cell::left(format!("ok {:>6}", snapshot.ok), 9, 100),
            Cell::left(format!("fail {:>6}", snapshot.fail), 11, 100),
            Cell::left(format!("skip {:>6}", snapshot.skip), 11, 80),
            Cell::left(elapsed, 17, 60),
        ],
    };

    build_row(cells, width)
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
    /// 源清单总字节数，全程不变；总进度以 2 倍它作为分母。
    total_bytes: u64,
    files_total: u64,
    files_done: u64,
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
            total_bytes: 0,
            files_total: 0,
            files_done: 0,
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

    /// 源清单总字节数，进入阶段 2 之前设置一次。
    pub fn set_total_bytes(&mut self, total_bytes: u64) {
        self.total_bytes = total_bytes;
    }

    pub fn set_phase(&mut self, phase: Phase, files_total: u64) {
        self.phase = phase;
        self.files_total = files_total;
        self.files_done = 0;
        self.last_draw = Instant::now() - Duration::from_secs(1);
    }

    /// 阶段 1 与阶段 3 的完成量由调用方直接推进（不经过统计计数）。
    pub fn set_progress(&mut self, files_done: u64) {
        self.files_done = files_done;
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

        let written = stats
            .written_bytes
            .load(std::sync::atomic::Ordering::Relaxed);
        let hashed = stats
            .hashed_bytes
            .load(std::sync::atomic::Ordering::Relaxed);
        let total_bytes = self.total_bytes;

        let files_done = match self.phase {
            Phase::Copy => {
                stats.files_ok.load(std::sync::atomic::Ordering::Relaxed) as u64
                    + stats
                        .files_failed
                        .load(std::sync::atomic::Ordering::Relaxed) as u64
            }
            Phase::Verify => {
                stats.verify_ok.load(std::sync::atomic::Ordering::Relaxed) as u64
                    + stats.verify_bad.load(std::sync::atomic::Ordering::Relaxed) as u64
            }
            Phase::Scan | Phase::Commit => self.files_done,
        };

        // 阶段行展示的字节进度：复制看已写字节，校验看单侧已读字节（双方读取各计一半）。
        let (bytes_done, bytes_total) = match self.phase {
            Phase::Copy => (written, total_bytes),
            Phase::Verify => (hashed / 2, total_bytes),
            Phase::Scan | Phase::Commit => (0, total_bytes),
        };

        let percent = overall_percent(self.phase, total_bytes, written, hashed);

        // ETA 按当前阶段的剩余工作量与对应方向的速度估算。
        let eta = match self.phase {
            Phase::Copy => eta_seconds(total_bytes.saturating_sub(written), write_speed),
            Phase::Verify => eta_seconds(
                total_bytes.saturating_mul(2).saturating_sub(hashed),
                read_speed,
            ),
            Phase::Scan | Phase::Commit => None,
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
    fn overall_progress_splits_copy_and_verify() {
        let total = 1000u64;
        assert_eq!(overall_percent(Phase::Scan, total, 0, 0), 0.0);
        assert_eq!(overall_percent(Phase::Copy, total, 0, 0), 0.0);
        assert_eq!(overall_percent(Phase::Copy, total, 500, 0), 0.25);
        assert_eq!(overall_percent(Phase::Copy, total, total, 0), 0.5);
        assert_eq!(overall_percent(Phase::Commit, total, total, 0), 0.5);
        // 校验阶段两侧各读一遍，按已读字节的一半计入。
        assert_eq!(overall_percent(Phase::Verify, total, total, total), 0.75);
        assert_eq!(overall_percent(Phase::Verify, total, total, total * 2), 1.0);
        // 没有文件时不应出现除零或超过 1 的进度。
        assert_eq!(overall_percent(Phase::Copy, 0, 0, 0), 0.0);
    }

    #[test]
    fn renders_four_lines_with_status_after_current_file() {
        let lines = render(&snapshot(Phase::Copy, 0.66), 120);
        assert_eq!(lines.len(), 4);
        assert!(lines[0].contains("66.0%"));
        assert!(lines[1].starts_with("COPY"));
        assert!(
            lines[2].starts_with("> 100MEDIA/"),
            "当前文件应在状态行之上"
        );
        assert!(lines[3].contains("buf"), "状态行应在当前文件之下");
    }

    #[test]
    fn status_columns_do_not_shift_when_counters_grow() {
        let mut small = snapshot(Phase::Copy, 0.5);
        small.ok = 9;
        small.files_done = 9;
        let mut large = small.clone_shallow();
        large.ok = 123456;
        large.files_done = 123456;
        large.files_total = 654321;
        large.elapsed = Duration::from_secs(3 * 3600 + 65);

        let small_lines = render(&small, 120);
        let large_lines = render(&large, 120);

        assert_eq!(
            small_lines[3].width(),
            large_lines[3].width(),
            "状态行宽度应固定：{small_lines:?} 与 {large_lines:?}"
        );
        let small_pos = small_lines[3].find("fail").unwrap();
        let large_pos = large_lines[3].find("fail").unwrap();
        assert_eq!(small_pos, large_pos, "fail 列位置不应移动");
        assert_eq!(
            small_lines[1].width(),
            large_lines[1].width(),
            "阶段行宽度应固定"
        );
    }

    #[test]
    fn cells_are_truncated_when_content_exceeds_width() {
        let mut snap = snapshot(Phase::Copy, 0.5);
        snap.ok = u64::MAX;
        snap.buffer_used = u64::MAX;
        let lines = render(&snap, 120);
        assert!(
            lines[3].contains('…') || lines[3].width() <= 120,
            "超长单元格应被截断：{}",
            lines[3]
        );
        for line in &lines {
            assert!(line.width() <= 120);
        }
    }

    #[test]
    fn degrades_columns_as_width_shrinks() {
        let wide = render(&snapshot(Phase::Copy, 0.5), 120).join("\n");
        assert!(wide.contains("ETA"));
        assert!(wide.contains("W "));
        assert!(wide.contains("GiB"));

        let mid = render(&snapshot(Phase::Copy, 0.5), 90).join("\n");
        assert!(!mid.contains("ETA"), "宽度不足时先丢 ETA：{mid}");

        let narrow = render(&snapshot(Phase::Copy, 0.5), 70).join("\n");
        assert!(!narrow.contains("W "), "继续丢写速：{narrow}");

        let tighter = render(&snapshot(Phase::Copy, 0.5), 55).join("\n");
        assert!(!tighter.contains("R "), "再丢读速：{tighter}");

        let tiny = render(&snapshot(Phase::Copy, 0.5), 30).join("\n");
        assert!(tiny.contains('%'));
        assert!(!tiny.contains("buf"));
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
        assert!(lines[2].width() <= 40);
        assert!(lines[2].ends_with(".MOV"));
    }

    #[test]
    fn full_percent_has_no_cursor_marker() {
        let lines = render(&snapshot(Phase::Copy, 1.0), 100);
        assert!(!lines[0].contains('>'));
    }

    impl Snapshot {
        fn clone_shallow(&self) -> Snapshot {
            Snapshot {
                phase: self.phase,
                percent: self.percent,
                files_done: self.files_done,
                files_total: self.files_total,
                bytes_done: self.bytes_done,
                bytes_total: self.bytes_total,
                read_speed: self.read_speed,
                write_speed: self.write_speed,
                eta: self.eta,
                ok: self.ok,
                fail: self.fail,
                skip: self.skip,
                elapsed: self.elapsed,
                buffer_used: self.buffer_used,
                buffer_bytes: self.buffer_bytes,
                current: self.current.clone(),
                verify_ok: self.verify_ok,
                verify_bad: self.verify_bad,
            }
        }
    }
}
