//! Run state shared between the worker threads: atomic byte counters, a bounded
//! failure list, cancellation flags and the current file.

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Mutex;

/// Upper bound on kept failure examples.
pub const MAX_FAILURE_EXAMPLES: usize = 32;
/// Upper bound on the bytes of kept failure detail.
pub const MAX_FAILURE_DETAIL_BYTES: usize = 64 * 1024;

#[derive(Default)]
struct FailureLog {
    count: usize,
    detail_bytes: usize,
    examples: Vec<String>,
}

impl FailureLog {
    fn record(&mut self, message: String) {
        self.count += 1;
        if self.examples.len() >= MAX_FAILURE_EXAMPLES {
            return;
        }
        if self.detail_bytes + message.len() > MAX_FAILURE_DETAIL_BYTES {
            return;
        }
        self.detail_bytes += message.len();
        self.examples.push(message);
    }
}

/// State shared by the copy observer, the verify loop and the progress line.
pub struct Stats {
    /// Bytes handled by the copy phase: added when a file is created, updated or skipped.
    pub copied_bytes: AtomicU64,
    /// Bytes read during verification. Both sides are read, so this is twice the file size.
    pub hashed_bytes: AtomicU64,
    pub files_skipped: AtomicUsize,
    pub verify_ok: AtomicUsize,
    pub verify_bad: AtomicUsize,
    pub cancel: AtomicBool,
    pub abort: AtomicBool,
    abort_reason: Mutex<Option<String>>,
    current: Mutex<String>,
    failures: Mutex<FailureLog>,
}

impl Default for Stats {
    fn default() -> Self {
        Self::new()
    }
}

impl Stats {
    pub fn new() -> Self {
        Self {
            copied_bytes: AtomicU64::new(0),
            hashed_bytes: AtomicU64::new(0),
            files_skipped: AtomicUsize::new(0),
            verify_ok: AtomicUsize::new(0),
            verify_bad: AtomicUsize::new(0),
            cancel: AtomicBool::new(false),
            abort: AtomicBool::new(false),
            abort_reason: Mutex::new(None),
            current: Mutex::new(String::new()),
            failures: Mutex::new(FailureLog::default()),
        }
    }

    pub fn set_current(&self, name: impl Into<String>) {
        if let Ok(mut guard) = self.current.lock() {
            *guard = name.into();
        }
    }

    pub fn current(&self) -> String {
        self.current.lock().map(|g| g.clone()).unwrap_or_default()
    }

    /// Record one failure; both the count and the detail are bounded.
    pub fn note_failure(&self, message: String) {
        if let Ok(mut log) = self.failures.lock() {
            log.record(message);
        }
    }

    pub fn failure_count(&self) -> usize {
        self.failures.lock().map(|l| l.count).unwrap_or(0)
    }

    pub fn failure_examples(&self) -> Vec<String> {
        self.failures
            .lock()
            .map(|l| l.examples.clone())
            .unwrap_or_default()
    }

    pub fn request_abort(&self, reason: impl Into<String>) {
        let reason = reason.into();
        if !self.abort.swap(true, Ordering::SeqCst) {
            if let Ok(mut guard) = self.abort_reason.lock() {
                *guard = Some(reason);
            }
        }
    }

    pub fn abort_reason(&self) -> Option<String> {
        self.abort_reason.lock().ok().and_then(|g| g.clone())
    }

    pub fn stop_requested(&self) -> bool {
        self.cancel.load(Ordering::Relaxed) || self.abort.load(Ordering::Relaxed)
    }
}

/// Binary prefixes.
pub const BYTE_UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];

/// Split a byte count into value and unit so callers can align columns.
pub fn human_bytes_parts(bytes: u64) -> (f64, &'static str) {
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < BYTE_UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    (value, BYTE_UNITS[unit])
}

/// Format a byte count with binary prefixes.
pub fn human_bytes(bytes: u64) -> String {
    let (value, unit) = human_bytes_parts(bytes);
    if unit == "B" {
        format!("{} {unit}", value.round() as u64)
    } else {
        format!("{value:.1} {unit}")
    }
}

/// Format seconds as mm:ss or hh:mm:ss.
pub fn human_duration(seconds: u64) -> String {
    let s = seconds % 60;
    let m = (seconds / 60) % 60;
    let h = seconds / 3600;
    if h > 0 {
        format!("{h:02}:{m:02}:{s:02}")
    } else {
        format!("{m:02}:{s:02}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failure_log_is_bounded() {
        let stats = Stats::new();
        for i in 0..100 {
            stats.note_failure(format!("failure {i}"));
        }
        assert_eq!(stats.failure_count(), 100);
        assert_eq!(stats.failure_examples().len(), MAX_FAILURE_EXAMPLES);
    }

    #[test]
    fn failure_detail_bytes_are_bounded() {
        let stats = Stats::new();
        let long = "x".repeat(4096);
        for _ in 0..100 {
            stats.note_failure(long.clone());
        }
        assert!(stats.failure_examples().len() <= MAX_FAILURE_DETAIL_BYTES / 4096 + 1);
    }

    #[test]
    fn human_units() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(1536), "1.5 KiB");
        assert_eq!(human_duration(65), "01:05");
        assert_eq!(human_duration(3725), "01:02:05");
    }
}
