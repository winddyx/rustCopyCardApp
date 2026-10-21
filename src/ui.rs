//! The single line of progress drawn while rcca runs: phase, overall percentage,
//! done/total bytes and the current file.
//!
//! No progress bars, no speed and no ETA. The line is redrawn in place on stderr.
//! When stderr is not a TTY or `--quiet` is given nothing is drawn at all and
//! stdout carries the summary only.

use std::io::{self, IsTerminal, Write};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use unicode_width::UnicodeWidthStr;

use crate::stats::{Stats, BYTE_UNITS};

/// Phases shown in the label column.
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

/// Label column width: `VERIFY` is the longest, so the file column never moves.
const LABEL_W: usize = 6;
/// Percentage column width, `%` included.
const PCT_W: usize = 4;
/// Gap between the fixed columns.
const GAP: usize = 2;
/// Minimum width left for the file name; below that the name is dropped entirely.
const MIN_FILE: usize = 8;

/// How the line is painted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Style {
    pub color: bool,
}

impl Style {
    /// Plain text, used by tests and piped output.
    pub const PLAIN: Self = Self { color: false };
}

const SGR_DIM: &str = "2";

/// Write side: it only updates a few numbers, the ticker thread does the drawing.
#[derive(Clone)]
pub struct LiveHandle {
    phase: Arc<AtomicU8>,
    total: Arc<AtomicU64>,
    scanned: Arc<AtomicUsize>,
}

impl LiveHandle {
    /// Number of files seen so far during the scan phase.
    pub fn set_scanned(&self, files: usize) {
        self.scanned.store(files, Ordering::Relaxed);
    }

    /// Total source bytes; twice that is the denominator of the whole run. Called
    /// once the scan is done.
    pub fn set_total(&self, bytes: u64) {
        self.total.store(bytes, Ordering::Relaxed);
    }

    /// Switch phase. The current file comes from `Stats`, so it follows on its own.
    pub fn set_phase(&self, phase: Phase) {
        self.phase.store(phase.code(), Ordering::Relaxed);
    }
}

/// Drawing side: while it runs, it is the only writer on stderr.
pub struct Live {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl Live {
    /// When `enabled` is false (stderr not a TTY, or --quiet) nothing is drawn and
    /// no thread is started.
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

    /// Stop refreshing and erase the line before the summary is printed.
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

/// Bytes accounted so far: bytes handled by the copy plus bytes covered by
/// verification (both sides are read, so that count is halved).
pub fn done_bytes(stats: &Stats) -> u64 {
    stats.copied_bytes.load(Ordering::Relaxed) + stats.hashed_bytes.load(Ordering::Relaxed) / 2
}

fn render(state: &LiveHandle, stats: &Stats, width: usize, style: Style) -> String {
    let phase = Phase::from_code(state.phase.load(Ordering::Relaxed));
    let label = paint(&pad_label(phase.label()), SGR_DIM, style);
    if phase == Phase::Scan {
        let scanned = state.scanned.load(Ordering::Relaxed);
        return format!("{label} {scanned} files");
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
            .trim_end()
            .to_string()
    } else if width >= LABEL_W + 1 + PCT_W {
        format!("{label} {pct}")
    } else {
        truncate_end(&format!("{label} {pct}"), width)
    }
}

/// Numerator and denominator share the denominator's unit, so the digits stay in
/// place while the line is redrawn.
fn bytes_columns(done: u64, denom: u64) -> String {
    let index = unit_index(denom);
    let unit = BYTE_UNITS[index];
    let den = format_value(denom, index);
    let num = format_value(done.min(denom), index);
    format!("{num:>width$}/{den} {unit}", width = den.width())
}

/// Same stepping rule as `human_bytes`, so units match the summary.
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
    // Keep trailing padding outside the SGR span.
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

/// Whether the line should be drawn: stderr is a TTY and --quiet was not given.
pub fn interactive(quiet: bool) -> bool {
    io::stderr().is_terminal() && !quiet
}

/// Colour policy: an explicit switch wins, otherwise NO_COLOR, TERM=dumb and TTY.
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
        assert_eq!(line, "SCAN   128 files");
    }

    #[test]
    fn line_shows_percent_bytes_and_file() {
        let stats = Stats::new();
        stats.copied_bytes.store(3_000_000_000, Ordering::Relaxed);
        stats.set_current("A001_06141325_C026.mov");
        let handle = view(Phase::Copy, 3_000_000_000);
        let line = render(&handle, &stats, 100, Style::PLAIN);
        assert!(line.starts_with("COPY  "), "{line}");
        assert!(line.contains("50%"), "the copy half is 50%: {line}");
        assert!(line.ends_with("A001_06141325_C026.mov"), "{line}");
        assert!(line.width() <= 100, "{line}");
    }

    #[test]
    fn verify_counts_both_reads_as_one_pass() {
        let stats = Stats::new();
        stats.copied_bytes.store(1000, Ordering::Relaxed);
        // Both sides read 2000 bytes each; only 2000 counts (halved).
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
        assert_eq!(column(&copy), column(&verify), "the file column must align");
        assert!(verify.starts_with("VERIFY"), "{verify}");
    }

    #[test]
    fn long_names_keep_their_tail() {
        let stats = Stats::new();
        stats.set_current("session_01_cameraA_0001.MOV");
        let line = render(&view(Phase::Verify, 1_000_000), &stats, 52, Style::PLAIN);
        assert!(line.width() <= 52, "{line}");
        assert!(line.ends_with("_0001.MOV"), "keep the extension: {line}");
    }

    #[test]
    fn narrow_terminals_drop_fields_in_order() {
        let stats = Stats::new();
        stats.set_current("A001_06141325_C026.mov");
        let handle = view(Phase::Copy, 1_000_000);
        for width in [10usize, 12, 16, 20, 24, 30, 40, 60, 120] {
            let line = render(&handle, &stats, width, Style::PLAIN);
            assert!(line.width() <= width, "too wide at {width}: {line}");
        }
    }

    #[test]
    fn color_style_only_paints_the_label() {
        let stats = Stats::new();
        stats.set_current("x.mov");
        let handle = view(Phase::Copy, 1_000);
        let plain = render(&handle, &stats, 80, Style::PLAIN);
        assert!(!plain.contains('\x1b'), "plain text must not carry SGR");

        let colored = render(&handle, &stats, 80, Style { color: true });
        assert!(colored.starts_with("\x1b[2mCOPY\x1b[0m"), "{colored}");
        assert!(colored.ends_with("x.mov"), "{colored}");
    }

    #[test]
    fn byte_columns_share_one_unit() {
        // Denominator 11.0 GiB: the numerator uses the same unit and width.
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
        // Same stepping as human_bytes: 1500 bytes is 1.5 KiB.
        assert_eq!(bytes_columns(0, 1500), "0.0/1.5 KiB".to_string());
        assert_eq!(bytes_columns(0, 900), "  0/900 B".to_string());
    }
}
