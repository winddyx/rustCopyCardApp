//! 运行期的一行文字进度：阶段、全程百分比、已完成/总量、当前文件。
//!
//! 不画进度条，也不显示速度与 ETA。整行原地刷新，写 stderr；stderr 不是 TTY 或
//! `--quiet` 时完全不绘制，stdout 只保留汇总。

use std::io::{self, IsTerminal, Write};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use unicode_width::UnicodeWidthStr;

use crate::stats::{Stats, BYTE_UNITS};

/// 阶段标签，两处进度共用同一行的同一个位置。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Scan,
    Copy,
    Verify,
}

impl Phase {
    pub fn label(self) -> &'static str {
        match self {
            Phase::Scan => "SCAN",
            Phase::Copy => "COPY",
            Phase::Verify => "VERIFY",
        }
    }

    fn code(self) -> u8 {
        match self {
            Phase::Scan => 0,
            Phase::Copy => 1,
            Phase::Verify => 2,
        }
    }

    fn from_code(code: u8) -> Self {
        match code {
            1 => Phase::Copy,
            2 => Phase::Verify,
            _ => Phase::Scan,
        }
    }
}

/// 标签列宽：`VERIFY` 最长，其余补空格，文件名列因此不会左右跳。
const LABEL_W: usize = 6;
/// 百分比列宽，含 `%`。
const PCT_W: usize = 4;
/// 字段之间的固定间隔。
const GAP: usize = 2;
/// 文件名至少要留出的列宽，低于它就整段丢掉文件名。
const MIN_FILE: usize = 8;

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

/// 写入侧：只更新几个数字，绘制由后台线程负责。
#[derive(Clone)]
pub struct LiveHandle {
    phase: Arc<AtomicU8>,
    total: Arc<AtomicU64>,
    scanned: Arc<AtomicUsize>,
}

impl LiveHandle {
    /// 扫描阶段已遍历的文件数。
    pub fn set_scanned(&self, files: usize) {
        self.scanned.store(files, Ordering::Relaxed);
    }

    /// 源总字节数；乘二就是全程的分母。扫描结束后调用一次。
    pub fn set_total(&self, bytes: u64) {
        self.total.store(bytes, Ordering::Relaxed);
    }

    /// 切换阶段；当前文件由 `Stats` 提供，切换后自然更新。
    pub fn set_phase(&self, phase: Phase) {
        self.phase.store(phase.code(), Ordering::Relaxed);
    }
}

/// 一行进度的绘制器，运行期间只有它往 stderr 写东西。
pub struct Live {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl Live {
    /// `enabled` 为假（stderr 非 TTY 或 --quiet）时不绘制，也不开线程。
    pub fn start(stats: Arc<Stats>, enabled: bool, style: Style) -> (Self, LiveHandle) {
        let state = LiveHandle {
            phase: Arc::new(AtomicU8::new(Phase::Scan.code())),
            total: Arc::new(AtomicU64::new(0)),
            scanned: Arc::new(AtomicUsize::new(0)),
        };
        let stop = Arc::new(AtomicBool::new(false));
        let handle = if enabled {
            let stop_flag = Arc::clone(&stop);
            let ticker_state = state.clone();
            Some(std::thread::spawn(move || {
                let mut drawn = false;
                let mut last = String::new();
                loop {
                    let width = terminal_width().unwrap_or(80);
                    let line = render(&ticker_state, &stats, width, style);
                    if line != last {
                        let mut out = io::stderr().lock();
                        let _ = write!(out, "\r\x1b[2K{line}");
                        let _ = out.flush();
                        drawn = true;
                        last = line;
                    }
                    if stop_flag.load(Ordering::Relaxed) {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(100));
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
        (Self { stop, handle }, state)
    }

    /// 停止刷新并擦掉这一行，随后才输出汇总。
    pub fn finish(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for Live {
    fn drop(&mut self) {
        self.finish();
    }
}

/// 已经完成的字节数：复制处理的字节加上校验覆盖的字节（每次校验读两侧，除以二）。
pub fn done_bytes(stats: &Stats) -> u64 {
    stats.copied_bytes.load(Ordering::Relaxed) + stats.hashed_bytes.load(Ordering::Relaxed) / 2
}

fn render(state: &LiveHandle, stats: &Stats, width: usize, style: Style) -> String {
    let phase = Phase::from_code(state.phase.load(Ordering::Relaxed));
    let label = paint(&pad_label(phase.label()), SGR_DIM, style);
    if phase == Phase::Scan {
        let scanned = state.scanned.load(Ordering::Relaxed);
        return format!("{label} {} 个文件", scanned);
    }

    let total = state.total.load(Ordering::Relaxed);
    let denom = total.saturating_mul(2);
    let done = done_bytes(stats).min(denom);
    let percent = done.saturating_mul(100).checked_div(denom).unwrap_or(100) as u32;
    let pct = format!("{percent:>3}%");
    let bytes = bytes_columns(done, denom);
    let current = stats.current();

    let fixed = LABEL_W + 1 + PCT_W + GAP + bytes.width() + GAP;
    if width >= fixed + MIN_FILE {
        let shown = truncate_start(&current, width - fixed);
        format!("{label} {pct}{}{bytes}  {shown}", " ".repeat(GAP))
    } else if width >= fixed {
        format!("{label} {pct}{}{bytes}", " ".repeat(GAP))
    } else if width >= LABEL_W + 1 + PCT_W {
        format!("{label} {pct}")
    } else {
        truncate_end(&format!("{label} {pct}"), width)
    }
}

/// 分子与分母共用分母的单位，位数固定，重绘时不左右跳动。
fn bytes_columns(done: u64, denom: u64) -> String {
    let index = unit_index(denom);
    let unit = BYTE_UNITS[index];
    let den = format_value(denom, index);
    let num = format_value(done.min(denom), index);
    format!("{num:>width$}/{den} {unit}", width = den.width())
}

/// 与 `human_bytes` 一致的进位规则，保证单位与汇总里的写法相同。
fn unit_index(bytes: u64) -> usize {
    let mut value = bytes as f64;
    let mut index = 0;
    while value >= 1024.0 && index + 1 < BYTE_UNITS.len() {
        value /= 1024.0;
        index += 1;
    }
    index
}

fn format_value(bytes: u64, index: usize) -> String {
    if index == 0 {
        bytes.to_string()
    } else {
        format!("{:.1}", bytes as f64 / 1024f64.powi(index as i32))
    }
}

fn pad_label(label: &str) -> String {
    format!("{label:<LABEL_W$}")
}

fn paint(text: &str, code: &str, style: Style) -> String {
    if !style.color || text.trim().is_empty() {
        return text.to_string();
    }
    // 尾部空格留在 SGR 之外。
    let trimmed = text.trim_end();
    let padding = &text[trimmed.len()..];
    format!("\x1b[{code}m{trimmed}\x1b[0m{padding}")
}

fn truncate_start(s: &str, max_width: usize) -> String {
    if s.width() <= max_width {
        return s.to_string();
    }
    if max_width <= 1 {
        return String::new();
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

    fn view(phase: Phase, total: u64) -> LiveHandle {
        LiveHandle {
            phase: Arc::new(AtomicU8::new(phase.code())),
            total: Arc::new(AtomicU64::new(total)),
            scanned: Arc::new(AtomicUsize::new(0)),
        }
    }

    #[test]
    fn scan_phase_counts_files() {
        let handle = view(Phase::Scan, 0);
        handle.set_scanned(128);
        let line = render(&handle, &Stats::new(), 80, Style::PLAIN);
        assert_eq!(line, "SCAN   128 个文件");
    }

    #[test]
    fn line_shows_percent_bytes_and_file() {
        let stats = Stats::new();
        stats.copied_bytes.store(3_000_000_000, Ordering::Relaxed);
        stats.set_current("A001_06141325_C026.mov");
        let handle = view(Phase::Copy, 3_000_000_000);
        let line = render(&handle, &stats, 100, Style::PLAIN);
        assert!(line.starts_with("COPY  "), "{line}");
        assert!(line.contains("50%"), "复制占全程一半：{line}");
        assert!(line.ends_with("A001_06141325_C026.mov"), "{line}");
        assert!(line.width() <= 100, "{line}");
    }

    #[test]
    fn verify_counts_both_reads_as_one_pass() {
        let stats = Stats::new();
        stats.copied_bytes.store(1000, Ordering::Relaxed);
        // 校验两侧各读 2000 字节，只计 2000（除以二）。
        stats.hashed_bytes.store(4000, Ordering::Relaxed);
        assert_eq!(done_bytes(&stats), 3000);
    }

    #[test]
    fn complete_run_reaches_one_hundred_percent() {
        let stats = Stats::new();
        stats.copied_bytes.store(5_000, Ordering::Relaxed);
        stats.hashed_bytes.store(10_000, Ordering::Relaxed);
        let handle = view(Phase::Verify, 5_000);
        let line = render(&handle, &stats, 80, Style::PLAIN);
        assert!(line.contains("100%"), "{line}");
    }

    #[test]
    fn numbers_never_exceed_one_hundred_percent() {
        let stats = Stats::new();
        stats.copied_bytes.store(9_999, Ordering::Relaxed);
        stats.hashed_bytes.store(99_999, Ordering::Relaxed);
        let handle = view(Phase::Verify, 5_000);
        let line = render(&handle, &stats, 80, Style::PLAIN);
        assert!(line.contains("100%"), "{line}");
    }

    #[test]
    fn both_phases_align_the_file_name() {
        let stats = Stats::new();
        stats.set_current("x.mov");
        let copy = render(&view(Phase::Copy, 0), &stats, 80, Style::PLAIN);
        let verify = render(&view(Phase::Verify, 0), &stats, 80, Style::PLAIN);
        let column = |line: &str| line[..line.find('x').unwrap()].width();
        assert_eq!(column(&copy), column(&verify), "文件名列应对齐");
        assert!(verify.starts_with("VERIFY"), "{verify}");
    }

    #[test]
    fn long_names_keep_their_tail() {
        let stats = Stats::new();
        stats.set_current("素材目录_第一期_相机A_0001.MOV");
        let line = render(&view(Phase::Verify, 1_000_000), &stats, 52, Style::PLAIN);
        assert!(line.width() <= 52, "{line}");
        assert!(line.ends_with("_0001.MOV"), "保留扩展名：{line}");
    }

    #[test]
    fn narrow_terminals_drop_fields_in_order() {
        let stats = Stats::new();
        stats.set_current("A001_06141325_C026.mov");
        let handle = view(Phase::Copy, 1_000_000);
        for width in [10usize, 12, 16, 20, 24, 30, 40, 60, 120] {
            let line = render(&handle, &stats, width, Style::PLAIN);
            assert!(line.width() <= width, "宽度 {width} 时超宽：{line}");
        }
    }

    #[test]
    fn color_style_only_paints_the_label() {
        let stats = Stats::new();
        stats.set_current("x.mov");
        let handle = view(Phase::Copy, 1_000);
        let plain = render(&handle, &stats, 80, Style::PLAIN);
        assert!(!plain.contains('\x1b'), "纯文本模式不应带 SGR");

        let colored = render(&handle, &stats, 80, Style { color: true });
        assert!(colored.starts_with("\x1b[2mCOPY\x1b[0m"), "{colored}");
        assert!(colored.ends_with("x.mov"), "{colored}");
    }

    #[test]
    fn byte_columns_share_one_unit() {
        // 分母 11.0 GiB，分子用同一单位，宽度与分母一致，重绘不跳动。
        assert_eq!(
            bytes_columns(0, 11 * 1024 * 1024 * 1024),
            " 0.0/11.0 GiB".to_string()
        );
        assert_eq!(
            bytes_columns(7 * 1024 * 1024 * 1024, 11 * 1024 * 1024 * 1024),
            " 7.0/11.0 GiB".to_string()
        );
    }

    #[test]
    fn small_totals_use_their_own_unit() {
        // 与 human_bytes 同一套进位：1500 字节写作 1.5 KiB。
        assert_eq!(bytes_columns(0, 1500), "0.0/1.5 KiB".to_string());
        assert_eq!(bytes_columns(0, 900), "  0/900 B".to_string());
    }
}
