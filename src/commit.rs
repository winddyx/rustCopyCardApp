//! 阶段 3：复查源、校验暂存文件身份与长度、原子重命名、目录 fsync。

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::copy::{FileInfo, Outcome};
use crate::paths::{tmp_rel_path, RelPath};
use crate::scan::{self, Entry, SourceIssue};

#[derive(Debug, Clone)]
pub enum CommitFailure {
    SourceIssue(SourceIssue),
    SizeMismatch { expected: u64, actual: u64 },
    StageMissing,
    StageReplaced,
    RenameFailed(String),
    ParentDirFailed(String),
}

impl CommitFailure {
    pub fn describe(&self) -> String {
        match self {
            CommitFailure::SourceIssue(issue) => issue.describe(),
            CommitFailure::SizeMismatch { expected, actual } => {
                format!("暂存文件长度不符：期望 {expected}，实际 {actual}")
            }
            CommitFailure::StageMissing => "暂存文件缺失".to_string(),
            CommitFailure::StageReplaced => "暂存文件在提交前被替换（dev/ino 不一致）".to_string(),
            CommitFailure::RenameFailed(err) => format!("重命名失败：{err}"),
            CommitFailure::ParentDirFailed(err) => format!("目标目录创建失败：{err}"),
        }
    }
}

#[derive(Debug, Default)]
pub struct CommitReport {
    pub committed: Vec<usize>,
    pub failures: Vec<(usize, CommitFailure)>,
}

impl CommitReport {
    pub fn committed_count(&self) -> usize {
        self.committed.len()
    }

    pub fn failure_count(&self) -> usize {
        self.failures.len()
    }
}

/// 执行阶段 3。只处理阶段 2 成功的条目。
pub fn run(
    files: &[Entry],
    outcomes: &[Option<Outcome>],
    source: &Path,
    target: &Path,
    fsync: bool,
) -> CommitReport {
    let stability = scan::check_stability(source, files);
    let mut report = CommitReport::default();
    let mut dirs_to_sync: Vec<PathBuf> = Vec::new();

    for (index, entry) in files.iter().enumerate() {
        let Some(Outcome::Ok(info)) = outcomes.get(index).and_then(|o| o.as_ref()) else {
            continue;
        };
        if let Some(issue) = stability.get(index).and_then(|i| i.clone()) {
            report
                .failures
                .push((index, CommitFailure::SourceIssue(issue)));
            remove_stage(target, &entry.rel);
            continue;
        }

        match commit_one(entry, *info, target) {
            Ok(parent) => {
                report.committed.push(index);
                if let Some(parent) = parent {
                    if !dirs_to_sync.contains(&parent) {
                        dirs_to_sync.push(parent);
                    }
                }
            }
            Err(failure) => {
                remove_stage(target, &entry.rel);
                report.failures.push((index, failure));
            }
        }
    }

    if fsync {
        for dir in dirs_to_sync {
            if let Ok(handle) = fs::File::open(&dir) {
                let _ = handle.sync_all();
            }
        }
    }

    report
}

fn commit_one(
    entry: &Entry,
    info: FileInfo,
    target: &Path,
) -> Result<Option<PathBuf>, CommitFailure> {
    let tmp_rel = tmp_rel_path(entry.rel.as_path());
    let tmp_abs = target.join(&tmp_rel);
    let final_abs = target.join(entry.rel.as_path());

    let meta = match fs::symlink_metadata(&tmp_abs) {
        Ok(m) => m,
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            return Err(CommitFailure::StageMissing)
        }
        Err(err) => return Err(CommitFailure::RenameFailed(err.to_string())),
    };

    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if (info.dev != 0 || info.ino != 0) && (meta.dev() != info.dev || meta.ino() != info.ino) {
            return Err(CommitFailure::StageReplaced);
        }
    }
    if meta.len() != entry.size {
        return Err(CommitFailure::SizeMismatch {
            expected: entry.size,
            actual: meta.len(),
        });
    }
    if meta.len() != info.size {
        return Err(CommitFailure::SizeMismatch {
            expected: info.size,
            actual: meta.len(),
        });
    }

    if let Some(parent) = final_abs.parent() {
        if !parent.as_os_str().is_empty() && !parent.exists() {
            fs::create_dir_all(parent)
                .map_err(|err| CommitFailure::ParentDirFailed(err.to_string()))?;
        }
    }

    #[cfg(windows)]
    {
        if final_abs.exists() {
            fs::remove_file(&final_abs)
                .map_err(|err| CommitFailure::RenameFailed(err.to_string()))?;
        }
    }

    fs::rename(&tmp_abs, &final_abs).map_err(|err| CommitFailure::RenameFailed(err.to_string()))?;

    Ok(final_abs.parent().map(|p| p.to_path_buf()))
}

fn remove_stage(target: &Path, rel: &RelPath) {
    let tmp_abs = target.join(tmp_rel_path(rel.as_path()));
    let _ = fs::remove_file(tmp_abs);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::copy::Outcome;
    use crate::paths::RelPath;
    use std::time::SystemTime;

    fn setup() -> (tempfile::TempDir, tempfile::TempDir, Vec<Entry>) {
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();
        fs::write(src.path().join("a.bin"), b"payload").unwrap();
        let manifest = scan::scan(src.path(), dst.path()).unwrap();
        (src, dst, manifest.files)
    }

    fn write_stage(target: &Path, rel: &RelPath, data: &[u8]) -> FileInfo {
        let path = target.join(tmp_rel_path(rel.as_path()));
        fs::write(&path, data).unwrap();
        let meta = fs::metadata(&path).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            FileInfo {
                dev: meta.dev(),
                ino: meta.ino(),
                size: meta.len(),
            }
        }
        #[cfg(not(unix))]
        {
            FileInfo {
                dev: 0,
                ino: 0,
                size: meta.len(),
            }
        }
    }

    #[test]
    fn commit_renames_stage_to_final() {
        let (src, dst, files) = setup();
        let info = write_stage(dst.path(), &files[0].rel, b"payload");
        let outcomes = vec![Some(Outcome::Ok(info))];
        let report = run(&files, &outcomes, src.path(), dst.path(), false);
        assert_eq!(report.committed_count(), 1);
        assert_eq!(
            fs::read(dst.path().join("a.bin")).unwrap(),
            b"payload".to_vec()
        );
        assert!(!dst.path().join(".a.bin.rcptmp").exists());
    }

    #[test]
    fn commit_rejects_changed_source() {
        let (src, dst, files) = setup();
        let info = write_stage(dst.path(), &files[0].rel, b"payload");
        fs::write(src.path().join("a.bin"), b"changed-much").unwrap();
        let outcomes = vec![Some(Outcome::Ok(info))];
        let report = run(&files, &outcomes, src.path(), dst.path(), false);
        assert_eq!(report.committed_count(), 0);
        assert_eq!(report.failure_count(), 1);
        assert!(!dst.path().join("a.bin").exists());
        assert!(!dst.path().join(".a.bin.rcptmp").exists());
    }

    #[test]
    fn commit_rejects_size_mismatch() {
        let (src, dst, files) = setup();
        let info = write_stage(dst.path(), &files[0].rel, b"short");
        let outcomes = vec![Some(Outcome::Ok(info))];
        let report = run(&files, &outcomes, src.path(), dst.path(), false);
        assert_eq!(report.committed_count(), 0);
        assert!(matches!(
            report.failures[0].1,
            CommitFailure::SizeMismatch { .. }
        ));
    }

    #[test]
    fn commit_reports_missing_stage() {
        let (src, dst, files) = setup();
        let info = FileInfo {
            dev: 1,
            ino: 1,
            size: 7,
        };
        let _ = SystemTime::now();
        let outcomes = vec![Some(Outcome::Ok(info))];
        let report = run(&files, &outcomes, src.path(), dst.path(), false);
        assert!(matches!(
            report.failures[0].1,
            CommitFailure::StageMissing | CommitFailure::StageReplaced
        ));
    }
}
