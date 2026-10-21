//! Verification pass: hash source and target for every file and compare the XXH64 digests.
//!
//! The loop honours the same `-j` as the copy: with `jobs > 1` several files are
//! verified at once, which overlaps source reads with target reads. Results are
//! collected per file and reported in manifest order, so the output does not depend
//! on which worker finished first.

use std::fs::{self, File};
use std::io::{self, Read};
use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::Mutex;

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
    /// Number of files verified at once, 1 means serial.
    pub jobs: usize,
}

/// Compute the XXH64 of a file (seed = 0) without reporting progress.
pub fn hash_file(path: &Path, chunk: usize, no_cache: bool) -> io::Result<u64> {
    let mut buffer = vec![0u8; chunk];
    hash_stream(path, &mut buffer, no_cache, &mut |_| {})
}

/// Read a file in chunks and hash it, calling `on_chunk` once per chunk.
pub fn hash_stream(
    path: &Path,
    buffer: &mut [u8],
    no_cache: bool,
    on_chunk: &mut dyn FnMut(u64),
) -> io::Result<u64> {
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
        on_chunk(n as u64);
    }
    Ok(hasher.digest())
}

#[cfg(target_os = "macos")]
fn disable_read_cache(file: &File) {
    use std::os::fd::AsRawFd;
    // F_NOCACHE = 48: bypass the unified buffer cache so the hash comes from the device.
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

/// What happened to one file, kept until the report is assembled in order.
enum Outcome {
    Matched,
    Missing(String),
    Mismatch(String),
    Failed(String),
}

/// Verify the entries that were committed by the copy phase.
pub fn run(
    files: &[Entry],
    committed: &[usize],
    source: &Path,
    target: &Path,
    opts: &Options,
    stats: &Stats,
) -> VerifyReport {
    let jobs = opts.jobs.max(1).min(committed.len().max(1));
    let outcomes: Mutex<Vec<Option<Outcome>>> =
        Mutex::new((0..committed.len()).map(|_| None).collect());
    let cursor = std::sync::atomic::AtomicUsize::new(0);

    std::thread::scope(|scope| {
        for _ in 0..jobs {
            scope.spawn(|| {
                // Each worker keeps its own read buffer.
                let mut buffer = vec![0u8; opts.chunk];
                loop {
                    let pos = cursor.fetch_add(1, Ordering::Relaxed);
                    let Some(&index) = committed.get(pos) else {
                        break;
                    };
                    if stats.stop_requested() {
                        break;
                    }
                    let Some(entry) = files.get(index) else {
                        continue;
                    };
                    let outcome = verify_one(entry, source, target, opts, stats, &mut buffer);
                    if let Ok(mut slots) = outcomes.lock() {
                        slots[pos] = Some(outcome);
                    }
                }
            });
        }
    });

    let mut report = VerifyReport::default();
    for outcome in outcomes
        .into_inner()
        .unwrap_or_default()
        .into_iter()
        .flatten()
    {
        match outcome {
            Outcome::Matched => report.matched += 1,
            Outcome::Missing(message) => report.missing.push(message),
            Outcome::Mismatch(message) => report.mismatched.push(message),
            Outcome::Failed(message) => report.errors.push(message),
        }
    }
    report
}

fn verify_one(
    entry: &Entry,
    source: &Path,
    target: &Path,
    opts: &Options,
    stats: &Stats,
    buffer: &mut [u8],
) -> Outcome {
    let rel = entry.rel.display();
    stats.set_current(entry.rel.file_name().to_string_lossy().into_owned());
    let src = source.join(entry.rel.as_path());
    let dst = target.join(entry.rel.as_path());

    if !dst.exists() {
        return Outcome::Missing(rel);
    }

    let src_hash = hash_counted(&src, buffer, opts, stats);
    let dst_hash = hash_counted(&dst, buffer, opts, stats);

    let outcome = match (src_hash, dst_hash) {
        (Ok(a), Ok(b)) if a == b => Outcome::Matched,
        (Ok(a), Ok(b)) => {
            let mut message = format!("{rel}: hash mismatch (source {a:016X}, target {b:016X})");
            if opts.keep_bad {
                let bad = target.join(bad_rel_path(entry.rel.as_path()));
                match fs::rename(&dst, &bad) {
                    Ok(()) => message.push_str(", quarantined as .badhash"),
                    Err(err) => {
                        message.push_str(&format!(", quarantine failed: {err}"));
                    }
                }
            } else {
                match fs::remove_file(&dst) {
                    Ok(()) => message.push_str(", target file deleted"),
                    Err(err) => message.push_str(&format!(", delete failed: {err}")),
                }
            }
            Outcome::Mismatch(message)
        }
        (Err(err), _) | (_, Err(err)) => Outcome::Failed(format!("{rel}: read failed: {err}")),
    };

    if let Some(issue) = crate::scan::check_stability(source, std::slice::from_ref(entry))
        .into_iter()
        .next()
        .flatten()
    {
        return Outcome::Failed(format!("{rel}: {}", issue.describe()));
    }

    outcome
}

/// Hash a file and add the bytes read to the stats the progress line reads.
fn hash_counted(path: &Path, buffer: &mut [u8], opts: &Options, stats: &Stats) -> io::Result<u64> {
    let hashed = &stats.hashed_bytes;
    hash_stream(path, buffer, opts.no_cache, &mut |n| {
        hashed.fetch_add(n, Ordering::Relaxed);
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_options() -> Options {
        Options {
            chunk: 4096,
            keep_bad: false,
            no_cache: false,
            jobs: 1,
        }
    }

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
            &base_options(),
            &stats,
        );
        assert_eq!(report.matched, 0);
        assert_eq!(report.mismatched.len(), 1);
        assert!(
            !dst.path().join("f.bin").exists(),
            "a mismatching target file is deleted by default"
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
                keep_bad: true,
                ..base_options()
            },
            &stats,
        );
        assert_eq!(report.mismatched.len(), 1);
        assert!(dst.path().join(".f.bin.badhash").exists());
    }

    /// Build a source/target pair with `good` matching files and `bad` mismatching ones.
    fn fixture(good: usize, bad: usize) -> (tempfile::TempDir, tempfile::TempDir, Vec<usize>) {
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();
        for i in 0..good {
            fs::write(src.path().join(format!("g{i:02}.bin")), b"same").unwrap();
            fs::write(dst.path().join(format!("g{i:02}.bin")), b"same").unwrap();
        }
        for i in 0..bad {
            fs::write(src.path().join(format!("b{i:02}.bin")), b"left").unwrap();
            fs::write(dst.path().join(format!("b{i:02}.bin")), b"right").unwrap();
        }
        let manifest = crate::scan::scan(src.path(), dst.path()).unwrap();
        let indices = (0..manifest.files.len()).collect();
        (src, dst, indices)
    }

    #[test]
    fn parallel_verification_matches_the_serial_result() {
        let (src, dst, indices) = fixture(12, 3);

        let manifest = crate::scan::scan(src.path(), dst.path()).unwrap();
        let serial = run(
            &manifest.files,
            &indices,
            src.path(),
            dst.path(),
            &base_options(),
            &Stats::new(),
        );

        // Rebuild the target so the mismatching files exist again for the parallel run.
        let (src2, dst2, indices2) = fixture(12, 3);
        let manifest2 = crate::scan::scan(src2.path(), dst2.path()).unwrap();
        let parallel = run(
            &manifest2.files,
            &indices2,
            src2.path(),
            dst2.path(),
            &Options {
                jobs: 4,
                ..base_options()
            },
            &Stats::new(),
        );

        assert_eq!(serial.matched, parallel.matched);
        assert_eq!(serial.mismatched.len(), parallel.mismatched.len());
        assert_eq!(serial.missing.len(), parallel.missing.len());
        assert_eq!(serial.errors.len(), parallel.errors.len());
        assert_eq!(parallel.matched, 12);
        assert_eq!(parallel.mismatched.len(), 3);
    }

    #[test]
    fn mismatches_are_reported_in_manifest_order() {
        let (src, dst, indices) = fixture(4, 6);
        let manifest = crate::scan::scan(src.path(), dst.path()).unwrap();
        let report = run(
            &manifest.files,
            &indices,
            src.path(),
            dst.path(),
            &Options {
                jobs: 8,
                ..base_options()
            },
            &Stats::new(),
        );
        let names: Vec<&str> = report
            .mismatched
            .iter()
            .map(|m| m.split(':').next().unwrap())
            .collect();
        let mut sorted = names.clone();
        sorted.sort();
        assert_eq!(
            names, sorted,
            "output order must not depend on worker order"
        );
    }

    #[test]
    fn jobs_greater_than_file_count_is_fine() {
        let (src, dst, indices) = fixture(3, 0);
        let manifest = crate::scan::scan(src.path(), dst.path()).unwrap();
        let report = run(
            &manifest.files,
            &indices,
            src.path(),
            dst.path(),
            &Options {
                jobs: 16,
                ..base_options()
            },
            &Stats::new(),
        );
        assert_eq!(report.matched, 3);
    }
}
