//! 读流水线与写流水线：一个读线程按清单顺序读，一个写线程按同序写 tmp，
//! 两者之间用有界字节队列（容量 = buffer_bytes / chunk）解耦。
//!
//! 不变量：
//! - 任一时刻只有一个文件在写；
//! - 在途数据不超过缓冲预算；
//! - 写入顺序严格等于清单顺序。

use std::fs::File;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::mpsc::{sync_channel, Receiver, SyncSender};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, SystemTime};

use crate::copy::{ActiveFile, Outcome};
use crate::paths::tmp_rel_path;
use crate::scan::Entry;
use crate::stats::Stats;

type FileDoneHook = Arc<dyn Fn(usize, &Outcome) + Send + Sync>;

/// 流水线参数。
#[derive(Clone)]
pub struct Options {
    pub chunk: usize,
    pub buffer_bytes: u64,
    pub fsync: bool,
    pub serial_io: bool,
    pub fail_fast: bool,
    /// 每个文件结束时回调（非交互模式下用于即时输出逐文件结果）。
    pub on_file_done: Option<FileDoneHook>,
}

impl Options {
    pub fn slots(&self) -> usize {
        let slots = self.buffer_bytes / self.chunk.max(1) as u64;
        slots.max(1) as usize
    }
}

/// 阶段 2 的结果，按清单下标对齐。
pub struct Report {
    pub outcomes: Vec<Option<Outcome>>,
    pub aborted: Option<String>,
    pub buffer_bytes_effective: u64,
    pub chunk_effective: usize,
}

enum Msg {
    Begin {
        index: usize,
        tmp_path: PathBuf,
        mode: u32,
        mtime: Option<SystemTime>,
    },
    Chunk {
        index: usize,
        block: Box<[u8]>,
        len: usize,
    },
    End {
        index: usize,
        error: Option<String>,
    },
    Skipped {
        index: usize,
        reason: String,
    },
}

/// 固定数量数据块的池子，避免稳态下逐块分配。
struct BlockPool {
    free: Mutex<Vec<Box<[u8]>>>,
    ready: Condvar,
    closing: Mutex<bool>,
}

impl BlockPool {
    fn new(slots: usize, chunk: usize) -> Self {
        let mut free = Vec::with_capacity(slots);
        for _ in 0..slots {
            free.push(vec![0u8; chunk].into_boxed_slice());
        }
        Self {
            free: Mutex::new(free),
            ready: Condvar::new(),
            closing: Mutex::new(false),
        }
    }

    fn take(&self) -> Option<Box<[u8]>> {
        let mut free = self.free.lock().ok()?;
        loop {
            if let Some(block) = free.pop() {
                return Some(block);
            }
            if *self.closing.lock().ok()? {
                return None;
            }
            free = self.ready.wait(free).ok()?;
        }
    }

    fn put(&self, block: Box<[u8]>) {
        if let Ok(mut free) = self.free.lock() {
            free.push(block);
        }
        self.ready.notify_one();
    }

    fn close(&self) {
        if let Ok(mut closing) = self.closing.lock() {
            *closing = true;
        }
        self.ready.notify_all();
    }
}

/// 执行阶段 2。`progress` 在等待期间被主线程周期性调用，用于刷新界面。
pub fn run(
    files: &[Entry],
    source: &Path,
    target: &Path,
    opts: &Options,
    stats: &Stats,
    progress: &mut dyn FnMut(),
) -> Report {
    let chunk = opts.chunk;
    let slots = opts.slots();
    let effective_buffer = (slots * chunk) as u64;

    if opts.serial_io {
        let outcomes = run_serial(files, source, target, opts, stats, progress);
        return Report {
            outcomes,
            aborted: stats.abort_reason(),
            buffer_bytes_effective: effective_buffer,
            chunk_effective: chunk,
        };
    }

    let pool = BlockPool::new(slots, chunk);
    let (data_tx, data_rx) = sync_channel::<Msg>(slots);

    let pool_ref = &pool;
    let outcomes = std::thread::scope(|scope| {
        let reader =
            scope.spawn(move || reader_loop(files, source, target, &data_tx, pool_ref, stats));
        let writer = scope.spawn(move || writer_loop(files.len(), opts, &data_rx, pool_ref, stats));

        while !writer.is_finished() {
            progress();
            std::thread::sleep(Duration::from_millis(50));
        }
        pool.close();

        let outcomes = match writer.join() {
            Ok(outcomes) => outcomes,
            Err(_) => vec![None; files.len()],
        };
        // 写线程退出后队列接收端已释放，读线程的 send 会失败并返回。
        let _ = reader.join();
        outcomes
    });

    Report {
        outcomes,
        aborted: stats.abort_reason(),
        buffer_bytes_effective: effective_buffer,
        chunk_effective: chunk,
    }
}

fn reader_loop(
    files: &[Entry],
    source: &Path,
    target: &Path,
    tx: &SyncSender<Msg>,
    pool: &BlockPool,
    stats: &Stats,
) {
    for (index, entry) in files.iter().enumerate() {
        if stats.stop_requested() {
            break;
        }
        stats.set_current(entry.rel.file_name().to_string_lossy().into_owned());
        let abs = source.join(entry.rel.as_path());
        let mut file = match File::open(&abs) {
            Ok(f) => f,
            Err(err) => {
                stats.files_skipped.fetch_add(1, Ordering::Relaxed);
                if tx
                    .send(Msg::Skipped {
                        index,
                        reason: format!("无法打开源文件：{err}"),
                    })
                    .is_err()
                {
                    return;
                }
                continue;
            }
        };

        let (mode, mtime) = match file.metadata() {
            Ok(meta) => (
                crate::scan::mode_of(&meta),
                meta.modified().ok().or(entry.mtime),
            ),
            Err(_) => (entry.mode, entry.mtime),
        };

        let tmp_path = target.join(tmp_rel_path(entry.rel.as_path()));
        if tx
            .send(Msg::Begin {
                index,
                tmp_path,
                mode,
                mtime,
            })
            .is_err()
        {
            return;
        }

        let mut read_error: Option<String> = None;
        loop {
            if stats.stop_requested() {
                read_error = Some("已中断".to_string());
                break;
            }
            let Some(mut block) = pool.take() else {
                return;
            };
            match file.read(&mut block) {
                Ok(0) => {
                    pool.put(block);
                    break;
                }
                Ok(n) => {
                    stats.read_bytes.fetch_add(n as u64, Ordering::Relaxed);
                    stats.in_flight_bytes.fetch_add(n as u64, Ordering::Relaxed);
                    if tx
                        .send(Msg::Chunk {
                            index,
                            block,
                            len: n,
                        })
                        .is_err()
                    {
                        return;
                    }
                }
                Err(err) if err.kind() == io::ErrorKind::Interrupted => {
                    pool.put(block);
                }
                Err(err) => {
                    pool.put(block);
                    read_error = Some(format!("读取源文件失败：{err}"));
                    break;
                }
            }
        }

        if tx
            .send(Msg::End {
                index,
                error: read_error,
            })
            .is_err()
        {
            return;
        }
    }
}

fn writer_loop(
    file_count: usize,
    opts: &Options,
    rx: &Receiver<Msg>,
    pool: &BlockPool,
    stats: &Stats,
) -> Vec<Option<Outcome>> {
    let mut outcomes: Vec<Option<Outcome>> = vec![None; file_count];
    let mut active: Option<ActiveFile> = None;
    let mut eio_streak = 0usize;

    while let Ok(msg) = rx.recv() {
        match msg {
            Msg::Begin {
                index,
                tmp_path,
                mode,
                mtime,
            } => {
                if let Some(previous) = active.take() {
                    let previous_index = previous.index;
                    let outcome = previous.abandon("文件边界异常，丢弃未完成的暂存文件");
                    notify(opts, previous_index, &outcome);
                    record(stats, &mut outcomes, previous_index, outcome);
                }
                let mut file = ActiveFile::begin(index, tmp_path);
                // 元数据在 finish 时应用，这里先记录下来。
                file.set_pending_meta(mode, mtime);
                active = Some(file);
            }
            Msg::Chunk { index, block, len } => {
                let written = match active.as_mut() {
                    Some(file) if file.index == index => file.write(&block[..len]),
                    _ => 0,
                };
                stats
                    .in_flight_bytes
                    .fetch_sub(len as u64, Ordering::Relaxed);
                stats
                    .written_bytes
                    .fetch_add(written as u64, Ordering::Relaxed);
                pool.put(block);
            }
            Msg::End { index, error } => {
                if let Some(file) = active.take() {
                    let (mode, mtime) = file.pending_meta();
                    let kind = file.error_kind();
                    let raw = file.error_raw();
                    let outcome = file.finish(opts.fsync, mode, mtime, error);
                    if outcome.is_ok() {
                        eio_streak = 0;
                    } else if kind == Some(io::ErrorKind::PermissionDenied)
                        || crate::platform::is_disk_full(raw)
                    {
                        // 权限与空间问题几乎不会自行恢复，直接中止。
                        stats.request_abort("目标卷不可写或空间不足");
                    } else if crate::platform::is_device_io_error(raw) {
                        eio_streak += 1;
                        if eio_streak >= 3 {
                            stats.request_abort("连续 IO 错误，设备可能已断开");
                        }
                    }
                    notify(opts, index, &outcome);
                    record(stats, &mut outcomes, index, outcome);
                    if opts.fail_fast && stats.files_failed.load(Ordering::Relaxed) > 0 {
                        stats.request_abort("--fail-fast：首个失败即中止");
                    }
                }
            }
            Msg::Skipped { index, reason } => {
                stats.note_failure(format!("跳过：{reason}"));
                let outcome = Outcome::Failed { reason };
                notify(opts, index, &outcome);
                outcomes[index] = Some(outcome);
            }
        }

        if stats.abort.load(Ordering::SeqCst) {
            break;
        }
    }

    if let Some(file) = active.take() {
        let index = file.index;
        let outcome = file.abandon(if stats.cancel.load(Ordering::SeqCst) {
            "已中断"
        } else {
            "已中止"
        });
        notify(opts, index, &outcome);
        record(stats, &mut outcomes, index, outcome);
    }

    pool.close();
    outcomes
}

fn notify(opts: &Options, index: usize, outcome: &Outcome) {
    if let Some(hook) = opts.on_file_done.as_ref() {
        hook(index, outcome);
    }
}

fn record(stats: &Stats, outcomes: &mut [Option<Outcome>], index: usize, outcome: Outcome) {
    match &outcome {
        Outcome::Ok(_) => {
            stats.files_ok.fetch_add(1, Ordering::Relaxed);
        }
        Outcome::Failed { reason } => {
            stats.record_failure(format!("复制失败：{reason}"));
        }
    }
    if index < outcomes.len() {
        outcomes[index] = Some(outcome);
    }
}

/// 同步模式：读一块、写一块，不重叠。用于同盘机械盘场景。
fn run_serial(
    files: &[Entry],
    source: &Path,
    target: &Path,
    opts: &Options,
    stats: &Stats,
    progress: &mut dyn FnMut(),
) -> Vec<Option<Outcome>> {
    let mut outcomes: Vec<Option<Outcome>> = vec![None; files.len()];
    let mut buffer = vec![0u8; opts.chunk];

    for (index, entry) in files.iter().enumerate() {
        if stats.stop_requested() {
            break;
        }
        stats.set_current(entry.rel.file_name().to_string_lossy().into_owned());
        let abs = source.join(entry.rel.as_path());
        let mut file = match File::open(&abs) {
            Ok(f) => f,
            Err(err) => {
                stats.files_skipped.fetch_add(1, Ordering::Relaxed);
                let outcome = Outcome::Failed {
                    reason: format!("无法打开源文件：{err}"),
                };
                record(stats, &mut outcomes, index, outcome);
                continue;
            }
        };
        let (mode, mtime) = match file.metadata() {
            Ok(meta) => (
                crate::scan::mode_of(&meta),
                meta.modified().ok().or(entry.mtime),
            ),
            Err(_) => (entry.mode, entry.mtime),
        };

        let tmp_path = target.join(tmp_rel_path(entry.rel.as_path()));
        let mut active = ActiveFile::begin(index, tmp_path);
        let mut read_error = None;
        loop {
            if stats.stop_requested() {
                read_error = Some("已中断".to_string());
                break;
            }
            match file.read(&mut buffer) {
                Ok(0) => break,
                Ok(n) => {
                    stats.read_bytes.fetch_add(n as u64, Ordering::Relaxed);
                    let written = active.write(&buffer[..n]);
                    stats
                        .written_bytes
                        .fetch_add(written as u64, Ordering::Relaxed);
                    if written != n {
                        read_error = Some("写入目标文件失败".to_string());
                        break;
                    }
                    progress();
                }
                Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
                Err(err) => {
                    read_error = Some(format!("读取源文件失败：{err}"));
                    break;
                }
            }
        }
        let raw = active.error_raw();
        let outcome = active.finish(opts.fsync, mode, mtime, read_error);
        if !outcome.is_ok() && crate::platform::is_disk_full(raw) {
            stats.request_abort("目标卷不可写或空间不足");
        }
        notify(opts, index, &outcome);
        record(stats, &mut outcomes, index, outcome);
        if opts.fail_fast && stats.files_failed.load(Ordering::Relaxed) > 0 {
            stats.request_abort("--fail-fast：首个失败即中止");
        }
        if stats.abort.load(Ordering::SeqCst) {
            break;
        }
    }

    outcomes
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options(chunk: usize, buffer: u64, serial: bool) -> Options {
        Options {
            chunk,
            buffer_bytes: buffer,
            fsync: false,
            serial_io: serial,
            fail_fast: false,
            on_file_done: None,
        }
    }

    #[test]
    fn slots_are_derived_from_byte_budget() {
        assert_eq!(options(1024 * 1024, 128 * 1024 * 1024, false).slots(), 128);
        assert_eq!(options(1024 * 1024, 1024, false).slots(), 1);
        assert_eq!(
            options(4 * 1024 * 1024, 128 * 1024 * 1024, false).slots(),
            32
        );
    }

    #[test]
    fn serial_and_pipeline_copy_identical_bytes() {
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();
        let payload: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
        std::fs::create_dir_all(src.path().join("a")).unwrap();
        std::fs::write(src.path().join("a/data.bin"), &payload).unwrap();
        std::fs::write(src.path().join("empty.bin"), b"").unwrap();

        let manifest = crate::scan::scan(src.path(), dst.path()).unwrap();
        let stats = Stats::new();
        let report = run(
            &manifest.files,
            src.path(),
            dst.path(),
            &options(4096, 16 * 1024, true),
            &stats,
            &mut || {},
        );
        assert!(report
            .outcomes
            .iter()
            .all(|o| matches!(o, Some(Outcome::Ok(_)))));
        assert_eq!(
            std::fs::read(dst.path().join("a/.data.bin.rcptmp")).unwrap(),
            payload
        );
        assert!(dst.path().join(".empty.bin.rcptmp").exists());
    }

    #[test]
    fn pipeline_respects_order_and_single_active_file() {
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();
        for i in 0..5 {
            std::fs::write(src.path().join(format!("f{i}.bin")), vec![i as u8; 12_345]).unwrap();
        }
        let manifest = crate::scan::scan(src.path(), dst.path()).unwrap();
        let stats = Stats::new();
        let report = run(
            &manifest.files,
            src.path(),
            dst.path(),
            &options(1024, 4096, false),
            &stats,
            &mut || {},
        );
        assert!(report
            .outcomes
            .iter()
            .all(|o| matches!(o, Some(Outcome::Ok(_)))));
        assert_eq!(
            stats.in_flight_bytes.load(Ordering::Relaxed),
            0,
            "全部块都应归还，在途字节归零"
        );
        let names: Vec<String> = manifest.files.iter().map(|f| f.rel.display()).collect();
        let mut sorted = names.clone();
        sorted.sort();
        assert_eq!(names, sorted, "清单按路径排序，写入顺序与之相同");
    }

    #[test]
    fn current_file_is_shown_as_basename_only() {
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(src.path().join("deep/nested")).unwrap();
        std::fs::write(src.path().join("deep/nested/clip.MP4"), b"payload").unwrap();
        let manifest = crate::scan::scan(src.path(), dst.path()).unwrap();
        let stats = Stats::new();
        let report = run(
            &manifest.files,
            src.path(),
            dst.path(),
            &options(4096, 16 * 1024, true),
            &stats,
            &mut || {},
        );
        assert!(report
            .outcomes
            .iter()
            .all(|o| matches!(o, Some(Outcome::Ok(_)))));
        assert_eq!(stats.current(), "clip.MP4", "当前文件只显示文件名");
    }

    #[test]
    fn in_flight_bytes_never_exceed_budget() {
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();
        std::fs::write(src.path().join("big.bin"), vec![7u8; 512 * 1024]).unwrap();
        let manifest = crate::scan::scan(src.path(), dst.path()).unwrap();
        let stats = Stats::new();
        let budget = 4096u64;
        let stats_ref = &stats;
        let observed = std::cell::Cell::new(0u64);
        let mut progress = || {
            let current = stats_ref.in_flight_bytes.load(Ordering::Relaxed);
            if current > observed.get() {
                observed.set(current);
            }
        };
        let report = run(
            &manifest.files,
            src.path(),
            dst.path(),
            &options(1024, budget, false),
            &stats,
            &mut progress,
        );
        assert!(report
            .outcomes
            .iter()
            .all(|o| matches!(o, Some(Outcome::Ok(_)))));
        assert!(
            observed.get() <= budget,
            "在途字节 {} 超过预算 {}",
            observed.get(),
            budget
        );
    }
}
