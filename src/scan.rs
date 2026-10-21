//! Traversal for the preflight and for the post-copy stability check: build the
//! manifest, check path rules and detect sources that change during the copy.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::paths::{looks_like_tmp, path_problems, windows_name_problems, RelPath};

/// mtime tolerance: exFAT is 10 ms, FAT is 2 s, so 2 s covers both.
pub const MTIME_TOLERANCE: Duration = Duration::from_secs(2);

#[derive(Clone, Debug)]
pub struct Entry {
    pub rel: RelPath,
    pub size: u64,
    pub mtime: Option<SystemTime>,
}

#[derive(Debug, Default)]
pub struct Manifest {
    pub files: Vec<Entry>,
    pub total_bytes: u64,
    pub skipped_symlinks: usize,
    pub skipped_special: usize,
    /// Directories or files that could not be read while traversing.
    pub errors: Vec<String>,
    /// Path rule conflicts: temp name collision, over-long path, illegal Windows name.
    pub conflicts: Vec<String>,
}

impl Manifest {
    pub fn file_count(&self) -> usize {
        self.files.len()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceIssue {
    Vanished,
    Changed(String),
}

impl SourceIssue {
    pub fn describe(&self) -> String {
        match self {
            SourceIssue::Vanished => "source file disappeared".to_string(),
            SourceIssue::Changed(d) => {
                format!("source file changed during the copy ({d})")
            }
        }
    }
}

/// Walk the source tree, build the manifest and check path rules.
pub fn scan(root: &Path, target_root: &Path) -> io::Result<Manifest> {
    scan_with_progress(root, target_root, &mut |_| {})
}

/// Same as `scan`, but reports the number of scanned files every 256 entries.
pub fn scan_with_progress(
    root: &Path,
    target_root: &Path,
    progress: &mut dyn FnMut(usize),
) -> io::Result<Manifest> {
    let mut manifest = Manifest::default();
    let mut seen = 0usize;
    let mut pending: Vec<PathBuf> = vec![PathBuf::new()];

    while let Some(rel_dir) = pending.pop() {
        let abs = if rel_dir.as_os_str().is_empty() {
            root.to_path_buf()
        } else {
            root.join(&rel_dir)
        };
        let read = match fs::read_dir(&abs) {
            Ok(rd) => rd,
            Err(err) => {
                manifest
                    .errors
                    .push(format!("cannot read directory {}: {err}", abs.display()));
                continue;
            }
        };

        let mut items = Vec::new();
        for item in read {
            match item {
                Ok(entry) => items.push(entry),
                Err(err) => manifest.errors.push(format!(
                    "cannot read directory entry {}: {err}",
                    abs.display()
                )),
            }
        }
        items.sort_by_key(|e| e.file_name());

        for item in items {
            let name = item.file_name();
            let child_rel = rel_dir.join(&name);
            let meta = match fs::symlink_metadata(item.path()) {
                Ok(m) => m,
                Err(err) => {
                    manifest.errors.push(format!(
                        "cannot read metadata {}: {err}",
                        item.path().display()
                    ));
                    continue;
                }
            };
            let file_type = meta.file_type();
            if file_type.is_symlink() {
                manifest.skipped_symlinks += 1;
                continue;
            }
            let rel = match RelPath::new(child_rel.clone()) {
                Ok(r) => r,
                Err(err) => {
                    manifest.conflicts.push(err);
                    continue;
                }
            };
            if let Some(problem) = path_problems(rel.as_path(), target_root) {
                manifest.conflicts.push(problem);
            }
            if let Some(problem) = windows_name_problems(rel.as_path()) {
                manifest.conflicts.push(problem);
            }
            if looks_like_tmp(&name) {
                manifest.conflicts.push(format!(
                    "source file name collides with the temp file rule ({}{}{}): {}",
                    crate::paths::TMP_PREFIX,
                    "xxxxxxxx",
                    crate::paths::TMP_SUFFIX,
                    rel.display()
                ));
            }

            seen += 1;
            if seen.is_multiple_of(256) {
                progress(manifest.files.len());
            }
            if file_type.is_dir() {
                pending.push(child_rel);
            } else if file_type.is_file() {
                manifest.total_bytes += meta.len();
                manifest.files.push(Entry {
                    rel,
                    size: meta.len(),
                    mtime: meta.modified().ok(),
                });
            } else {
                manifest.skipped_special += 1;
            }
        }
    }

    // Keep files sorted by path so repeated runs behave the same.
    manifest.files.sort_by(|a, b| a.rel.cmp(&b.rel));
    progress(manifest.files.len());
    Ok(manifest)
}

/// Probe every source file for readability. Returns the unreadable ones.
pub fn probe_readable(root: &Path, files: &[Entry]) -> Vec<(RelPath, String)> {
    let mut bad = Vec::new();
    for entry in files {
        if let Err(err) = fs::File::open(root.join(entry.rel.as_path())) {
            bad.push((entry.rel.clone(), err.to_string()));
        }
    }
    bad
}

/// Re-check every source file against the manifest recorded before the copy.
pub fn check_stability(root: &Path, files: &[Entry]) -> Vec<Option<SourceIssue>> {
    files
        .iter()
        .map(|entry| {
            let abs = root.join(entry.rel.as_path());
            let meta = match fs::symlink_metadata(&abs) {
                Ok(m) => m,
                Err(err) => {
                    return if err.kind() == io::ErrorKind::NotFound {
                        Some(SourceIssue::Vanished)
                    } else {
                        Some(SourceIssue::Changed(format!("cannot read metadata: {err}")))
                    };
                }
            };
            if meta.len() != entry.size {
                return Some(SourceIssue::Changed(format!(
                    "size changed from {} to {}",
                    entry.size,
                    meta.len()
                )));
            }
            let now_mtime = meta.modified().ok();
            if mtime_changed(entry.mtime, now_mtime) {
                return Some(SourceIssue::Changed(
                    "modification time changed".to_string(),
                ));
            }
            None
        })
        .collect()
}

fn mtime_changed(before: Option<SystemTime>, after: Option<SystemTime>) -> bool {
    match (before, after) {
        (Some(a), Some(b)) => {
            let delta = match b.duration_since(a) {
                Ok(d) => d,
                Err(err) => err.duration(),
            };
            delta > MTIME_TOLERANCE
        }
        (None, None) => false,
        // With only one side known there is nothing to compare; do not report a change.
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn tmpdir() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[test]
    fn scan_collects_files() {
        let src = tmpdir();
        let dst = tmpdir();
        fs::create_dir_all(src.path().join("a/b")).unwrap();
        fs::write(src.path().join("a/b/c.MOV"), b"hello").unwrap();
        fs::write(src.path().join("top.txt"), b"x").unwrap();
        fs::create_dir_all(src.path().join("empty")).unwrap();

        let manifest = scan(src.path(), dst.path()).unwrap();
        assert_eq!(manifest.file_count(), 2);
        assert_eq!(manifest.total_bytes, 6);
        assert!(manifest.errors.is_empty());
        let rels: Vec<String> = manifest.files.iter().map(|f| f.rel.display()).collect();
        assert_eq!(rels, vec!["a/b/c.MOV".to_string(), "top.txt".to_string()]);
    }

    #[test]
    fn scan_skips_symlinks_and_special() {
        let src = tmpdir();
        let dst = tmpdir();
        fs::write(src.path().join("real.txt"), b"data").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("real.txt", src.path().join("link.txt")).unwrap();
        let manifest = scan(src.path(), dst.path()).unwrap();
        assert_eq!(manifest.file_count(), 1);
        #[cfg(unix)]
        assert_eq!(manifest.skipped_symlinks, 1);
    }

    #[test]
    fn scan_flags_tmp_name_conflict() {
        let src = tmpdir();
        let dst = tmpdir();
        let mut f = fs::File::create(src.path().join(".sy-0badc0de.tmp")).unwrap();
        f.write_all(b"x").unwrap();
        let manifest = scan(src.path(), dst.path()).unwrap();
        assert!(!manifest.conflicts.is_empty());
    }

    #[test]
    fn mtime_change_is_detected() {
        let src = tmpdir();
        let dst = tmpdir();
        let path = src.path().join("f.bin");
        fs::write(&path, b"abc").unwrap();
        let manifest = scan(src.path(), dst.path()).unwrap();
        let issues = check_stability(src.path(), &manifest.files);
        assert!(issues[0].is_none());

        fs::write(&path, b"abcdef").unwrap();
        let issues = check_stability(src.path(), &manifest.files);
        assert!(matches!(issues[0], Some(SourceIssue::Changed(_))));

        fs::remove_file(&path).unwrap();
        let issues = check_stability(src.path(), &manifest.files);
        assert_eq!(issues[0], Some(SourceIssue::Vanished));
    }
}
