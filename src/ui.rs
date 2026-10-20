//! 终端界面：两行阶段进度（COPY / VRFY）加一行当前文件。
//!
//! 复制阶段的输出由内嵌的 sy 直接绘制，本界面只在校验阶段刷新。
//! 进度写 stderr，且只在 stderr 是 TTY 时逐帧刷新；stdout 只保留汇总。

use std::io::{self, IsTerminal, Write};
use std::time::{Duration, Instant};

use unicode_width::UnicodeWidthStr;

use crate::stats::{
    eta_seconds, human_bytes_parts, human_duration, speed_parts, SpeedMeter, Stats,
};

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

/// 单个阶段的进度。两个阶段各占全程工作的一半，因此各自按自己的总量算百分比。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stage {
    pub files_done: u64,
    pub files_total: u64,
    pub bytes_done: u64,
    pub bytes_total: u64,
}

impl Stage {
    /// 已完成的阶段：文件数与字节数都记满，比例恒为 100%。
    pub fn complete(files: u64, bytes: u64) -> Self {
        Self {
            files_done: files,
            files_total: files,
            bytes_done: bytes,
            bytes_total: bytes,
        }
    }

    /// 阶段自身的完成比例（0 到 1）；总量为零时按 0 处理。
    pub fn ratio(&self) -> f64 {
        if self.bytes_total > 0 {
            (self.bytes_done as f64 / self.bytes_total as f64).clamp(0.0, 1.0)
        } else if self.files_total > 0 {
            (self.files_done as f64 / self.files_total as f64).clamp(0.0, 1.0)
        } else {
            0.0
        }
    }
}

/// 绘制风格：是否上色、是否只用 ASCII 字形。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Style {
    pub color: bool,
    pub ascii: bool,
}

impl Style {
    /// 纯文本、Unicode 字形（测试与管道输出使用）。
    pub const PLAIN: Self = Self {
        color: false,
        ascii: false,
    };
}

/// 一次绘制所需的全部信息。
pub struct Snapshot {
    /// 复制阶段，界面出现时它已经完成。
    pub copy: Stage,
    pub verify: Stage,
    pub read_speed: f64,
    pub eta: Option<u64>,
    pub current: String,
    pub verify_bad: u64,
}

const LABEL_W: usize = 4;
const PCT_W: usize = 5;
const BYTES_W: usize = 17;
const FILES_W: usize = 11;
const SPEED_W: usize = 12;
const TAIL_W: usize = 10;
const SEP: usize = 2;
const MIN_BAR: usize = 10;
const MAX_BAR: usize = 60;
const FILE_INDENT: usize = LABEL_W + SEP;

const SGR_DIM: &str = "2";
const SGR_BOLD: &str = "1";
const SGR_ACCENT: &str = "36";
const SGR_ERROR: &str = "31";

/// 纯函数渲染，便于单元测试。返回值固定为 3 行：复制阶段、校验阶段、当前文件。
pub fn render(snapshot: &Snapshot, width: usize, style: Style) -> Vec<String> {
    let width = width.max(16);
    let copy = RowParts {
        label: Phase::Copy.label(),
        stage: snapshot.copy,
        speed: None,
        tail: None,
        tail_is_error: false,
    };
    let verify_tail = if snapshot.verify_bad > 0 {
        Some(format!("BAD {}", snapshot.verify_bad))
    } else {
        snapshot
            .eta
            .map(|seconds| format!("ETA {}", human_duration(seconds)))
    };
    let verify = RowParts {
        label: Phase::Verify.label(),
        stage: snapshot.verify,
        speed: Some(snapshot.read_speed),
        tail: verify_tail,
        tail_is_error: snapshot.verify_bad > 0,
    };
    vec![
        render_stage_row(&copy, width, style),
        render_stage_row(&verify, width, style),
        render_file_line(&snapshot.current, width, style),
    ]
}

struct RowParts<'a> {
    label: &'a str,
    stage: Stage,
    speed: Option<f64>,
    tail: Option<String>,
    tail_is_error: bool,
}

/// 一行阶段：标签、进度条、百分比、字节、文件计数、速率、尾部槽位（ETA 或 BAD n）。
///
/// 列宽固定，数字位数变化不移动后面的列；宽度不足时按“速率与 ETA、文件计数、字节”的顺序整列丢弃，
/// 进度条吃掉剩下的宽度（10 到 60 列之间）。
fn render_stage_row(parts: &RowParts, width: usize, style: Style) -> String {
    let mut show_status = true;
    let mut show_files = true;
    let mut show_bytes = true;

    loop {
        // 单元格数：标签、进度条、百分比，加上可选列。
        let cells =
            3 + usize::from(show_bytes) + usize::from(show_files) + if show_status { 2 } else { 0 };
        let mut fixed = LABEL_W + PCT_W;
        if show_bytes {
            fixed += BYTES_W;
        }
        if show_files {
            fixed += FILES_W;
        }
        if show_status {
            fixed += SPEED_W + TAIL_W;
        }
        let seps = SEP * (cells - 1);
        if fixed + seps + MIN_BAR <= width {
            let bar_w = width.saturating_sub(fixed + seps).min(MAX_BAR);
            return compose_row(parts, bar_w, show_bytes, show_files, show_status, style);
        }
        // 丢一列：先丢速率与 ETA，再丢文件计数，最后丢字节。
        if show_status {
            show_status = false;
        } else if show_files {
            show_files = false;
        } else if show_bytes {
            show_bytes = false;
        } else {
            // 已经丢到只剩标签、进度条与百分比：进度条吃掉剩余宽度（极窄时可能小于 MIN_BAR）。
            let bar_w = width.saturating_sub(LABEL_W + PCT_W + SEP * 2).min(MAX_BAR);
            return compose_row(parts, bar_w, false, false, false, style);
        }
    }
}

fn compose_row(
    parts: &RowParts,
    bar_w: usize,
    show_bytes: bool,
    show_files: bool,
    show_status: bool,
    style: Style,
) -> String {
    let ratio = parts.stage.ratio();
    // 未真正完成时不显示 100%，避免四舍五入让人以为已经结束。
    let percent = if ratio >= 1.0 {
        100.0
    } else {
        (ratio * 100.0).min(99.0)
    };
    let pct = format!("{percent:>4.0}%");

    let mut out = String::new();
    out.push_str(&paint(&pad(parts.label, LABEL_W, style), SGR_DIM, style));
    out.push_str(&sep());
    out.push_str(&bar(ratio, bar_w, style));
    out.push_str(&sep());
    out.push_str(&paint(&pct, SGR_BOLD, style));

    if show_bytes {
        out.push_str(&sep());
        out.push_str(&pad(&bytes_text(&parts.stage), BYTES_W, style));
    }
    if show_files {
        out.push_str(&sep());
        out.push_str(&paint(
            &pad(&files_text(&parts.stage), FILES_W, style),
            SGR_DIM,
            style,
        ));
    }
    if show_status {
        out.push_str(&sep());
        out.push_str(&pad(&speed_text(parts.speed), SPEED_W, style));
        out.push_str(&sep());
        let tail = parts.tail.clone().unwrap_or_default();
        let code = if parts.tail_is_error {
            SGR_ERROR
        } else {
            SGR_DIM
        };
        out.push_str(&paint(tail.trim_end(), code, style));
    }
    out.trim_end().to_string()
}

fn render_file_line(current: &str, width: usize, style: Style) -> String {
    if current.is_empty() {
        return String::new();
    }
    let available = width.saturating_sub(FILE_INDENT);
    let name = truncate_start(current, available);
    format!(
        "{}{}",
        " ".repeat(FILE_INDENT),
        paint(&name, SGR_DIM, style)
    )
}

/// 两侧用同一个单位（总量所在的单位），避免出现 “512.0 KiB/600.0 MiB” 这种长度不一的写法。
fn bytes_text(stage: &Stage) -> String {
    if stage.bytes_total == 0 {
        return String::new();
    }
    let (total_value, unit) = human_bytes_parts(stage.bytes_total);
    let divisor = (stage.bytes_total as f64 / total_value.max(f64::EPSILON)).max(1.0);
    let done_value = stage.bytes_done as f64 / divisor;
    if unit == "B" {
        format!("{}/{total_value:.0} {unit}", done_value.round() as u64)
    } else {
        format!("{done_value:.1}/{total_value:.1} {unit}")
    }
}

fn files_text(stage: &Stage) -> String {
    if stage.files_total == 0 {
        return String::new();
    }
    format!("{}/{}", stage.files_done, stage.files_total)
}

fn speed_text(speed: Option<f64>) -> String {
    match speed {
        Some(speed) if speed > 0.0 => {
            let (value, unit) = speed_parts(speed);
            format!("{value:.1} {unit}/s")
        }
        Some(_) => "0 B/s".to_string(),
        None => String::new(),
    }
}

fn paint(text: &str, code: &str, style: Style) -> String {
    if !style.color || text.is_empty() {
        return text.to_string();
    }
    format!("\x1b[{code}m{text}\x1b[0m")
}

fn sep() -> String {
    " ".repeat(SEP)
}

fn pad(text: &str, width: usize, style: Style) -> String {
    let ellipsis = if style.ascii { ".." } else { "…" };
    let text = truncate_end(text, width, ellipsis);
    let fill = width.saturating_sub(text.width());
    format!("{text}{}", " ".repeat(fill))
}

/// 进度条：填充量与阶段比例一致，未完成部分用浅色轨道。
fn bar(ratio: f64, width: usize, style: Style) -> String {
    let (edge_l, fill_ch, track_ch, edge_r) = if style.ascii {
        ("|", "#", "-", "|")
    } else {
        ("▕", "█", "░", "▏")
    };
    // 两个端点也算在 width 内，使整条进度条恰好占 width 列。
    let inner = width.saturating_sub(2);
    let filled = ((ratio.clamp(0.0, 1.0) * inner as f64).round() as usize).min(inner);
    let track = inner - filled;
    format!(
        "{}{}{}{}",
        paint(edge_l, SGR_DIM, style),
        paint(&fill_ch.repeat(filled), SGR_ACCENT, style),
        paint(&track_ch.repeat(track), SGR_DIM, style),
        paint(edge_r, SGR_DIM, style),
    )
}

fn truncate_end(s: &str, max_width: usize, ellipsis: &str) -> String {
    if s.width() <= max_width {
        return s.to_string();
    }
    let mark_w = ellipsis.width();
    if max_width <= mark_w {
        return ellipsis.chars().take(max_width).collect();
    }
    let mut out = String::new();
    let mut used = 0;
    for ch in s.chars() {
        let w = ch.to_string().width();
        if used + w > max_width - mark_w {
            break;
        }
        used += w;
        out.push(ch);
    }
    out.push_str(ellipsis);
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

/// 一次运行的界面状态与绘制器。
pub struct Ui {
    painter: Painter,
    read_meter: SpeedMeter,
    last_draw: Instant,
    phase: Phase,
    /// 复制阶段在界面出现时已经结束，这里保存它的最终统计。
    copy: Stage,
    /// 源清单总字节数，全程不变。
    total_bytes: u64,
    files_total: u64,
    files_done: u64,
    enabled: bool,
    style: Style,
    last_current: String,
    /// 每次换阶段后的第一次采样只重建速率基线，不参与平均。
    meter_needs_reset: bool,
}

impl Ui {
    /// `color` 与 `ascii` 由调用方按终端能力与命令行开关决定；界面本身只在 stderr 是 TTY 时启用。
    pub fn new(quiet: bool, color: bool, ascii: bool) -> Self {
        let interactive = io::stderr().is_terminal() && !quiet;
        let style = Style {
            color: color && interactive,
            ascii,
        };
        Self {
            painter: Painter::new(interactive, style),
            read_meter: SpeedMeter::new(),
            last_draw: Instant::now() - Duration::from_secs(1),
            phase: Phase::Scan,
            copy: Stage::default(),
            total_bytes: 0,
            files_total: 0,
            files_done: 0,
            enabled: interactive,
            style,
            last_current: String::new(),
            meter_needs_reset: true,
        }
    }

    /// 源清单总字节数，进入校验之前设置一次。
    pub fn set_total_bytes(&mut self, total_bytes: u64) {
        self.total_bytes = total_bytes;
    }

    /// 复制阶段结束后调用：COPY 行显示为完成，并带上清单的文件数与字节数。
    pub fn set_copy_totals(&mut self, files: u64, bytes: u64) {
        self.copy = Stage::complete(files, bytes);
    }

    pub fn set_phase(&mut self, phase: Phase, files_total: u64) {
        self.phase = phase;
        self.files_total = files_total;
        self.files_done = 0;
        self.meter_needs_reset = true;
        self.last_draw = Instant::now() - Duration::from_secs(1);
    }

    /// 阶段结束时的完成量由调用方直接推进（不经过统计计数）。
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

        // 校验阶段两侧各读一遍，单侧已读字节按一半计。
        let verify = Stage {
            files_done: if self.files_total > 0 {
                files_done.min(self.files_total)
            } else {
                files_done
            },
            files_total: self.files_total,
            bytes_done: hashed / 2,
            bytes_total: total_bytes,
        };

        // ETA 按剩余工作量（两侧）与读速估算。
        let eta = eta_seconds(
            total_bytes.saturating_mul(2).saturating_sub(hashed),
            read_speed,
        );

        let snapshot = Snapshot {
            copy: self.copy,
            verify,
            read_speed,
            eta,
            current: stats.current(),
            verify_bad: stats.verify_bad.load(std::sync::atomic::Ordering::Relaxed) as u64,
        };

        let width = terminal_width().unwrap_or(80);
        self.painter.draw(&render(&snapshot, width, self.style));
    }

    pub fn finish(&mut self) {
        self.painter.finish();
    }
}

fn terminal_width() -> Option<usize> {
    terminal_size::terminal_size().map(|(w, _)| w.0 as usize)
}

struct Painter {
    enabled: bool,
    drawn: usize,
}

impl Painter {
    fn new(enabled: bool, _style: Style) -> Self {
        Self { enabled, drawn: 0 }
    }

    fn draw(&mut self, lines: &[String]) {
        if !self.enabled {
            return;
        }
        let mut out = io::stderr().lock();
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
        let mut out = io::stderr().lock();
        let _ = write!(out, "\x1b[{}A", self.drawn);
        for _ in 0..self.drawn {
            let _ = write!(out, "\r\x1b[2K\n");
        }
        let _ = write!(out, "\x1b[{}A\x1b[?25h", self.drawn);
        let _ = out.flush();
        self.drawn = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot() -> Snapshot {
        Snapshot {
            copy: Stage::complete(1351, 40 * 1024 * 1024 * 1024),
            verify: Stage {
                files_done: 138,
                files_total: 1351,
                bytes_done: 5 * 1024 * 1024 * 1024,
                bytes_total: 40 * 1024 * 1024 * 1024,
            },
            read_speed: 364.4 * 1024.0 * 1024.0,
            eta: Some(199),
            current: "C7418.MP4".to_string(),
            verify_bad: 0,
        }
    }

    fn clone_snapshot(snap: &Snapshot) -> Snapshot {
        Snapshot {
            copy: snap.copy,
            verify: snap.verify,
            read_speed: snap.read_speed,
            eta: snap.eta,
            current: snap.current.clone(),
            verify_bad: snap.verify_bad,
        }
    }

    #[test]
    fn stage_ratio_prefers_bytes() {
        let stage = Stage {
            files_done: 1,
            files_total: 100,
            bytes_done: 50,
            bytes_total: 100,
        };
        assert!((stage.ratio() - 0.5).abs() < 1e-9);
        let files_only = Stage {
            files_done: 1,
            files_total: 4,
            bytes_done: 0,
            bytes_total: 0,
        };
        assert!((files_only.ratio() - 0.25).abs() < 1e-9);
        assert_eq!(Stage::default().ratio(), 0.0);
    }

    #[test]
    fn complete_stage_is_full() {
        let stage = Stage::complete(12, 4096);
        assert_eq!(stage.ratio(), 1.0);
        assert_eq!(stage.files_done, 12);
        assert_eq!(stage.bytes_total, 4096);
    }

    #[test]
    fn renders_two_stage_rows_and_the_current_file() {
        let lines = render(&snapshot(), 100, Style::PLAIN);
        assert_eq!(lines.len(), 3);
        assert!(
            lines[0].starts_with("COPY"),
            "第一行是复制阶段：{}",
            lines[0]
        );
        assert!(lines[0].contains("100%"), "复制阶段已完成：{}", lines[0]);
        assert!(
            lines[1].starts_with("VRFY"),
            "第二行是校验阶段：{}",
            lines[1]
        );
        assert!(lines[1].contains("ETA"), "{}", lines[1]);
        assert!(lines[2].trim_end().ends_with("C7418.MP4"), "{}", lines[2]);
    }

    #[test]
    fn verify_percent_is_stage_local() {
        let mut snap = snapshot();
        snap.verify.bytes_done = snap.verify.bytes_total / 2;
        let lines = render(&snap, 100, Style::PLAIN);
        assert!(
            lines[1].contains(" 50%"),
            "校验按自身总量算百分比：{}",
            lines[1]
        );
    }

    #[test]
    fn bad_count_replaces_eta() {
        let mut snap = snapshot();
        snap.verify_bad = 2;
        let lines = render(&snap, 100, Style::PLAIN);
        assert!(lines[1].contains("BAD 2"), "{}", lines[1]);
        assert!(!lines[1].contains("ETA"), "{}", lines[1]);
    }

    #[test]
    fn columns_do_not_shift_when_counters_grow() {
        let mut small = snapshot();
        small.verify.files_done = 9;
        let mut large = clone_snapshot(&small);
        large.verify.files_done = 1234;
        large.verify.bytes_done = 39 * 1024 * 1024 * 1024;

        let small_lines = render(&small, 110, Style::PLAIN);
        let large_lines = render(&large, 110, Style::PLAIN);
        let column = |line: &str, needle: &str| line[..line.find(needle).unwrap()].width();
        assert_eq!(
            column(&small_lines[1], "ETA"),
            column(&large_lines[1], "ETA"),
            "ETA 列位置不应移动"
        );
        assert_eq!(
            column(&small_lines[1], "MiB/s"),
            column(&large_lines[1], "MiB/s"),
            "速率所在列不应移动"
        );
        assert_eq!(small_lines[1].width(), large_lines[1].width(), "行宽固定");
        assert_eq!(small_lines[0].width(), large_lines[0].width(), "行宽固定");
    }

    #[test]
    fn every_line_fits_the_width() {
        for width in [16usize, 20, 30, 44, 60, 72, 90, 110, 200] {
            let lines = render(&snapshot(), width, Style::PLAIN);
            assert_eq!(lines.len(), 3);
            for line in &lines {
                assert!(
                    line.width() <= width,
                    "宽度 {width} 时行宽 {} 超出：{line}",
                    line.width()
                );
            }
        }
    }

    #[test]
    fn degrades_columns_as_width_shrinks() {
        let wide = render(&snapshot(), 110, Style::PLAIN).join("\n");
        assert!(wide.contains("ETA"));
        assert!(wide.contains("MiB/s"));
        assert!(wide.contains("GiB"));
        assert!(wide.contains("1351"));

        let mid = render(&snapshot(), 70, Style::PLAIN).join("\n");
        assert!(!mid.contains("ETA"), "宽度不足时先丢 ETA 与速率：{mid}");
        assert!(mid.contains("1351"), "文件计数仍然保留：{mid}");

        let narrow = render(&snapshot(), 40, Style::PLAIN).join("\n");
        assert!(!narrow.contains("1351"), "再窄就丢文件计数：{narrow}");
        assert!(narrow.contains("VRFY"), "{narrow}");
        assert!(narrow.contains('%'), "{narrow}");
    }

    #[test]
    fn ascii_style_avoids_non_ascii_glyphs() {
        let style = Style {
            color: false,
            ascii: true,
        };
        let mut snap = snapshot();
        snap.verify_bad = 3;
        for line in render(&snap, 100, style) {
            assert!(line.is_ascii(), "ASCII 模式不应出现非 ASCII 字形：{line}");
        }
    }

    #[test]
    fn color_style_emits_sgr_codes() {
        let plain = render(&snapshot(), 100, Style::PLAIN).join("\n");
        assert!(!plain.contains('\x1b'), "纯文本模式不应带 SGR");

        let style = Style {
            color: true,
            ascii: false,
        };
        let colored = render(&snapshot(), 100, style).join("\n");
        assert!(colored.contains("\x1b[36m"), "进度条填充用强调色");
        assert!(colored.contains("\x1b[1m"), "百分比加粗");
    }

    #[test]
    fn long_file_names_keep_their_tail() {
        let mut snap = snapshot();
        snap.current = "素材目录_第一期_相机A_0001.MOV".to_string();
        let lines = render(&snap, 40, Style::PLAIN);
        let name = lines[2].trim_end();
        assert!(name.ends_with("_0001.MOV"), "保留文件尾名：{name}");
        assert!(lines[2].width() <= 40, "行宽不超宽：{name}");
    }

    #[test]
    fn empty_current_file_yields_blank_line() {
        let mut snap = snapshot();
        snap.current = String::new();
        let lines = render(&snap, 100, Style::PLAIN);
        assert_eq!(lines[2], "");
    }
}
