//! 运行统计：原子计数、速度滑窗、有界失败清单、取消与中止标志。

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// 失败样例条数上限。
pub const MAX_FAILURE_EXAMPLES: usize = 32;
/// 失败样例明细的字节上限。
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

/// 三个线程共享的运行状态。
pub struct Stats {
    pub read_bytes: AtomicU64,
    pub written_bytes: AtomicU64,
    pub hashed_bytes: AtomicU64,
    pub in_flight_bytes: AtomicU64,
    pub files_ok: AtomicUsize,
    pub files_failed: AtomicUsize,
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
            read_bytes: AtomicU64::new(0),
            written_bytes: AtomicU64::new(0),
            hashed_bytes: AtomicU64::new(0),
            in_flight_bytes: AtomicU64::new(0),
            files_ok: AtomicUsize::new(0),
            files_failed: AtomicUsize::new(0),
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

    /// 只记录一条失败样例，不增加失败计数（用于跳过等已在其它计数中统计的情况）。
    pub fn note_failure(&self, message: String) {
        if let Ok(mut log) = self.failures.lock() {
            log.record(message);
        }
    }

    /// 记录一条失败，并同步累计失败文件数。
    pub fn record_failure(&self, message: String) {
        self.files_failed.fetch_add(1, Ordering::Relaxed);
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

/// 1 秒滑窗的指数加权速度计。
pub struct SpeedMeter {
    last_bytes: u64,
    last_at: Instant,
    ema: f64,
    initialized: bool,
}

impl SpeedMeter {
    pub fn new() -> Self {
        Self {
            last_bytes: 0,
            last_at: Instant::now(),
            ema: 0.0,
            initialized: false,
        }
    }

    /// 采样一次累计字节数，返回当前速度（字节每秒）。
    pub fn sample(&mut self, total_bytes: u64, now: Instant) -> f64 {
        let elapsed = now.saturating_duration_since(self.last_at).as_secs_f64();
        if elapsed < 0.05 {
            return self.ema;
        }
        let delta = total_bytes.saturating_sub(self.last_bytes) as f64;
        let instant = delta / elapsed;
        self.last_bytes = total_bytes;
        self.last_at = now;
        if !self.initialized {
            self.ema = instant;
            self.initialized = true;
        } else {
            // 约 1 秒窗口的 EMA（每次采样间隔约 100 ms）。
            self.ema = 0.7 * self.ema + 0.3 * instant;
        }
        self.ema
    }

    pub fn speed(&self) -> f64 {
        self.ema
    }
}

impl Default for SpeedMeter {
    fn default() -> Self {
        Self::new()
    }
}

/// 把字节数格式化为便于阅读的单位（二进制前缀）。
pub fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

pub fn human_speed(bytes_per_sec: f64) -> String {
    if bytes_per_sec <= 0.0 {
        return "0 B/s".to_string();
    }
    format!("{}/s", human_bytes(bytes_per_sec.round() as u64))
}

/// 把秒数格式化为 mm:ss 或 hh:mm:ss。
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

/// 按速度估算剩余时间。
pub fn eta_seconds(remaining_bytes: u64, bytes_per_sec: f64) -> Option<u64> {
    if bytes_per_sec < 64.0 * 1024.0 {
        return None;
    }
    Some((remaining_bytes as f64 / bytes_per_sec).ceil() as u64)
}

pub fn elapsed(seconds: f64) -> Duration {
    Duration::from_secs_f64(seconds.max(0.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failure_log_is_bounded() {
        let stats = Stats::new();
        for i in 0..100 {
            stats.record_failure(format!("失败 {i}"));
        }
        assert_eq!(stats.failure_count(), 100);
        assert_eq!(stats.failure_examples().len(), MAX_FAILURE_EXAMPLES);
        assert_eq!(stats.files_failed.load(Ordering::Relaxed), 100);
    }

    #[test]
    fn failure_detail_bytes_are_bounded() {
        let stats = Stats::new();
        let long = "x".repeat(4096);
        for _ in 0..100 {
            stats.record_failure(long.clone());
        }
        assert!(stats.failure_examples().len() <= MAX_FAILURE_DETAIL_BYTES / 4096 + 1);
    }

    #[test]
    fn speed_meter_converges() {
        let mut meter = SpeedMeter::new();
        let start = Instant::now();
        let mut total = 0u64;
        for step in 1..=10u64 {
            total += 1024 * 1024;
            let now = start + Duration::from_millis(100 * step);
            meter.sample(total, now);
        }
        let speed = meter.speed();
        // 每 100 ms 累计 1 MiB，即约 10 MiB/s。
        assert!(
            (9_000_000.0..=11_000_000.0).contains(&speed),
            "速度应接近 10 MiB/s，实际 {speed}"
        );
    }

    #[test]
    fn human_units() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(1536), "1.5 KiB");
        assert_eq!(human_duration(65), "01:05");
        assert_eq!(human_duration(3725), "01:02:05");
        assert_eq!(eta_seconds(1_000_000, 1000.0), None);
        assert_eq!(eta_seconds(1_000_000, 500_000.0), Some(2));
    }
}
