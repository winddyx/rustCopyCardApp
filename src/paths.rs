//! 路径规则：相对路径类型、tmp 命名、目标布局与路径长度检查。
//!
//! 引擎内部只传递相对源根的路径（`RelPath`），不把任意绝对路径透传到复制与校验逻辑。

use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};

/// 暂存文件名后缀。
pub const TMP_SUFFIX: &str = ".rcptmp";
/// 校验失败时的隔离后缀。
pub const BAD_SUFFIX: &str = ".badhash";

/// 相对源根、已校验的路径。
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RelPath(PathBuf);

impl RelPath {
    /// 构造并校验：必须是相对路径，且不含 `..`、不含根前缀。
    pub fn new(path: impl Into<PathBuf>) -> std::result::Result<Self, String> {
        let path = path.into();
        if path.as_os_str().is_empty() {
            return Err("相对路径为空".to_string());
        }
        if path.is_absolute() {
            return Err(format!("相对路径不能是绝对路径：{}", path.display()));
        }
        for comp in path.components() {
            match comp {
                Component::Normal(_) | Component::CurDir => {}
                Component::ParentDir => {
                    return Err(format!("相对路径不能包含 ..：{}", path.display()));
                }
                Component::RootDir | Component::Prefix(_) => {
                    return Err(format!("相对路径不能包含根前缀：{}", path.display()));
                }
            }
        }
        Ok(RelPath(path))
    }

    pub fn as_path(&self) -> &Path {
        &self.0
    }

    pub fn into_path_buf(self) -> PathBuf {
        self.0
    }

    /// 用于日志与界面展示；文件名可能是非 UTF-8，此处有损转换仅用于显示。
    pub fn display(&self) -> String {
        self.0.to_string_lossy().into_owned()
    }

    pub fn file_name(&self) -> &std::ffi::OsStr {
        self.0.file_name().unwrap_or_default()
    }

    pub fn join_target(&self, root: &Path) -> PathBuf {
        root.join(&self.0)
    }
}

/// 最终路径对应的暂存路径：同目录下 `.<name>.rcptmp`。
pub fn tmp_rel_path(rel: &Path) -> PathBuf {
    let parent = rel.parent().unwrap_or_else(|| Path::new(""));
    let name = rel.file_name().unwrap_or_default();
    let mut tmp_name = OsString::from(".");
    tmp_name.push(name);
    tmp_name.push(TMP_SUFFIX);
    if parent.as_os_str().is_empty() {
        PathBuf::from(tmp_name)
    } else {
        parent.join(tmp_name)
    }
}

/// 校验失败时的隔离路径：同目录下 `.<name>.badhash`。
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

/// 文件名是否形如本工具的暂存文件。
pub fn looks_like_tmp(name: &std::ffi::OsStr) -> bool {
    let s = name.to_string_lossy();
    s.starts_with('.') && s.ends_with(TMP_SUFFIX) && s.len() > TMP_SUFFIX.len() + 1
}

/// 目标根是否等于源根或位于其内 / 反之。两者都已规范化。
pub fn nesting_problem(source: &Path, target: &Path) -> Option<String> {
    if source == target {
        return Some("源与目标是同一个目录".to_string());
    }
    if target.starts_with(source) {
        return Some(format!(
            "目标目录位于源目录内（会自复制递归）：源 {}，目标 {}",
            source.display(),
            target.display()
        ));
    }
    if source.starts_with(target) {
        return Some(format!(
            "源目录位于目标目录内（会与目标内容混在一起）：源 {}，目标 {}",
            source.display(),
            target.display()
        ));
    }
    None
}

/// 目标根 + 相对路径的路径长度与单段长度是否超限。
pub fn path_problems(rel: &Path, target_root: &Path) -> Option<String> {
    let total = target_root.as_os_str().len() + 1 + rel.as_os_str().len();
    if total > path_limit() {
        return Some(format!(
            "路径总长度 {} 超过平台上限 {}：{}",
            total,
            path_limit(),
            target_root.join(rel).display()
        ));
    }
    for comp in rel.components() {
        let len = comp.as_os_str().len();
        if len > 255 {
            return Some(format!(
                "单段文件名长度 {} 超过 255：{}",
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

/// 把 Windows 上不合法或保留的字符问题暴露给预检（其它平台恒为空）。
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
                return Some(format!("文件名包含 Windows 不允许的字符：{s}"));
            }
            let stem = s.split('.').next().unwrap_or("").to_ascii_uppercase();
            if RESERVED.contains(&stem.as_str()) {
                return Some(format!("文件名是 Windows 保留名：{s}"));
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
    fn tmp_path_is_sibling_hidden_name() {
        assert_eq!(
            tmp_rel_path(Path::new("a/b/c.MOV")),
            PathBuf::from("a/b/.c.MOV.rcptmp")
        );
        assert_eq!(
            tmp_rel_path(Path::new("c.MOV")),
            PathBuf::from(".c.MOV.rcptmp")
        );
        assert_eq!(
            tmp_rel_path(Path::new("a/.hidden")),
            PathBuf::from("a/..hidden.rcptmp")
        );
    }

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
        assert!(looks_like_tmp(std::ffi::OsStr::new(".a.MOV.rcptmp")));
        assert!(!looks_like_tmp(std::ffi::OsStr::new("a.MOV")));
        assert!(!looks_like_tmp(std::ffi::OsStr::new(".rcptmp")));
    }
}
