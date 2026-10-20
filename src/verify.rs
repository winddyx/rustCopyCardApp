//! 阶段 4：逐个文件计算源与目标的 XXH64 并比较。

use std::fs::{self, File};
use std::io::{self, Read};
use std::path::Path;
use std::sync::atomic::Ordering;

use xxhash_rust::xxh64::Xxh64;

use crate::paths::bad_rel_path;
use crate::scan::Entry;
use crate::stats::Stats;

#[derive(Debug, Default)]
pub struct VerifyReport {
    pub matched: usize,
    pub mismatched: Vec<String>,
    pub missing: Vec<String>,
    pub errors: Vec<String>,
}

impl VerifyReport {
    pub fn failed(&self) -> bool {
        !self.mismatched.is_empty() || !self.missing.is_empty() || !self.errors.is_empty()
    }
}

#[derive(Debug, Clone)]
pub struct Options {
    pub chunk: usize,
    pub keep_bad: bool,
    pub no_cache: bool,
}

/// 计算单个文件的 XXH64（seed = 0）。
pub fn hash_file(path: &Path, chunk: usize, no_cache: bool) -> io::Result<u64> {
    let mut buffer = vec![0u8; chunk];
    hash_file_with(path, &mut buffer, no_cache)
}

pub fn hash_file_with(path: &Path, buffer: &mut [u8], no_cache: bool) -> io::Result<u64> {
    let file = File::open(path)?;
    if no_cache {
        disable_read_cache(&file);
    }
    let mut hasher = Xxh64::new(0);
    let mut reader = io::BufReader::with_capacity(buffer.len(), file);
    loop {
        let n = match reader.read(buffer) {
            Ok(0) => break,
            Ok(n) => n,
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(err) => return Err(err),
        };
        hasher.update(&buffer[..n]);
    }
    Ok(hasher.digest())
}

#[cfg(target_os = "macos")]
fn disable_read_cache(file: &File) {
    use std::os::fd::AsRawFd;
    // F_NOCACHE = 48：让读取绕过统一缓冲，使哈希来自设备。
    unsafe {
        libc::fcntl(file.as_raw_fd(), libc::F_NOCACHE, 1);
    }
}

#[cfg(target_os = "linux")]
fn disable_read_cache(file: &File) {
    use std::os::fd::AsRawFd;
    unsafe {
        libc::posix_fadvise(file.as_raw_fd(), 0, 0, libc::POSIX_FADV_DONTNEED);
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn disable_read_cache(_file: &File) {}

/// 执行阶段 4，只校验阶段 3 已提交的条目。
pub fn run(
    files: &[Entry],
    committed: &[usize],
    source: &Path,
    target: &Path,
    opts: &Options,
    stats: &Stats,
    progress: &mut dyn FnMut(&Stats),
) -> VerifyReport {
    let mut report = VerifyReport::default();
    let mut buffer = vec![0u8; opts.chunk];

    for &index in committed {
        let Some(entry) = files.get(index) else {
            continue;
        };
        if stats.stop_requested() {
            break;
        }
        let rel = entry.rel.display();
        stats.set_current(entry.rel.file_name().to_string_lossy().into_owned());
        let src = source.join(entry.rel.as_path());
        let dst = target.join(entry.rel.as_path());

        if !dst.exists() {
            stats.verify_bad.fetch_add(1, Ordering::Relaxed);
            report.missing.push(rel);
            progress(stats);
            continue;
        }

        let src_hash = hash_with_progress(&src, &mut buffer, opts, stats, progress);
        let dst_hash = hash_with_progress(&dst, &mut buffer, opts, stats, progress);

        match (src_hash, dst_hash) {
            (Ok(a), Ok(b)) if a == b => {
                stats.verify_ok.fetch_add(1, Ordering::Relaxed);
                report.matched += 1;
            }
            (Ok(a), Ok(b)) => {
                stats.verify_bad.fetch_add(1, Ordering::Relaxed);
                let mut message = format!("{rel}：哈希不一致（源 {:016X}，目标 {:016X}）", a, b);
                if opts.keep_bad {
                    let bad = target.join(bad_rel_path(entry.rel.as_path()));
                    match fs::rename(&dst, &bad) {
                        Ok(()) => message.push_str("，已隔离为 .badhash"),
                        Err(err) => {
                            message.push_str(&format!("，隔离失败：{err}"));
                        }
                    }
                } else {
                    match fs::remove_file(&dst) {
                        Ok(()) => message.push_str("，已删除目标文件"),
                        Err(err) => message.push_str(&format!("，删除失败：{err}")),
                    }
                }
                report.mismatched.push(message);
            }
            (Err(err), _) | (_, Err(err)) => {
                stats.verify_bad.fetch_add(1, Ordering::Relaxed);
                report.errors.push(format!("{rel}：读取失败 {err}"));
            }
        }

        if let Some(issue) = crate::scan::check_stability(source, &files[index..index + 1])
            .into_iter()
            .next()
            .flatten()
        {
            report.errors.push(format!("{rel}：{}", issue.describe()));
        }

        progress(stats);
    }

    report
}

fn hash_with_progress(
    path: &Path,
    buffer: &mut [u8],
    opts: &Options,
    stats: &Stats,
    progress: &mut dyn FnMut(&Stats),
) -> io::Result<u64> {
    let file = File::open(path)?;
    if opts.no_cache {
        disable_read_cache(&file);
    }
    let mut hasher = Xxh64::new(0);
    let mut reader = io::BufReader::with_capacity(buffer.len(), file);
    loop {
        if stats.stop_requested() {
            return Err(io::Error::new(io::ErrorKind::Interrupted, "已中断"));
        }
        let n = match reader.read(buffer) {
            Ok(0) => break,
            Ok(n) => n,
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(err) => return Err(err),
        };
        hasher.update(&buffer[..n]);
        stats.hashed_bytes.fetch_add(n as u64, Ordering::Relaxed);
        progress(stats);
    }
    Ok(hasher.digest())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_file_has_known_hash() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("empty");
        fs::write(&path, b"").unwrap();
        assert_eq!(
            hash_file(&path, 4096, false).unwrap(),
            0xEF46_DB37_51D8_E999
        );
    }

    #[test]
    fn hash_is_chunk_size_independent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("data");
        let payload: Vec<u8> = (0..100_000u32).map(|i| (i % 253) as u8).collect();
        fs::write(&path, &payload).unwrap();
        let a = hash_file(&path, 1, false).unwrap();
        let b = hash_file(&path, 4093, false).unwrap();
        let c = hash_file(&path, 1024 * 1024, false).unwrap();
        assert_eq!(a, b);
        assert_eq!(b, c);
    }

    #[test]
    fn detection_of_mismatch() {
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();
        fs::write(src.path().join("f.bin"), b"abc").unwrap();
        fs::write(dst.path().join("f.bin"), b"abd").unwrap();
        let manifest = crate::scan::scan(src.path(), dst.path()).unwrap();
        let stats = Stats::new();
        let report = run(
            &manifest.files,
            &[0],
            src.path(),
            dst.path(),
            &Options {
                chunk: 4096,
                keep_bad: false,
                no_cache: false,
            },
            &stats,
            &mut |_| {},
        );
        assert_eq!(report.matched, 0);
        assert_eq!(report.mismatched.len(), 1);
        assert!(
            !dst.path().join("f.bin").exists(),
            "默认删除不一致的目标文件"
        );
    }

    #[test]
    fn keep_bad_moves_file() {
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();
        fs::write(src.path().join("f.bin"), b"abc").unwrap();
        fs::write(dst.path().join("f.bin"), b"abd").unwrap();
        let manifest = crate::scan::scan(src.path(), dst.path()).unwrap();
        let stats = Stats::new();
        let report = run(
            &manifest.files,
            &[0],
            src.path(),
            dst.path(),
            &Options {
                chunk: 4096,
                keep_bad: true,
                no_cache: false,
            },
            &stats,
            &mut |_| {},
        );
        assert_eq!(report.mismatched.len(), 1);
        assert!(dst.path().join(".f.bin.badhash").exists());
    }
}
