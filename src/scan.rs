//! 阶段 1 与阶段 3 的遍历：构建清单、检查目标、判定源在复制期间是否变化。

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::paths::{looks_like_tmp, path_problems, windows_name_problems, RelPath};

/// mtime 容差：exFAT 为 10 ms、FAT 为 2 s，此处统一取 2 s。
pub const MTIME_TOLERANCE: Duration = Duration::from_secs(2);

#[derive(Clone, Debug)]
pub struct Entry {
    pub rel: RelPath,
    pub size: u64,
    pub mtime: Option<SystemTime>,
    pub mode: u32,
}

#[derive(Clone, Debug)]
pub struct DirRec {
    pub rel: RelPath,
    pub mode: u32,
}

#[derive(Debug, Default)]
pub struct Manifest {
    pub files: Vec<Entry>,
    pub dirs: Vec<DirRec>,
    pub total_bytes: u64,
    pub skipped_symlinks: usize,
    pub skipped_special: usize,
    /// 遍历过程中无法读取的目录或文件。
    pub errors: Vec<String>,
    /// 路径规则冲突：tmp 命名冲突、路径超长、Windows 非法名。
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
            SourceIssue::Vanished => "源文件已消失".to_string(),
            SourceIssue::Changed(d) => format!("源文件在复制期间发生变化（{d}）"),
        }
    }
}

/// 阶段 1：递归遍历源目录，构建清单并做路径规则检查。
pub fn scan(root: &Path, target_root: &Path) -> io::Result<Manifest> {
    scan_with_progress(root, target_root, &mut |_| {})
}

/// 同上，但每 256 个条目回调一次已扫描文件数，用于界面显示进度。
pub fn scan_with_progress(
    root: &Path,
    target_root: &Path,
    progress: &mut dyn FnMut(usize),
) -> io::Result<Manifest> {
    let mut manifest = Manifest::default();
    let mut seen = 0usize;
    let mut pending: Vec<PathBuf> = vec![PathBuf::new()];
    let mut dirs: Vec<DirRec> = Vec::new();

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
                    .push(format!("无法读取目录 {}：{err}", abs.display()));
                continue;
            }
        };

        let mut items = Vec::new();
        for item in read {
            match item {
                Ok(entry) => items.push(entry),
                Err(err) => manifest
                    .errors
                    .push(format!("目录项读取失败 {}：{err}", abs.display())),
            }
        }
        items.sort_by_key(|e| e.file_name());

        for item in items {
            let name = item.file_name();
            let child_rel = rel_dir.join(&name);
            let meta = match fs::symlink_metadata(item.path()) {
                Ok(m) => m,
                Err(err) => {
                    manifest
                        .errors
                        .push(format!("无法获取元数据 {}：{err}", item.path().display()));
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
                    "源文件名与暂存文件规则冲突（以 . 开头并以 {} 结尾）：{}",
                    crate::paths::TMP_SUFFIX,
                    rel.display()
                ));
            }

            seen += 1;
            if seen.is_multiple_of(256) {
                progress(manifest.files.len());
            }
            if file_type.is_dir() {
                dirs.push(DirRec {
                    mode: mode_of(&meta),
                    rel,
                });
                pending.push(child_rel);
            } else if file_type.is_file() {
                manifest.total_bytes += meta.len();
                manifest.files.push(Entry {
                    rel,
                    size: meta.len(),
                    mtime: meta.modified().ok(),
                    mode: mode_of(&meta),
                });
            } else {
                manifest.skipped_special += 1;
            }
        }
    }

    // 目录按深度升序（父目录先建），文件与目录按路径排序，保证多次运行顺序一致。
    dirs.sort_by(|a, b| {
        let da = a.rel.as_path().components().count();
        let db = b.rel.as_path().components().count();
        da.cmp(&db).then_with(|| a.rel.cmp(&b.rel))
    });
    manifest.files.sort_by(|a, b| a.rel.cmp(&b.rel));
    manifest.dirs = dirs;
    progress(manifest.files.len());
    Ok(manifest)
}

/// 逐个源文件试探可读性。返回不可读清单。
pub fn probe_readable(root: &Path, files: &[Entry]) -> Vec<(RelPath, String)> {
    let mut bad = Vec::new();
    for entry in files {
        if let Err(err) = fs::File::open(root.join(entry.rel.as_path())) {
            bad.push((entry.rel.clone(), err.to_string()));
        }
    }
    bad
}

/// 阶段 3：复查每个源文件是否与阶段 1 清单一致。
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
                        Some(SourceIssue::Changed(format!("元数据读取失败：{err}")))
                    };
                }
            };
            if meta.len() != entry.size {
                return Some(SourceIssue::Changed(format!(
                    "大小 {} 变为 {}",
                    entry.size,
                    meta.len()
                )));
            }
            let now_mtime = meta.modified().ok();
            if mtime_changed(entry.mtime, now_mtime) {
                return Some(SourceIssue::Changed("修改时间变化".to_string()));
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
        // 一侧拿不到时间时不做判定，避免误报。
        _ => false,
    }
}

/// 按清单创建目录（父目录在前）。目录权限暂时放宽，保证能写入，末尾再恢复精确权限。
pub fn create_dirs(target: &Path, dirs: &[DirRec]) -> io::Result<()> {
    for dir in dirs {
        let abs = target.join(dir.rel.as_path());
        fs::create_dir_all(&abs)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            // 复制期间需要写权限，源目录若是只读，稍后由 restore_dir_modes 收尾。
            let mode = (dir.mode & 0o7777) | 0o700;
            let _ = fs::set_permissions(&abs, fs::Permissions::from_mode(mode));
        }
    }
    Ok(())
}

/// 全部校验完成后恢复目录的原始权限。失败只警告，不改变任务结论。
pub fn restore_dir_modes(target: &Path, dirs: &[DirRec]) -> Vec<String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut warnings = Vec::new();
        // 倒序处理，避免父目录先变只读后导致子目录无法修改。
        for dir in dirs.iter().rev() {
            let abs = target.join(dir.rel.as_path());
            if let Err(err) =
                fs::set_permissions(&abs, fs::Permissions::from_mode(dir.mode & 0o7777))
            {
                warnings.push(format!("恢复目录权限失败 {}：{err}", abs.display()));
            }
        }
        warnings
    }
    #[cfg(not(unix))]
    {
        let _ = (target, dirs);
        Vec::new()
    }
}

pub fn mode_of(meta: &fs::Metadata) -> u32 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        meta.mode() & 0o7777
    }
    #[cfg(not(unix))]
    {
        let _ = meta;
        0o644
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
    fn scan_collects_files_and_dirs() {
        let src = tmpdir();
        let dst = tmpdir();
        fs::create_dir_all(src.path().join("a/b")).unwrap();
        fs::write(src.path().join("a/b/c.MOV"), b"hello").unwrap();
        fs::write(src.path().join("top.txt"), b"x").unwrap();
        fs::create_dir_all(src.path().join("empty")).unwrap();

        let manifest = scan(src.path(), dst.path()).unwrap();
        assert_eq!(manifest.file_count(), 2);
        assert_eq!(manifest.total_bytes, 6);
        assert!(manifest.dirs.iter().any(|d| d.rel.display() == "a/b"));
        assert!(manifest.dirs.iter().any(|d| d.rel.display() == "empty"));
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
        let mut f = fs::File::create(src.path().join(".x.MOV.rcptmp")).unwrap();
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
