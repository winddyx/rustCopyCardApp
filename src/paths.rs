//! Path rules: the relative path type, temp naming, target layout and length checks.
//!
//! The engine only passes paths relative to the source root (`RelPath`); arbitrary
//! absolute paths never reach the copy or verify logic.

use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};

/// Temp file prefix and suffix used by sy: `.sy-xxxxxxxx.tmp`.
pub const TMP_PREFIX: &str = ".sy-";
pub const TMP_SUFFIX: &str = ".tmp";
/// Suffix used to quarantine a file that failed verification.
pub const BAD_SUFFIX: &str = ".badhash";

/// A path relative to the source root, validated on construction.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RelPath(PathBuf);

impl RelPath {
    /// Build and validate: must be relative, without `..` and without a root prefix.
    pub fn new(path: impl Into<PathBuf>) -> std::result::Result<Self, String> {
        let path = path.into();
        if path.as_os_str().is_empty() {
            return Err("relative path is empty".to_string());
        }
        if path.is_absolute() {
            return Err(format!("relative path is absolute: {}", path.display()));
        }
        for comp in path.components() {
            match comp {
                Component::Normal(_) | Component::CurDir => {}
                Component::ParentDir => {
                    return Err(format!("relative path contains ..: {}", path.display()));
                }
                Component::RootDir | Component::Prefix(_) => {
                    return Err(format!(
                        "relative path has a root prefix: {}",
                        path.display()
                    ));
                }
            }
        }
        Ok(RelPath(path))
    }

    pub fn as_path(&self) -> &Path {
        &self.0
    }

    /// Used for logs and display; the file name may not be UTF-8, so this is lossy.
    pub fn display(&self) -> String {
        self.0.to_string_lossy().into_owned()
    }

    pub fn file_name(&self) -> &std::ffi::OsStr {
        self.0.file_name().unwrap_or_default()
    }
}

/// Quarantine path for a file that failed verification: `.<name>.badhash` next to it.
pub fn bad_rel_path(rel: &Path) -> PathBuf {
    let parent = rel.parent().unwrap_or_else(|| Path::new(""));
    let name = rel.file_name().unwrap_or_default();
    let mut bad_name = OsString::from(".");
    bad_name.push(name);
    bad_name.push(BAD_SUFFIX);
    if parent.as_os_str().is_empty() {
        PathBuf::from(bad_name)
    } else {
        parent.join(bad_name)
    }
}

/// Whether a name looks like one of sy's temp files (older rcca `.rcptmp` too).
pub fn looks_like_tmp(name: &std::ffi::OsStr) -> bool {
    let s = name.to_string_lossy();
    (s.starts_with(TMP_PREFIX) && s.ends_with(TMP_SUFFIX))
        || (s.starts_with('.') && s.ends_with(".rcptmp") && s.len() > ".rcptmp".len() + 1)
}

/// Whether the target root equals the source root or sits inside it, or the other
/// way round. Both paths must already be canonical.
pub fn nesting_problem(source: &Path, target: &Path) -> Option<String> {
    if source == target {
        return Some("source and target are the same directory".to_string());
    }
    if target.starts_with(source) {
        return Some(format!(
            "target is inside the source (the copy would recurse into itself): source {}, target {}",
            source.display(),
            target.display()
        ));
    }
    if source.starts_with(target) {
        return Some(format!(
            "source is inside the target (their contents would mix): source {}, target {}",
            source.display(),
            target.display()
        ));
    }
    None
}

/// Whether target root plus relative path exceeds the platform path or component limit.
pub fn path_problems(rel: &Path, target_root: &Path) -> Option<String> {
    let total = target_root.as_os_str().len() + 1 + rel.as_os_str().len();
    if total > path_limit() {
        return Some(format!(
            "path length {} exceeds the platform limit {}: {}",
            total,
            path_limit(),
            target_root.join(rel).display()
        ));
    }
    for comp in rel.components() {
        let len = comp.as_os_str().len();
        if len > 255 {
            return Some(format!(
                "file name component length {} exceeds 255: {}",
                len,
                rel.display()
            ));
        }
    }
    None
}

fn path_limit() -> usize {
    #[cfg(target_os = "windows")]
    {
        259
    }
    #[cfg(target_os = "macos")]
    {
        1023
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        4095
    }
}

/// Report illegal or reserved Windows names to the preflight (always empty elsewhere).
pub fn windows_name_problems(rel: &Path) -> Option<String> {
    #[cfg(target_os = "windows")]
    {
        const RESERVED: [&str; 22] = [
            "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7",
            "COM8", "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
        ];
        for comp in rel.components() {
            let s = comp.as_os_str().to_string_lossy();
            if s.contains(['<', '>', ':', '"', '|', '?', '*']) || s.ends_with(['.', ' ']) {
                return Some(format!(
                    "file name contains characters Windows rejects: {s}"
                ));
            }
            let stem = s.split('.').next().unwrap_or("").to_ascii_uppercase();
            if RESERVED.contains(&stem.as_str()) {
                return Some(format!("file name is a reserved Windows name: {s}"));
            }
        }
        None
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = rel;
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bad_path_is_sibling_hidden_name() {
        assert_eq!(
            bad_rel_path(Path::new("a/c.MOV")),
            PathBuf::from("a/.c.MOV.badhash")
        );
    }

    #[test]
    fn rel_path_rejects_escape() {
        assert!(RelPath::new("../x").is_err());
        assert!(RelPath::new("/abs").is_err());
        assert!(RelPath::new("").is_err());
        assert!(RelPath::new("a/b").is_ok());
    }

    #[test]
    fn nesting_detection() {
        let s = Path::new("/data/card");
        assert!(nesting_problem(s, Path::new("/data/card/sub")).is_some());
        assert!(nesting_problem(s, s).is_some());
        assert!(nesting_problem(Path::new("/data/card/sub"), s).is_some());
        assert!(nesting_problem(s, Path::new("/out/copy")).is_none());
    }

    #[test]
    fn tmp_name_detection() {
        assert!(looks_like_tmp(std::ffi::OsStr::new(".sy-1a2b3c4d.tmp")));
        assert!(looks_like_tmp(std::ffi::OsStr::new(".a.MOV.rcptmp")));
        assert!(!looks_like_tmp(std::ffi::OsStr::new("a.MOV")));
        assert!(!looks_like_tmp(std::ffi::OsStr::new(".rcptmp")));
    }
}
