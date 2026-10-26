//! 路径规则：相对路径类型、临时文件命名、目标布局与长度检查。
//!
//! 引擎层只传相对源根目录的路径（`RelPath`），任意绝对路径不会进入复制或校验逻辑。
//! 这是一条设计约束：把"相对路径"做成独立类型后，签名本身就拒绝了绝对路径，
//! 运行时也就不必在每处再判断一次。

// `OsString` / `OsStr` 是"操作系统原生字符串"的所有权版和借用版：
// 它们不保证是合法 UTF-8（Unix 上可以是任意字节），因此拼路径必须用它们。
use std::ffi::OsString;
// `Component` 是路径片段枚举，`Path`/`PathBuf` 分别是借用的和拥有的路径类型。
use std::path::{Component, Path, PathBuf};

/// sy 使用的临时文件前后缀：`.sy-xxxxxxxx.tmp`。
///
/// `pub const` 常量在编译期内联，类型标注 `&str` 会自动带上 `'static` 生命周期，
/// 因为字符串字面量存放在二进制的只读数据段，整个进程期间都有效。
pub const TMP_PREFIX: &str = ".sy-";
// 与前缀配对使用，`looks_like_tmp` 同时检查两者。
pub const TMP_SUFFIX: &str = ".tmp";
/// 校验失败的文件被改名隔离时使用的后缀。
pub const BAD_SUFFIX: &str = ".badhash";

/// 相对源根目录的路径，构造时完成校验。
///
/// 这是 Rust 常见的"newtype"写法：元组结构体 `RelPath(PathBuf)` 内部只有一个字段，
/// 字段默认私有，外部只能通过 `new` 构造。于是"已校验的相对路径"成为一种类型保证，
/// 后续函数拿到 `&RelPath` 就无需再校验一次。
///
/// 派生这些 trait 各有用途：`Clone` 便于把条目放进多个集合；`Debug` 用于断言失败时打印；
/// `PartialEq`/`Eq` 让路径可比较；`PartialOrd`/`Ord` 让清单能按路径排序，从而让多次运行
/// 的输出顺序稳定；`Hash` 让 `RelPath` 能作为 `HashSet` 的键（main.rs 里筛掉不可读文件
/// 时就用到了这一点）。
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RelPath(PathBuf);

// 本类型的方法都写在 `impl RelPath` 里；构造、访问、显示分成三类。
impl RelPath {
    /// 构造并校验：必须是相对路径，不含 `..`，也不带根前缀。
    ///
    /// 参数写成 `impl Into<PathBuf>`，调用方传 `&str`、`String`、`PathBuf` 都可以。
    /// 返回类型写全了 `std::result::Result`，因为本文件没有引入 `crate::error::Result`，
    /// 这里的错误类型是朴素的 `String`，只用于上报给 preflight 的冲突列表。
    pub fn new(path: impl Into<PathBuf>) -> std::result::Result<Self, String> {
        // `into()` 把参数统一转成 `PathBuf`，后面的代码只需要处理一种类型。
        let path = path.into();
        // `OsStr` 与 `str` 的区别：路径在 Unix 上可以是任意字节，在 Windows 上是
        // UTF-16，因此不能假设是合法的 UTF-8。判断空、拼字符串要用 `OsStr` 的接口。
        if path.as_os_str().is_empty() {
            return Err("relative path is empty".to_string());
        }
        // 绝对路径直接拒绝：本类型承诺"相对源根目录"。
        if path.is_absolute() {
            // `{}` 调用 `Display`；`Path::display()` 返回一个能安全打印非 UTF-8 路径的包装。
            return Err(format!("relative path is absolute: {}", path.display()));
        }
        // `components()` 把路径切成有语义的片段，比字符串切分安全：
        // `..` 和根目录会被单独识别出来，而不是当成普通字符。
        for comp in path.components() {
            // `match` 必须穷尽所有变体；`Component` 一共四个变体，下面全部覆盖。
            match comp {
                // 正常名字与 `.` 可接受；`|` 是 or 模式，两种变体共用同一个分支。
                Component::Normal(_) | Component::CurDir => {}
                // `..` 会跳出目标根目录，必须拒绝。
                Component::ParentDir => {
                    return Err(format!("relative path contains ..: {}", path.display()));
                }
                // `Prefix` 只在 Windows 出现（如 `C:`）；`RootDir` 是路径开头的 `/`。
                Component::RootDir | Component::Prefix(_) => {
                    return Err(format!(
                        "relative path has a root prefix: {}",
                        path.display()
                    ));
                }
            }
        }
        // 走到这里说明全部片段都合法，把校验过的值包进 newtype 返回。
        Ok(RelPath(path))
    }

    /// 取出内部的 `&Path`。返回引用而不是克隆，避免每次调用都复制一份路径。
    pub fn as_path(&self) -> &Path {
        // `self.0` 访问元组结构体的第 0 个字段；`&` 取引用返回。
        &self.0
    }

    /// 用于日志和显示；文件名可能不是 UTF-8，所以转换是有损的。
    ///
    /// `to_string_lossy()` 返回 `Cow<str>`：能零拷贝借用时就不分配，遇到非法字节才替换成
    /// `U+FFFD` 并分配新字符串。`into_owned()` 把它统一转成 `String`，
    /// 这样函数签名不必暴露 `Cow` 这种实现细节。
    pub fn display(&self) -> String {
        self.0.to_string_lossy().into_owned()
    }

    /// 取最后一段文件名。返回 `&OsStr` 而不是 `&str`，因为文件名未必是 UTF-8。
    ///
    /// 理论上构造时已保证非空，`unwrap_or_default()` 只是让这里不必处理 `Option`：
    /// 它把 `None` 变成 `OsStr::new("")`。
    pub fn file_name(&self) -> &std::ffi::OsStr {
        self.0.file_name().unwrap_or_default()
    }
}

/// 校验失败文件的隔离路径：同目录下的 `.<name>.badhash`。
///
/// 之所以在文件名前加点，是为了让隔离文件变成隐藏文件，既不干扰正常文件名的匹配，
/// 也一眼能看出它不是正常产物。
pub fn bad_rel_path(rel: &Path) -> PathBuf {
    // `parent()` 对只有文件名的路径返回 `Some("")` 而不是 `None`，
    // 但为了覆盖极端情况仍用 `unwrap_or_else` 兜底（传闭包，只有需要时才构造默认值）。
    let parent = rel.parent().unwrap_or_else(|| Path::new(""));
    let name = rel.file_name().unwrap_or_default();
    // `OsString` 可以增量拼接：避免把非 UTF-8 的文件名硬转成 UTF-8 再拼回去。
    let mut bad_name = OsString::from(".");
    // `push` 直接追加一段 `OsStr`，不经过字符串转换。
    bad_name.push(name);
    bad_name.push(BAD_SUFFIX);
    // 父目录为空串说明文件就在源根目录下，此时不能再 `join`，否则会多出一个分隔符。
    if parent.as_os_str().is_empty() {
        // 文件就在源根目录下，没有父目录可拼；`PathBuf::from(OsString)` 拥有所有权。
        PathBuf::from(bad_name)
    } else {
        // `join` 拼出新路径，返回新的 `PathBuf`，两个输入都不被消费。
        parent.join(bad_name)
    }
}

/// 判断一个名字是否像 sy 的临时文件（也兼容旧版 rcca 的 `.rcptmp`）。
///
/// 这里的 `&&` 与 `||` 有短路语义：左侧不成立时右侧不会被求值。
/// 第二个分支额外检查长度，是为了不把恰好叫 `.rcptmp` 的文件误判为临时产物。
pub fn looks_like_tmp(name: &std::ffi::OsStr) -> bool {
    // 有损转换得到 `Cow<str>`；变量 `s` 可以直接调用 `str` 的方法。
    let s = name.to_string_lossy();
    // 两个条件用 `||` 连接：新版 sy 的临时文件名，或旧版 rcca 的临时文件名。
    // 第二个条件里的 `.len() > ".rcptmp".len() + 1` 要求前面至少还有一个字符，
    // 因此文件名恰好等于 `.rcptmp` 时不会被误判。
    (s.starts_with(TMP_PREFIX) && s.ends_with(TMP_SUFFIX))
        || (s.starts_with('.') && s.ends_with(".rcptmp") && s.len() > ".rcptmp".len() + 1)
}

/// 把 Windows 规范路径上的 `\\?\`（以及 `\\?\UNC\`）前缀去掉。
///
/// `fs::canonicalize` 在 Windows 上总是返回带 `\\?\` 前缀的路径：这个前缀让系统跳过
/// 260 字符的路径规范化和名字清理，是给 API 用的写法，不是给用户看的。原样打印出来
/// 只会让 `SRC` / `DST` 两行、错误提示里的路径都多出一段无意义的字符，所以路径在进入
/// 后续流程之前先还原成普通写法。其他平台（以及不带该前缀的路径）原样返回。
///
/// 去掉前缀不会让原本可用的路径变成超长路径：清单里的每一个条目都已经由
/// `path_problems` 按平台的 `path_limit()` 检查过。
pub fn strip_verbatim(path: &Path) -> PathBuf {
    #[cfg(target_os = "windows")]
    {
        // 前缀是 `Component::Prefix` 的一个变体；只有 `Verbatim*` 两类需要改写。
        use std::path::Prefix;

        // 逐个取片段：先拿走第一个前缀片段，剩下的按顺序拼回普通路径。
        let mut components = path.components();
        let Some(Component::Prefix(prefix)) = components.next() else {
            return path.to_path_buf();
        };
        // 尾随空格或点只有在 verbatim 路径里才是名字的一部分，普通 Win32 路径会把它们
        // 裁掉。这样的路径一旦去掉前缀就指向另一个文件，因此保持原样。
        if components.clone().any(is_verbatim_only_name) {
            return path.to_path_buf();
        }
        match prefix.kind() {
            // `\\?\C:\card\a` 还原成 `C:\card\a`。
            Prefix::VerbatimDisk(letter) => {
                // `C:` 后面必须紧跟分隔符，否则会变成"驱动器相对路径"（`C:a`）。
                let mut out = PathBuf::from(format!("{}:\\", letter as char));
                for comp in components {
                    out.push(comp.as_os_str());
                }
                out
            }
            // `\\?\UNC\server\share\a` 还原成 `\\server\share\a`。
            Prefix::VerbatimUNC(server, share) => {
                let mut out = PathBuf::from(r"\\");
                out.push(server);
                out.push(share);
                for comp in components {
                    out.push(comp.as_os_str());
                }
                out
            }
            // 其余前缀不在 canonicalize 的输出范围内，保持原样，避免猜错。
            _ => path.to_path_buf(),
        }
    }
    #[cfg(not(target_os = "windows"))]
    {
        // 非 Windows 平台没有这种前缀，统一返回一份可拥有的副本。
        let _ = path;
        path.to_path_buf()
    }
}

/// 该路径片段是否只有 verbatim 路径才能表达（尾随空格或点）。
///
/// 普通 Win32 路径在规范化时会把文件名末尾的空格与点裁掉，因此 `a.` 与 `a` 是同一个
/// 文件；verbatim 路径则按字面处理。去掉 `\\?\` 前缀会改变这种路径的含义。
#[cfg(target_os = "windows")]
fn is_verbatim_only_name(comp: Component<'_>) -> bool {
    match comp {
        Component::Normal(name) => {
            let s = name.to_string_lossy();
            s.ends_with(' ') || s.ends_with('.')
        }
        // 根目录与 `..` 等片段不受这条规则影响。
        _ => false,
    }
}

/// 目标根目录是否等于源根目录、位于源内部，或者反过来。两个路径必须已经规范化。
///
/// `Path::starts_with` 是按路径片段比较而不是按字符串前缀比较：`/card2` 不以
/// `/card` 开头，这一点与 `str::starts_with` 不同，正好符合这里的需求。
pub fn nesting_problem(source: &Path, target: &Path) -> Option<String> {
    // 完全相同：复制会覆盖自身，必须先拒绝。
    if source == target {
        return Some("source and target are the same directory".to_string());
    }
    // 目标在源内部：复制过程中会不断发现新文件，形成递归。
    if target.starts_with(source) {
        return Some(format!(
            "target is inside the source (the copy would recurse into itself): source {}, target {}",
            source.display(),
            target.display()
        ));
    }
    // 源在目标内部：两边内容会混在一起，删除语义还可能误删源文件。
    if source.starts_with(target) {
        return Some(format!(
            "source is inside the target (their contents would mix): source {}, target {}",
            source.display(),
            target.display()
        ));
    }
    // 嵌套检查通过就返回 `None`；调用方用 `if let Some(problem) = ...` 取出问题。
    None
}

/// 一段路径在平台上的长度，用于与文件系统的限制比较。
///
/// 两个平台的口径本来不同：Unix 以字节计（`NAME_MAX` 255 字节、`PATH_MAX` 4096 字节），
/// Windows 以字符计（`MAX_PATH` 260 个 UTF-16 码元，单段 255 个）。`OsStr::len()` 在
/// Windows 上返回的是 WTF-8 字节数，一个汉字会被算成 3，于是完全合法的中文路径
/// （90 个汉字的文件名）会被误判为超长；`encode_wide().count()` 才是 Windows 认的口径。
fn os_len(s: &std::ffi::OsStr) -> usize {
    #[cfg(target_os = "windows")]
    {
        // `encode_wide` 产出 UTF-16 码元序列，长度即 Windows 计数的单位。
        use std::os::windows::ffi::OsStrExt;
        s.encode_wide().count()
    }
    #[cfg(not(target_os = "windows"))]
    {
        // Unix 的限制以字节计，`OsStr::len()` 正是字节数。
        s.len()
    }
}

/// 检查目标根目录加相对路径是否超过平台的路径总长或单段长度限制。
///
/// 长度按平台自己的口径计算（见 `os_len`），不按 UTF-8 字节。
pub fn path_problems(rel: &Path, target_root: &Path) -> Option<String> {
    // 总长 = 目标根 + 分隔符(1) + 相对路径；分隔符在两种口径下都算 1。
    let total = os_len(target_root.as_os_str()) + 1 + os_len(rel.as_os_str());
    if total > path_limit() {
        return Some(format!(
            "path length {} exceeds the platform limit {}: {}",
            total,
            // 同一个函数调用两次：它只是返回常量，没有副作用。
            path_limit(),
            target_root.join(rel).display()
        ));
    }
    // 单独检查每一段的长度：多数文件系统单段上限是 255。
    for comp in rel.components() {
        let len = os_len(comp.as_os_str());
        if len > 255 {
            return Some(format!(
                "file name component length {} exceeds 255: {}",
                len,
                rel.display()
            ));
        }
    }
    // 两轮检查都通过。
    None
}

/// 各平台的路径长度上限。
///
/// 返回值的单位与 `os_len` 一致：Unix 是字节，Windows 是 UTF-16 码元。
///
/// 同一个函数名写了三份定义，靠 `#[cfg(target_os = ...)]` 在编译期只保留一份：
/// 这是条件编译，不是运行时分支，未选中的代码根本不会被编译进二进制。
/// 三个分支的返回类型必须一致，否则调用点会因找不到唯一实现而编译失败。
fn path_limit() -> usize {
    // 每个分支都用花括号包住一个表达式块，块的值就是函数返回值。
    // 只有当前平台对应的那个块会被编译。
    #[cfg(target_os = "windows")]
    {
        259
    }
    // macOS 的 PATH_MAX 是 1024，含结尾空字节，因此可用字节是 1023。
    #[cfg(target_os = "macos")]
    {
        1023
    }
    // 其余平台（主要是 Linux）按 PATH_MAX 4095 计算。
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        4095
    }
}

/// 把非法的或保留的 Windows 文件名报告给 preflight（其他平台恒为空）。
///
/// 函数在非 Windows 上也存在，只是函数体变成"忽略参数、返回 `None`"，
/// 这样调用方（scan.rs）不必到处写 `#[cfg]`。参数在未使用的分支里用 `let _ = rel;`
/// 显式丢弃，避免触发"未使用变量"的编译警告。
pub fn windows_name_problems(rel: &Path) -> Option<String> {
    #[cfg(target_os = "windows")]
    {
        // 定长数组 `[&str; 22]`：长度写进类型，`contains` 因此可以在数组上直接查找。
        const RESERVED: [&str; 22] = [
            "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7",
            "COM8", "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
        ];
        // 逐段检查：只有文件名部分要判断，目录名同样受限，所以每一段都看。
        for comp in rel.components() {
            // 有损转换即可，非法字节不会命中下面的保留字或非法字符判断。
            let s = comp.as_os_str().to_string_lossy();
            // `contains(['<', '>', ...])`：字符数组作为模式，只要出现其中一个就算命中。
            // `ends_with(['.', ' '])` 同理，对应 Windows 不允许文件名以点或空格结尾的规则。
            if s.contains(['<', '>', ':', '"', '|', '?', '*']) || s.ends_with(['.', ' ']) {
                return Some(format!(
                    "file name contains characters Windows rejects: {s}"
                ));
            }
            // Windows 的保留名判断不看扩展名，所以取第一段并转大写后比对。
            // `split('.').next()` 一定返回 `Some`（即使空串也会产出空串），
            // 这里仍用 `unwrap_or("")` 兜底，避免依赖这个细节。
            let stem = s.split('.').next().unwrap_or("").to_ascii_uppercase();
            // `contains(&stem.as_str())` 在 `[&str; 22]` 上查找，需要借到 `&str`。
            if RESERVED.contains(&stem.as_str()) {
                return Some(format!("file name is a reserved Windows name: {s}"));
            }
        }
        // 全部合格。
        None
    }
    #[cfg(not(target_os = "windows"))]
    {
        // 非 Windows 平台没有这些规则，直接返回"无问题"。
        // `let _ = rel;` 显式使用一次参数，告诉编译器"不是忘了它"。
        let _ = rel;
        None
    }
}

// 测试模块同样只在 `cargo test` 时编译。
#[cfg(test)]
mod tests {
    // 引入父模块的全部条目，包括私有函数 `path_limit` 之外的所有工具函数。
    use super::*;

    #[test]
    fn bad_path_is_sibling_hidden_name() {
        // 断言不匹配时 `assert_eq!` 会打印两侧的值，因此两边类型都只需要 `Debug`。
        // `Path::new` 借用字符串字面量构造路径，不分配内存。
        assert_eq!(
            bad_rel_path(Path::new("a/c.MOV")),
            PathBuf::from("a/.c.MOV.badhash")
        );
    }

    #[test]
    fn rel_path_rejects_escape() {
        // 三类危险输入：向上越界、绝对路径、空路径。
        // `is_err()` 只关心结果分支，不关心具体错误文本。
        assert!(RelPath::new("../x").is_err());
        assert!(RelPath::new("/abs").is_err());
        assert!(RelPath::new("").is_err());
        // 正常的相对路径要能通过：`is_ok()` 是 `is_err()` 的反面。
        assert!(RelPath::new("a/b").is_ok());
    }

    #[test]
    fn nesting_detection() {
        // 复用同一个 `&Path`，说明这些函数都不消费参数。
        let s = Path::new("/data/card");
        // 目标在源内部、两者相同、源在目标内部，三种都要被识别。
        assert!(nesting_problem(s, Path::new("/data/card/sub")).is_some());
        assert!(nesting_problem(s, s).is_some());
        assert!(nesting_problem(Path::new("/data/card/sub"), s).is_some());
        // 平级目录是合法布局：返回 `None`。
        assert!(nesting_problem(s, Path::new("/out/copy")).is_none());
    }

    // Windows 上的 `fs::canonicalize` 一定返回带 `\\?\` 前缀的路径。这里用字面量验证
    // 前缀被正确还原：盘符路径、UNC 路径，以及本来就没有前缀的路径。
    #[cfg(target_os = "windows")]
    #[test]
    fn verbatim_prefix_is_removed() {
        assert_eq!(
            strip_verbatim(Path::new(r"\\?\C:\card\100MEDIA\A001.MOV")),
            PathBuf::from(r"C:\card\100MEDIA\A001.MOV")
        );
        assert_eq!(
            strip_verbatim(Path::new(r"\\?\UNC\server\share\card\a.bin")),
            PathBuf::from(r"\\server\share\card\a.bin")
        );
        // 没有前缀的路径原样返回，不能因为"没有可剥的东西"而丢掉片段。
        assert_eq!(
            strip_verbatim(Path::new(r"C:\card")),
            PathBuf::from(r"C:\card")
        );
        // 尾随空格或点只有 verbatim 路径才表达得出，去掉前缀会让路径指向另一个文件，
        // 因此这类路径必须原样保留。
        assert_eq!(
            strip_verbatim(Path::new(r"\\?\C:\card\name.")),
            PathBuf::from(r"\\?\C:\card\name.")
        );
        assert_eq!(
            strip_verbatim(Path::new(r"\\?\C:\card\name ")),
            PathBuf::from(r"\\?\C:\card\name ")
        );
    }

    // 长度限制必须按平台自己的计数单位判断：同一个 90 汉字的文件名在 Windows 上
    // 是 90 个 UTF-16 码元（合法），在 Unix 上是 270 字节（超过单段 255）。
    #[test]
    fn component_limit_follows_the_platform_unit() {
        let name = format!("{}.JPG", "照".repeat(90));
        let rel = Path::new(&name);
        let root = Path::new("/t");
        #[cfg(target_os = "windows")]
        {
            // Windows 按 UTF-16 码元计：94 个码元，远低于 255。
            assert!(path_problems(rel, root).is_none());
        }
        #[cfg(not(target_os = "windows"))]
        {
            // Unix 按字节计：270 字节超过 255。
            assert!(path_problems(rel, root).is_some());
        }
    }

    // 总长限制同样按平台单位：Windows 的 259 是字符数而不是字节数。
    #[cfg(target_os = "windows")]
    #[test]
    fn total_limit_counts_utf16_units_on_windows() {
        // 目标根 "C:\" 加 38 个字符，加分隔符，加 100 个汉字 = 141 个码元。
        let root = format!("C:\\{}", "r".repeat(38));
        let rel = "照".repeat(100);
        assert!(path_problems(Path::new(&rel), Path::new(&root)).is_none());
        // 300 个汉字仍然超过 Windows 的 259 上限，必须被拒绝。
        let too_long = "照".repeat(300);
        assert!(path_problems(Path::new(&too_long), Path::new(&root)).is_some());
    }

    #[test]
    fn tmp_name_detection() {
        // `OsStr::new` 从字符串字面量构造，供只接受 `&OsStr` 的接口使用。
        assert!(looks_like_tmp(std::ffi::OsStr::new(".sy-1a2b3c4d.tmp")));
        // 旧版 rcca 的临时文件名也要能识别出来。
        assert!(looks_like_tmp(std::ffi::OsStr::new(".a.MOV.rcptmp")));
        // 普通文件名不能误判。
        assert!(!looks_like_tmp(std::ffi::OsStr::new("a.MOV")));
        // 只有后缀本身不算临时文件，长度检查拦住了它。
        assert!(!looks_like_tmp(std::ffi::OsStr::new(".rcptmp")));
    }
}
