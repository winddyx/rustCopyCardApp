//! 四行自适应终端界面：百分比与当前文件、校验计数与时间、阶段与进度、速率。
//!
//! 复制阶段由内嵌的 sy 直接输出，本界面只在校验阶段绘制。
//!
//! 各行按固定列宽渲染成表格，数值增长不会移动列位置；
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
            // 复制阶段交给 sy 后没有可用的字节计数；校验一开始就占满后半程。
            Phase::Copy => 0.0,
            Phase::Verify => 0.5,
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
    pub eta: Option<u64>,
    pub elapsed: Duration,
    pub current: String,
    pub verify_ok: u64,
    pub verify_bad: u64,
}

/// 固定宽度的表格单元格：列位置不随内容长度变化，内容超长时在单元格内截断。
///
/// 文本左对齐，因此标签与数值之间没有多余空隙；宽度固定使位数增长不会推动后续列。
struct Cell {
    text: String,
    width: usize,
    /// 宽度不足时优先丢弃优先级低的列。
    priority: u8,
}

impl Cell {
    fn left(text: String, width: usize, priority: u8) -> Self {
        Self {
            text,
            width,
            priority,
        }
    }
}

/// 把单元格内容裁剪或补齐到固定显示宽度。
fn fit_cell(cell: &Cell) -> String {
    let truncated = truncate_end(&cell.text, cell.width);
    let pad = cell.width.saturating_sub(truncated.width());
    format!("{truncated}{}", " ".repeat(pad))
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
/// 进度百分比与当前文件、计数与时间、阶段与进度、缓冲与速率。
pub fn render(snapshot: &Snapshot, width: usize) -> Vec<String> {
    let width = width.max(20);
    vec![
        render_head_line(snapshot, width),
        render_counters_line(snapshot, width),
        render_phase_line(snapshot, width),
        render_io_line(snapshot, width),
    ]
}

/// 第一行：全程百分比与当前文件（仅文件名）。
fn render_head_line(snapshot: &Snapshot, width: usize) -> String {
    let prefix = format!("{:>5.1}%  >  ", snapshot.percent.clamp(0.0, 1.0) * 100.0);
    let available = width.saturating_sub(prefix.width());
    format!("{prefix}{}", truncate_start(&snapshot.current, available))
}

/// 第二行：校验的匹配与不一致计数、已用时间与 ETA。
///
/// 数值一律左对齐：标签紧跟数值，没有多余缩进；列宽固定，位数增加也不会移动后面的列。
fn render_counters_line(snapshot: &Snapshot, width: usize) -> String {
    let elapsed = format!("elapsed {}", human_duration(snapshot.elapsed.as_secs()));
    let eta = snapshot
        .eta
        .map(|seconds| format!("ETA {}", human_duration(seconds)));

    let mut cells = Vec::new();
    if snapshot.phase == Phase::Verify {
        cells.push(Cell::left(format!("match {}", snapshot.verify_ok), 11, 100));
        cells.push(Cell::left(
            format!("mismatch {}", snapshot.verify_bad),
            14,
            100,
        ));
    }
    cells.push(Cell::left(elapsed, 16, 60));
    if let Some(eta) = eta {
        cells.push(Cell::left(eta, 12, 40));
    }
    build_row(cells, width)
}

/// 第三行：阶段名、文件计数、字节进度。
fn render_phase_line(snapshot: &Snapshot, width: usize) -> String {
    let counts = if snapshot.files_total > 0 {
        format!("{}/{} files", snapshot.files_done, snapshot.files_total)
    } else {
        format!("{} files", snapshot.files_done)
    };

    let mut cells = vec![
        Cell::left(snapshot.phase.label().to_string(), 4, 100),
        Cell::left(counts, 17, 80),
    ];
    if matches!(snapshot.phase, Phase::Copy | Phase::Verify) {
        cells.push(Cell::left(
            format!(
                "{}/{}",
                human_bytes(snapshot.bytes_done),
                human_bytes(snapshot.bytes_total)
            ),
            23,
            60,
        ));
    }
    build_row(cells, width)
}

/// 第四行：读取速率（哈希从两侧读数据）。
fn render_io_line(snapshot: &Snapshot, width: usize) -> String {
    if !matches!(snapshot.phase, Phase::Copy | Phase::Verify) {
        return String::new();
    }
    let (read_value, read_unit) = speed_parts(snapshot.read_speed);
    build_row(
        vec![Cell::left(
            format!("R {read_value:.1} {read_unit}/s"),
            15,
            40,
        )],
        width,
    )
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
    started: Instant,
    last_draw: Instant,
    phase: Phase,
    /// 源清单总字节数，全程不变；总进度以 2 倍它作为分母。
    total_bytes: u64,
    files_total: u64,
    files_done: u64,
    enabled: bool,
    last_current: String,
    /// 每次换阶段后的第一次采样只重建速率基线，不参与平均。
    meter_needs_reset: bool,
}

impl Ui {
    pub fn new(quiet: bool) -> Self {
        let interactive = io::stdout().is_terminal() && !quiet;
        let color = interactive && std::env::var_os("NO_COLOR").is_none();
        Self {
            painter: Painter::new(interactive, color),
            read_meter: SpeedMeter::new(),
            started: Instant::now(),
            last_draw: Instant::now() - Duration::from_secs(1),
            phase: Phase::Scan,
            total_bytes: 0,
            files_total: 0,
            files_done: 0,
            enabled: interactive,
            last_current: String::new(),
            meter_needs_reset: true,
        }
    }

    /// 源清单总字节数，进入阶段 2 之前设置一次。
    pub fn set_total_bytes(&mut self, total_bytes: u64) {
        self.total_bytes = total_bytes;
    }

    pub fn set_phase(&mut self, phase: Phase, files_total: u64) {
        self.phase = phase;
        self.files_total = files_total;
        self.files_done = 0;
        self.meter_needs_reset = true;
        self.last_draw = Instant::now() - Duration::from_secs(1);
    }

    /// 阶段 1 与阶段 3 的完成量由调用方直接推进（不经过统计计数）。
    pub fn set_progress(&mut self, files_done: u64) {
        self.files_done = files_done;
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

        let read_bytes = stats.read_bytes.load(std::sync::atomic::Ordering::Relaxed);
        if self.meter_needs_reset {
            // 上一阶段的读数与间隔不属于本阶段，先重建基线再开始测速。
            self.read_meter.reset(read_bytes, now);
            self.meter_needs_reset = false;
        }
        let read_speed = self.read_meter.sample(read_bytes, now);

        let hashed = stats
            .hashed_bytes
            .load(std::sync::atomic::Ordering::Relaxed);
        let total_bytes = self.total_bytes;

        let files_done = match self.phase {
            Phase::Verify => {
                stats.verify_ok.load(std::sync::atomic::Ordering::Relaxed) as u64
                    + stats.verify_bad.load(std::sync::atomic::Ordering::Relaxed) as u64
            }
            Phase::Scan | Phase::Copy | Phase::Commit => self.files_done,
        };

        // 阶段行展示的字节进度：校验按单侧已读字节（两侧各读一遍，各计一半）。
        let (bytes_done, bytes_total) = match self.phase {
            Phase::Verify => (hashed / 2, total_bytes),
            Phase::Scan | Phase::Copy | Phase::Commit => (0, total_bytes),
        };

        let percent = overall_percent(self.phase, total_bytes, 0, hashed);

        // ETA 按当前阶段的剩余工作量与读速估算。
        let eta = match self.phase {
            Phase::Verify => eta_seconds(
                total_bytes.saturating_mul(2).saturating_sub(hashed),
                read_speed,
            ),
            Phase::Scan | Phase::Copy | Phase::Commit => None,
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
            eta,
            elapsed: now.duration_since(self.started),
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
            eta: Some(52),
            elapsed: Duration::from_secs(14),
            current: "C7463.MP4".to_string(),
            verify_ok: 8,
            verify_bad: 0,
        }
    }

    #[test]
    fn overall_progress_puts_verify_in_the_second_half() {
        let total = 1000u64;
        assert_eq!(overall_percent(Phase::Scan, total, 0, 0), 0.0);
        // 复制交给 sy 后，校验开始时已经是 50%。
        assert_eq!(overall_percent(Phase::Verify, total, 0, 0), 0.5);
        // 校验两侧各读一遍，按已读字节的一半计入。
        assert_eq!(overall_percent(Phase::Verify, total, 0, total), 0.75);
        assert_eq!(overall_percent(Phase::Verify, total, 0, total * 2), 1.0);
        assert_eq!(overall_percent(Phase::Copy, total, total, 0), 0.5);
        // 没有文件时不应出现除零或超过 1 的进度。
        assert_eq!(overall_percent(Phase::Verify, 0, 0, 0), 0.5);
    }

    #[test]
    fn renders_percent_and_file_on_first_line() {
        let lines = render(&snapshot(Phase::Verify, 0.5), 120);
        assert_eq!(lines.len(), 4);
        assert_eq!(lines[0], " 50.0%  >  C7463.MP4");
        assert!(lines[1].starts_with("match "));
        assert!(lines[1].contains("mismatch "));
        assert!(lines[1].contains("ETA"));
        assert!(lines[2].starts_with("VRFY"));
        assert!(lines[2].contains("GiB"));
        assert!(lines[3].starts_with("R "));
    }

    #[test]
    fn counters_line_has_no_copy_counters() {
        let lines = render(&snapshot(Phase::Verify, 0.6), 120);
        assert!(
            !lines[1].contains("ok "),
            "复制计数由 sy 输出，不再出现在界面里"
        );
        assert!(!lines[1].contains("skip"));
    }

    #[test]
    fn columns_do_not_shift_when_counters_grow() {
        let mut small = snapshot(Phase::Verify, 0.5);
        small.verify_ok = 9;
        small.files_done = 9;
        let mut large = small.clone_shallow();
        large.verify_ok = 12345;
        large.verify_bad = 6789;
        large.files_done = 12345;
        large.files_total = 54321;
        large.elapsed = Duration::from_secs(3 * 3600 + 65);

        let small_lines = render(&small, 120);
        let large_lines = render(&large, 120);

        // 用显示宽度比较列位置：`…` 是 3 字节宽 1 列的字符，字节下标会误导。
        let column = |line: &str, needle: &str| line[..line.find(needle).unwrap()].width();
        assert_eq!(
            column(&small_lines[1], "mismatch"),
            column(&large_lines[1], "mismatch"),
            "mismatch 列位置不应移动"
        );
        assert_eq!(
            column(&small_lines[1], "elapsed"),
            column(&large_lines[1], "elapsed"),
            "elapsed 列位置不应移动"
        );
        assert_eq!(
            column(&small_lines[2], "GiB"),
            column(&large_lines[2], "GiB"),
            "字节进度列位置不应移动"
        );
        assert_eq!(
            small_lines[1].width(),
            large_lines[1].width(),
            "计数行宽度固定"
        );
        assert_eq!(
            small_lines[2].width(),
            large_lines[2].width(),
            "阶段行宽度固定"
        );
    }

    #[test]
    fn cells_are_truncated_when_content_exceeds_width() {
        let mut snap = snapshot(Phase::Verify, 0.5);
        snap.verify_ok = u64::MAX;
        snap.verify_bad = u64::MAX;
        let lines = render(&snap, 120);
        for line in &lines {
            assert!(line.width() <= 120, "行宽超出：{line}");
        }
    }

    #[test]
    fn degrades_columns_as_width_shrinks() {
        let wide = render(&snapshot(Phase::Verify, 0.5), 120).join("\n");
        assert!(wide.contains("ETA"));
        assert!(wide.contains("GiB"));

        let mid = render(&snapshot(Phase::Verify, 0.5), 40).join("\n");
        assert!(!mid.contains("ETA"), "宽度不足时先丢 ETA：{mid}");

        let tiny = render(&snapshot(Phase::Verify, 0.5), 20).join("\n");
        assert!(tiny.contains('%'));
        assert!(!tiny.contains("GiB"));
    }

    #[test]
    fn every_line_fits_the_width() {
        for width in [20usize, 30, 44, 60, 72, 90, 110, 200] {
            let lines = render(&snapshot(Phase::Verify, 0.42), width);
            assert_eq!(lines.len(), 4);
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
    fn io_line_is_empty_outside_io_phases() {
        let lines = render(&snapshot(Phase::Scan, 0.0), 100);
        assert_eq!(lines[3], "");
        assert_eq!(lines.len(), 4);
    }

    #[test]
    fn long_file_names_keep_their_tail() {
        let mut snap = snapshot(Phase::Verify, 0.6);
        snap.current = "素材目录_第一期_相机A_0001.MOV".to_string();
        let lines = render(&snap, 40);
        assert!(lines[0].width() <= 40);
        assert!(lines[0].ends_with(".MOV"), "应保留文件名尾部：{}", lines[0]);
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
                eta: self.eta,
                elapsed: self.elapsed,
                current: self.current.clone(),
                verify_ok: self.verify_ok,
                verify_bad: self.verify_bad,
            }
        }
    }
}
