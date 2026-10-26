//! 前置检查与复制后稳定性检查所用的目录遍历：生成清单、检查路径规则，
//! 并识别复制过程中被改动的源文件。

// 文件系统操作与错误类型；`io::Result` 会在签名里出现。
use std::fs;
use std::io;
// 路径相关的借用与拥有类型。
use std::path::{Path, PathBuf};
// 时间类型用于记录并复核修改时间。
use std::time::{Duration, SystemTime};

// 复用 paths 模块的规则检查，避免同一套判断写两遍。
use crate::paths::{looks_like_tmp, path_problems, windows_name_problems, RelPath};

/// mtime 容差：exFAT 的时间戳精度是 10 ms，FAT 是 2 s，取 2 s 可以覆盖两者。
///
/// `Duration` 是标准库的时长类型，`const` 上下文中可以直接调用它的构造方法。
pub const MTIME_TOLERANCE: Duration = Duration::from_secs(2);

/// 清单里的一条记录：相对路径、大小和可选的修改时间。
///
/// `mtime` 用 `Option<SystemTime>`：某些文件系统或平台取不到修改时间，
/// 用 `None` 表示"未知"比用哨兵值（比如 0）更安全，后续比较也不至于误判。
#[derive(Clone, Debug)]
pub struct Entry {
    pub rel: RelPath,
    // `u64` 与 `Metadata::len()` 的返回类型一致，避免来回转换。
    pub size: u64,
    pub mtime: Option<SystemTime>,
}

/// 源目录的完整清单。
///
/// `#[derive(Default)]` 自动生成"所有字段取默认值"的构造：`Vec` 是空向量，
/// `u64`/`usize` 是 0。因此 `Manifest::default()` 就等于一份空清单。
#[derive(Debug, Default)]
pub struct Manifest {
    // 所有普通文件的条目，最后会按相对路径排序。
    pub files: Vec<Entry>,
    // 文件大小之和，用于进度分母与剩余空间检查。
    pub total_bytes: u64,
    // 被跳过的符号链接数：sy 会复制它们，但 rcca 无法校验。
    pub skipped_symlinks: usize,
    // 特殊文件数（管道、套接字、设备节点）。sy 只认常规文件与符号链接，
    // 这类条目交给它会阻塞或失败，因此 preflight 直接拒绝，这里只用于给出原因。
    pub skipped_special: usize,
    /// 第一个特殊文件的相对路径，供错误信息指路。
    pub first_special: Option<RelPath>,
    /// 遍历过程中读不到的目录或文件。
    pub errors: Vec<String>,
    /// 路径规则冲突：临时文件名撞车、路径过长、非法 Windows 文件名。
    pub conflicts: Vec<String>,
}

/// 复制完成后再核对源文件时可能发现的问题。
///
/// `Changed(String)` 携带原因，`Vanished` 不带：枚举变体可以各自带不同的载荷，
/// 这是枚举比"错误码加字符串"更好用的地方。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceIssue {
    // 源文件在复制期间消失。
    Vanished,
    // 源文件内容或时间戳发生变化，圆括号里是原因描述。
    Changed(String),
}

// 问题类型的方法：只负责转成给人看的文本。
impl SourceIssue {
    /// 把问题转成给人看的一句话。返回 `String` 而不是 `&str`，
    /// 因为部分分支需要拼接原因，生命周期上没法借用。
    pub fn describe(&self) -> String {
        // `&self` 匹配时不必写 `&`：默认绑定模式会自动按引用匹配变体。
        match self {
            SourceIssue::Vanished => "source file disappeared".to_string(),
            // `d` 是 `&String`，可以直接用于格式化。
            SourceIssue::Changed(d) => {
                format!("source file changed during the copy ({d})")
            }
        }
    }
}

/// 遍历源目录树，生成清单并检查路径规则。
///
/// 这是个便捷包装：不需要进度回调时传一个空闭包 `&mut |_| {}`，
/// 从而让调用点只关心结果。空闭包的类型由参数位置推断出来。
pub fn scan(root: &Path, target_root: &Path) -> io::Result<Manifest> {
    // `&mut |_| {}` 现场构造一个忽略参数的闭包，`&mut` 把它当作可变引用的 trait 对象传入。
    scan_with_progress(root, target_root, &mut |_| {})
}

/// 与 `scan` 相同，但每扫描 256 个条目就回调一次已收集的文件数。
///
/// 第三个参数是 `&mut dyn FnMut(usize)`：`dyn Trait` 表示按 trait 对象做动态分发，
/// `&mut` 允许回调内部维护状态（这里是计数器）。之所以用 trait 对象而不是泛型
/// `F: FnMut(usize)`，是为了让 `scan` 与 `scan_with_progress` 共享同一份实现，
/// 否则每次传入不同的闭包类型都会生成一份新的单态化代码。
///
/// 遍历方式是自己维护栈的深度优先搜索：`pending` 里放待展开的目录（相对路径），
/// 循环弹出直到为空。相比递归，这在深目录树上不会消耗线程栈。
pub fn scan_with_progress(
    // 源根目录，已规范化。
    root: &Path,
    // 目标根目录，只用于路径长度检查，因此不要求存在。
    target_root: &Path,
    // 进度回调：参数是"目前已收集的文件数"。
    progress: &mut dyn FnMut(usize),
) -> io::Result<Manifest> {
    // 空清单起点：`Default` 派生出来的构造函数。
    let mut manifest = Manifest::default();
    // `seen` 统计遍历到的条目总数（含目录），用于控制回调频率。
    let mut seen = 0usize;
    // 待展开目录栈；初始只有空路径，代表源根目录自身。
    let mut pending: Vec<PathBuf> = vec![PathBuf::new()];

    // `while let` 在 `pop()` 返回 `Some` 时进入循环，栈空后自动结束。
    while let Some(rel_dir) = pending.pop() {
        // 空相对路径对应根目录本身，此时不能用 `join`（会产生多余的分隔符）。
        let abs = if rel_dir.as_os_str().is_empty() {
            root.to_path_buf()
        } else {
            root.join(&rel_dir)
        };
        // 读目录失败不直接返回错误，而是记录下来继续走：这样能一次报出全部问题。
        // `match` 与 `?` 的区别就在这里，`?` 会在第一个错误处提前返回。
        let read = match fs::read_dir(&abs) {
            Ok(rd) => rd,
            Err(err) => {
                manifest
                    .errors
                    .push(format!("cannot read directory {}: {err}", abs.display()));
                // `continue` 回到 `while` 顶部处理栈里的下一个目录。
                continue;
            }
        };

        // 先把这一层全部收下来，才能排序；`read_dir` 的顺序由文件系统决定，不可依赖。
        let mut items = Vec::new();
        // `read_dir` 返回的是"逐项产出 `Result` 的迭代器"：目录读到一半可能出错，
        // 因此每项都要单独判断，不能直接 `collect` 成结果向量。
        for item in read {
            match item {
                Ok(entry) => items.push(entry),
                // 单个条目读失败不算致命，记进 errors 继续。
                Err(err) => manifest.errors.push(format!(
                    "cannot read directory entry {}: {err}",
                    abs.display()
                )),
            }
        }
        // 按名字排序，让遍历顺序在多次运行之间保持一致（也便于测试断言）。
        // `sort_by_key` 只计算一次键，这里键是文件名引用，开销很小。
        items.sort_by_key(|e| e.file_name());

        // 逐个条目判断类型、构造相对路径、做规则检查。
        for item in items {
            let name = item.file_name();
            // `join` 拼出"相对源根目录"的路径，作为后续所有操作的唯一标识。
            let child_rel = rel_dir.join(&name);
            // 这里用 `symlink_metadata` 而不是 `metadata`：前者不跟随符号链接，
            // 因此能看出某个条目本身是链接，而不是它指向的目标。
            let meta = match fs::symlink_metadata(item.path()) {
                Ok(m) => m,
                // 元数据读不到就跳过这个条目，但记录下来。
                Err(err) => {
                    manifest.errors.push(format!(
                        "cannot read metadata {}: {err}",
                        item.path().display()
                    ));
                    continue;
                }
            };
            // 由元数据判断类型，避免再发一次系统调用。
            let file_type = meta.file_type();
            // 符号链接会被 sy 复制，但 rcca 无法对它计算哈希，因此计入跳过而不是静默丢弃。
            if file_type.is_symlink() {
                manifest.skipped_symlinks += 1;
                continue;
            }
            // 构造 `RelPath` 时会校验 `..`、绝对路径等情况；失败即视为路径规则冲突。
            // 这里需要 `clone()`：`child_rel` 后面还要用来入栈或取大小。
            let rel = match RelPath::new(child_rel.clone()) {
                Ok(r) => r,
                Err(err) => {
                    // 错误类型是 `String`，正好可以直接塞进冲突列表。
                    manifest.conflicts.push(err);
                    continue;
                }
            };
            // 平台相关的路径长度检查：目标根加相对路径是否超限。
            if let Some(problem) = path_problems(rel.as_path(), target_root) {
                manifest.conflicts.push(problem);
            }
            // Windows 保留名与非法字符检查；其他平台恒为 `None`。
            if let Some(problem) = windows_name_problems(rel.as_path()) {
                manifest.conflicts.push(problem);
            }
            // 源里出现形如 sy 临时文件的名字会造成混淆，提前报告。
            if looks_like_tmp(&name) {
                manifest.conflicts.push(format!(
                    "source file name collides with the temp file rule ({}{}{}): {}",
                    crate::paths::TMP_PREFIX,
                    "xxxxxxxx",
                    crate::paths::TMP_SUFFIX,
                    rel.display()
                ));
            }

            // 每个条目都计入 `seen`，用它控制回调频率。
            seen += 1;
            // 每 256 个条目上报一次，避免回调过于频繁。
            // `is_multiple_of` 比 `% 256 == 0` 更直白，也不会有取模的语义歧义。
            if seen.is_multiple_of(256) {
                // 回调收到的是"已收集的文件数"，与 `seen` 不同：目录不计入文件数。
                progress(manifest.files.len());
            }
            if file_type.is_dir() {
                // 目录入栈，稍后展开；此时还不知道其内容大小。
                pending.push(child_rel);
            } else if file_type.is_file() {
                // 普通文件：累加总字节数并记录条目。
                manifest.total_bytes += meta.len();
                manifest.files.push(Entry {
                    rel,
                    // `meta.modified()` 返回 `Result`，取不到修改时间时用 `.ok()` 变成 `None`。
                    size: meta.len(),
                    mtime: meta.modified().ok(),
                });
            } else {
                // 剩下的都是特殊文件：管道、套接字、设备节点等。sy 会把它们当作常规文件
                // 去读（管道会永久阻塞、设备可能读不完），因此记下第一个路径供 preflight 报错。
                manifest.skipped_special += 1;
                if manifest.first_special.is_none() {
                    manifest.first_special = Some(rel);
                }
            }
        }
    }

    // `sort_by` 配合 `RelPath` 派生的 `Ord`，得到稳定的清单顺序。
    manifest.files.sort_by(|a, b| a.rel.cmp(&b.rel));
    // 最后一次上报，确保进度行显示的是最终数量。
    progress(manifest.files.len());
    // `Ok(...)` 包装成功结果；函数返回 `io::Result<Manifest>`。
    Ok(manifest)
}

/// 逐个打开源文件检查可读性，返回读不到的那些及其原因。
///
/// 只 `open` 不读取：权限不足、文件已删除等问题在这一步就会暴露，
/// 而不会等到复制中途才发现。
pub fn probe_readable(root: &Path, files: &[Entry]) -> Vec<(RelPath, String)> {
    // 结果向量：每项是"相对路径 + 错误原因"。
    let mut bad = Vec::new();
    for entry in files {
        // `File::open` 成功后句柄会立刻析构关闭；这里只需要知道能否打开。
        if let Err(err) = fs::File::open(root.join(entry.rel.as_path())) {
            // `entry.rel` 需要克隆：`bad` 要拥有自己的数据，借用的条目生命周期不够长。
            bad.push((entry.rel.clone(), err.to_string()));
        }
    }
    bad
}

/// 按复制前记录的清单重新核对每个源文件。
///
/// 返回值与 `files` 按下标一一对应：`None` 表示正常，`Some(issue)` 表示有问题。
/// 用"同长度的结果向量"而不是只返回出错项，调用方（verify.rs）才能按索引取用，
/// 且不必关心顺序。
pub fn check_stability(root: &Path, files: &[Entry]) -> Vec<Option<SourceIssue>> {
    files
        .iter()
        // `map` 加 `collect` 是 Rust 里最常见的"逐项转换再收成容器"组合：
        // 目标容器的类型由函数返回类型 `Vec<Option<SourceIssue>>` 推断。
        .map(|entry| {
            // 按相对路径还原绝对路径，与复制前记录的清单对比。
            let abs = root.join(entry.rel.as_path());
            let meta = match fs::symlink_metadata(&abs) {
                Ok(m) => m,
                Err(err) => {
                    // `ErrorKind::NotFound` 说明文件在复制期间消失，其余情况归为读取失败。
                    // 闭包里的 `return` 只从这个闭包返回，不会提前结束外层函数。
                    return if err.kind() == io::ErrorKind::NotFound {
                        Some(SourceIssue::Vanished)
                    } else {
                        Some(SourceIssue::Changed(format!("cannot read metadata: {err}")))
                    };
                }
            };
            // 大小变化说明内容已经不同，不必再读。
            if meta.len() != entry.size {
                return Some(SourceIssue::Changed(format!(
                    "size changed from {} to {}",
                    entry.size,
                    meta.len()
                )));
            }
            // 大小相同但修改时间变了，也可能内容已改，需要报告。
            let now_mtime = meta.modified().ok();
            if mtime_changed(entry.mtime, now_mtime) {
                return Some(SourceIssue::Changed(
                    "modification time changed".to_string(),
                ));
            }
            // 走到这里说明该文件与清单一致。
            None
        })
        .collect()
}

/// 判断修改时间的变化是否超出容差。
///
/// 两个 `Option<SystemTime>` 用元组匹配一次处理完三种组合，
/// 比嵌套 `if let` 更清楚。
fn mtime_changed(before: Option<SystemTime>, after: Option<SystemTime>) -> bool {
    // 匹配的是"两个 `Option` 组成的元组"，因此模式里也是元组。
    match (before, after) {
        (Some(a), Some(b)) => {
            // `duration_since` 在时间倒退时返回 `Err`，此时取 `err.duration()`
            // 得到的仍是差值的绝对值，因此两个方向都能比较。
            let delta = match b.duration_since(a) {
                Ok(d) => d,
                Err(err) => err.duration(),
            };
            // `>` 直接比较两个 `Duration`，正好是"超出容差"的含义。
            delta > MTIME_TOLERANCE
        }
        (None, None) => false,
        // 只有一侧已知时无从比较，不报变化，避免误报；`_` 覆盖剩下两种组合。
        _ => false,
    }
}

// 测试模块只在 `cargo test` 时编译。
#[cfg(test)]
mod tests {
    use super::*;
    // `Write` trait 必须在作用域内，`f.write_all(...)` 才能调用。
    use std::io::Write;

    /// 建一个临时目录，`TempDir` 在离开作用域时会自动删除它（RAII）。
    fn tmpdir() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[test]
    fn scan_collects_files() {
        // 源与目标各用一个临时目录，二者平级，避免触发嵌套检查。
        let src = tmpdir();
        let dst = tmpdir();
        // 建两层目录并各放一个文件，另外建一个空目录验证它不计入文件数。
        fs::create_dir_all(src.path().join("a/b")).unwrap();
        fs::write(src.path().join("a/b/c.MOV"), b"hello").unwrap();
        fs::write(src.path().join("top.txt"), b"x").unwrap();
        fs::create_dir_all(src.path().join("empty")).unwrap();

        let manifest = scan(src.path(), dst.path()).unwrap();
        // 空目录不计入文件数，只统计普通文件。
        assert_eq!(manifest.files.len(), 2);
        // 5 + 1 = 6 字节。
        assert_eq!(manifest.total_bytes, 6);
        assert!(manifest.errors.is_empty());
        // 清单按路径排序，所以断言可以直接写出期望顺序。
        // 期望值用 `PathBuf` 而不是 `display()` 的字符串：`Path` 的比较按路径片段进行，
        // Windows 上的 `a\b\c.MOV` 与 `a/b/c.MOV` 因此相等，断言不必跟着平台换分隔符。
        let rels: Vec<PathBuf> = manifest
            .files
            .iter()
            .map(|f| f.rel.as_path().to_path_buf())
            .collect();
        assert_eq!(
            rels,
            vec![PathBuf::from("a/b/c.MOV"), PathBuf::from("top.txt")]
        );
    }

    #[test]
    fn scan_skips_symlinks_and_special() {
        let src = tmpdir();
        let dst = tmpdir();
        fs::write(src.path().join("real.txt"), b"data").unwrap();
        // 符号链接只在 Unix 上创建；`#[cfg(unix)]` 让测试在其他平台上依然能编译运行。
        // 第一个参数是链接内容（相对路径），第二个是链接自身的路径。
        #[cfg(unix)]
        std::os::unix::fs::symlink("real.txt", src.path().join("link.txt")).unwrap();
        let manifest = scan(src.path(), dst.path()).unwrap();
        // 只有真实文件进入清单。
        assert_eq!(manifest.files.len(), 1);
        // 链接被计入跳过，而不是静默消失。
        #[cfg(unix)]
        assert_eq!(manifest.skipped_symlinks, 1);
    }

    #[cfg(unix)]
    #[test]
    fn scan_records_the_first_special_file() {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;

        let src = tmpdir();
        let dst = tmpdir();
        fs::write(src.path().join("real.txt"), b"x").unwrap();
        // 管道节点：`is_file()` 为假且不是链接，因此归入特殊文件。
        let fifo = src.path().join("pipe.fifo");
        let path = CString::new(fifo.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o644) }, 0);

        let manifest = scan(src.path(), dst.path()).unwrap();
        assert_eq!(manifest.skipped_special, 1);
        // 记录第一个路径，preflight 才能指出该删哪个文件。
        assert_eq!(
            manifest.first_special.as_ref().map(|rel| rel.display()),
            Some("pipe.fifo".to_string())
        );
        // 常规文件照旧进入清单。
        assert_eq!(manifest.files.len(), 1);
    }

    #[test]
    fn scan_flags_tmp_name_conflict() {
        let src = tmpdir();
        let dst = tmpdir();
        // 故意造一个形如 sy 临时文件的源文件。
        let mut f = fs::File::create(src.path().join(".sy-0badc0de.tmp")).unwrap();
        // `write_all` 来自 `Write` trait，需要上面 `use std::io::Write;` 才能调用。
        f.write_all(b"x").unwrap();
        let manifest = scan(src.path(), dst.path()).unwrap();
        // 形如 sy 临时文件的源文件必须被报告为冲突。
        assert!(!manifest.conflicts.is_empty());
    }

    #[test]
    fn mtime_change_is_detected() {
        let src = tmpdir();
        let dst = tmpdir();
        let path = src.path().join("f.bin");
        fs::write(&path, b"abc").unwrap();
        let manifest = scan(src.path(), dst.path()).unwrap();
        // 刚开始时源与清单一致。
        let issues = check_stability(src.path(), &manifest.files);
        assert!(issues[0].is_none());

        // 大小改变后应当被识别为"已变化"。
        // `matches!` 用模式判断变体，同时忽略内部的原因文本。
        fs::write(&path, b"abcdef").unwrap();
        let issues = check_stability(src.path(), &manifest.files);
        assert!(matches!(issues[0], Some(SourceIssue::Changed(_))));

        // 文件消失后应当被识别为 `Vanished`。
        // `SourceIssue` 派生了 `PartialEq`，因此可以直接比较相等。
        fs::remove_file(&path).unwrap();
        let issues = check_stability(src.path(), &manifest.files);
        assert_eq!(issues[0], Some(SourceIssue::Vanished));
    }
}
