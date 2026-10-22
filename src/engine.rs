//! 复制阶段：驱动内嵌的 sy 引擎（`vendor/sy`，上游 nijaru/sy v0.4.1）。
//!
//! rcca 只负责映射选项与路径语义：源目录按"内容"传入（等价于
//! `sy <source>/ <target> --delete`）；遍历和校验都留在 rcca 这一侧。

// `Path` 是借用的路径类型，函数参数一律用它；需要拥有所有权时才用 `PathBuf`。
use std::path::Path;

// 从内嵌 sy 里引入引擎、配置和三种删除模式；`SyncStats` 是 sy 返回的计数器。
use sy::sync::{DeleteMode, SyncConfig, SyncEngine, SyncStats};
// `LocalTransport` 表示"源和目标都在本机"，不做网络传输。
use sy::transport::local::LocalTransport;

// `crate::` 前缀指本 crate 的根模块；`use` 里的花括号一次引入多个名字。
use crate::error::{Error, Result};

/// 此处内嵌的上游 sy 版本。
pub const VERSION: &str = "v0.4.1";

/// 删除保护阈值（百分比），与上游 `--max-delete 50%` 一致。
const DELETE_THRESHOLD: u8 = 50;

/// sy 用 0 表示"不限制逐文件错误数"，这也是 rcca 的默认语义。
const UNLIMITED_ERRORS: usize = 0;

/// 传给 sy 的复制选项。
///
/// 结构体没有 `Default` 实现，而是提供一个语义明确的 `mirror()` 构造器：
/// 让调用方必须显式选择语义，比依赖一组隐式默认值安全。
#[derive(Debug, Clone)]
pub struct Options {
    /// 删除目标中源里已不存在的文件（镜像语义）。
    pub delete: bool,
    /// 交给 sy 的并行传输数（对应它的 `-j`），1 表示串行。
    pub jobs: usize,
    /// 错误上限，用来实现 --fail-fast；`None` 表示不限，
    /// 由 `effective_max_errors` 换算成 sy 的表示法。
    pub max_errors: Option<usize>,
}

impl Options {
    /// 默认的镜像选项：保留删除语义，其余沿用上游默认值。
    pub fn mirror() -> Self {
        // `Self { ... }` 是结构体字面量，必须写全所有字段（除非用 `..other` 展开）。
        Self {
            // 镜像语义的核心：目标多出来的文件要删掉。
            delete: true,
            // 默认串行，与命令行的 `-j` 默认值保持一致。
            jobs: 1,
            // `None` 表示不对错误数设限。
            max_errors: None,
        }
    }
}

/// 把 `max_errors` 换算成 sy 的取值：`None` -> 0（sy 表示不限），`Some(n)` -> `n`。
///
/// 注意不能只在上限存在时才赋值：`SyncConfig::test_default()` 里 `max_errors` 是 100，
/// 留着不动会让"默认不限"变成一条没有出现在文档里的 100 条上限。
fn effective_max_errors(opts: &Options) -> usize {
    opts.max_errors.unwrap_or(UNLIMITED_ERRORS)
}

/// 注册"某个文件开始复制"时的回调，界面据此显示文件名。
///
/// 泛型参数上的约束 `F: Fn(&Path) + Send + Sync + 'static` 三者都有必要：
/// `Fn` 表示可以像函数那样调用；`Send` 让回调能跨线程移动；`Sync` 让它能被多个线程
/// 同时共享；`'static` 表示回调不借用短生命周期的数据，因为 sy 会长期保存它。
///
/// `Box::new(observer)` 把具体类型装箱成 trait 对象，sy 侧才能保存"任意实现了该
/// 约束的闭包"；如果不装箱，泛型参数会传导到 sy，使它无法存放回调。
pub fn set_file_observer<F>(observer: F)
// `where` 子句与写在尖括号里等价，只是当约束较长时可读性更好。
where
    F: Fn(&Path) + Send + Sync + 'static,
{
    // 装箱后调用 sy 的同名函数；所有权随着 `Box` 一起交给 sy，
    // 回调从此活在 sy 的静态变量里，直到进程结束或被再次覆盖。
    sy::sync::progress::set_file_observer(Box::new(observer));
}

/// 注册每个任务完成时的回调：该任务计入的字节数，以及它是否传输了一个普通文件。
pub fn set_bytes_observer<F>(observer: F)
where
    // 签名 `Fn(u64, bool)` 表示接收两个参数、无返回值；元组形式写成 `Fn((u64, bool))`
    // 才是"一个元组参数"，这里刻意用多参数形式。
    F: Fn(u64, bool) + Send + Sync + 'static,
{
    sy::sync::progress::set_bytes_observer(Box::new(observer));
}

/// 把源目录镜像到目标目录，返回 sy 的计数器。
pub fn copy_tree(source: &Path, target: &Path, opts: &Options) -> Result<SyncStats> {
    // 先拿一份 sy 的测试默认配置，再覆盖 rcca 关心的字段，这样上游新增的字段
    // 不会因为漏写而拿到零值。
    // `mut` 是必须的：下面要逐字段修改这份配置。
    let mut config = SyncConfig::test_default();
    // sy 在这里只负责搬运字节：进度条、计划信息、逐文件进度全部关闭，
    // 界面完全由 rcca 绘制。
    config.quiet = true;
    config.progress = false;
    // `max(1)` 保证至少一个传输槽位，避免 0 导致卡死。
    config.max_concurrent = opts.jobs.max(1);
    // `if` 在这里当表达式用：两个分支都产生一个 `DeleteMode` 值，赋给同名字段。
    config.delete = if opts.delete {
        // `Enabled` 是带字段的变体（结构体式变体），用花括号构造。
        DeleteMode::Enabled {
            threshold: DELETE_THRESHOLD,
            // `force: false` 表示不绕过保护，超阈值时让 sy 拒绝执行。
            force: false,
        }
    } else {
        // 无形段变体，直接写名字即可。
        DeleteMode::Disabled
    };
    // `format!("{DELETE_THRESHOLD}%")` 直接引用同名的常量，得到 "50%"。
    // sy 期望这个字段是 `Option<String>` 形式的阈值描述，所以包进 `Some`。
    config.max_delete = Some(format!("{DELETE_THRESHOLD}%"));
    // 显式写入上限：`None` 换算成 sy 的 0（不限），不能沿用 `test_default()` 的 100。
    config.max_errors = effective_max_errors(opts);

    // sy 是异步实现，需要 tokio 运行时。rcca 本身是同步程序，因此在这里临时建一个，
    // 用 `block_on` 把异步调用当同步用；运行时随后随作用域结束而销毁。
    // 建造者模式链式调用：`new_multi_thread()` 返回 builder，`enable_all()` 打开
    // 网络与定时器等驱动，`build()` 才真正创建运行时并返回 `Result`。
    // `map_err` 把构建错误包装成 rcca 自己的错误类型，才能用 `?` 向上抛。
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|err| Error::copy(format!("cannot create the async runtime: {err}")))?;

    // 引擎持有配置和传输实现；`LocalTransport::new()` 表示本机读、本机写。
    let engine = SyncEngine::with_config(LocalTransport::new(), config);
    // `block_on` 在内部驱动异步任务直到结束，把 Future 的结果当同步返回值使用。
    // 它返回 sy 自己的错误类型，这里统一转成 `Error::Copy`（退出码 1）。
    runtime
        .block_on(engine.sync(source, target))
        .map_err(|err| Error::copy(format!("sy: {err}")))
}

#[cfg(test)]
mod tests {
    // 引入父模块的一切，包括 `Options`、`copy_tree` 和两个 `set_*_observer`。
    use super::*;

    #[test]
    fn mirror_options_enable_delete() {
        let opts = Options::mirror();
        // 镜像语义必须默认开启删除，其余保持串行、无错误上限。
        assert!(opts.delete);
        assert_eq!(opts.jobs, 1);
        // `Option` 用 `is_none()` 判断，比 `assert_eq!(x, None)` 更清楚。
        assert!(opts.max_errors.is_none());
    }

    #[test]
    fn only_fail_fast_caps_the_error_count() {
        // 默认不限错误数：必须映射成 sy 的 0，而不是沿用 `test_default()` 的 100。
        assert_eq!(effective_max_errors(&Options::mirror()), 0);
        // --fail-fast 对应"最多一个错误"。
        let fail_fast = Options {
            max_errors: Some(1),
            ..Options::mirror()
        };
        assert_eq!(effective_max_errors(&fail_fast), 1);
    }

    #[test]
    fn file_observer_sees_each_copied_file() {
        // 回调和测试代码都要访问计数器，因此用 `Arc<AtomicUsize>`：
        // `Arc` 提供跨线程的共享所有权，原子类型提供无锁的自增。
        // 这两行 `use` 写在函数内部，只在本函数生效，不污染整个模块。
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        // `Arc::new` 创建共享所有权；`Arc::clone(&seen)` 只是把引用计数加一。
        let seen = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&seen);
        // 回调要求 `'static`，所以用 `move` 把 `counter` 移进闭包；
        // 若直接借用 `seen`，闭包就带上了栈上的生命周期，无法满足约束。
        // `_path` 前面的下划线表示"故意不用这个参数"，避免未使用变量的警告。
        set_file_observer(move |_path| {
            // `fetch_add` 原子地加一并返回旧值；这里不关心旧值，所以丢弃返回结果。
            counter.fetch_add(1, Ordering::Relaxed);
        });

        // `tempfile::tempdir()` 创建临时目录，`TempDir` 在离开作用域时递归删除它。
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();
        // `b"a"` 是字节字符串字面量，类型是 `&[u8; 1]`，正是 `write` 需要的。
        std::fs::write(src.path().join("a.txt"), b"a").unwrap();
        std::fs::write(src.path().join("b.txt"), b"b").unwrap();

        copy_tree(src.path(), dst.path(), &Options::mirror()).unwrap();
        // 断言失败时把实际值一起打印出来，便于看出回调是否根本没触发。
        // `assert!` 支持格式参数，第二个及之后的参数与 `format!` 用法相同。
        assert!(
            seen.load(Ordering::Relaxed) >= 2,
            "every file should be reported: {}",
            seen.load(Ordering::Relaxed)
        );
    }

    #[test]
    fn copies_directory_contents() {
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();

        // 建一个两层目录，再放一个根目录下的文件，用来验证"按内容复制"。
        std::fs::create_dir_all(src.path().join("a/b")).unwrap();
        std::fs::write(src.path().join("a/b/clip.MP4"), b"payload").unwrap();
        std::fs::write(src.path().join("root.txt"), b"root").unwrap();

        // 返回的计数器用于检查复制结果，`let stats` 后面会逐项断言。
        let stats = copy_tree(src.path(), dst.path(), &Options::mirror()).unwrap();

        // 源目录的内容落在目标根目录下，不会多出一层以源目录命名的目录。
        // `join` 依次拼接路径片段，`read` 读出全部字节用于比较。
        assert_eq!(
            std::fs::read(dst.path().join("a/b/clip.MP4")).unwrap(),
            b"payload"
        );
        assert_eq!(std::fs::read(dst.path().join("root.txt")).unwrap(), b"root");
        // 新建或更新的文件数至少是 2；具体归到哪一类取决于目标是否已存在。
        assert!(stats.files_created + stats.files_updated >= 2);
        // 正常复制不应该有错误记录。
        assert_eq!(stats.errors.len(), 0);
    }

    #[test]
    fn mirror_deletes_files_missing_in_source() {
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();

        // 源里有两个文件；目标里额外多一个 stale.txt，用来验证删除行为。
        std::fs::write(src.path().join("keep1.txt"), b"1").unwrap();
        std::fs::write(src.path().join("keep2.txt"), b"2").unwrap();
        std::fs::write(dst.path().join("keep1.txt"), b"1").unwrap();
        std::fs::write(dst.path().join("keep2.txt"), b"2").unwrap();
        std::fs::write(dst.path().join("stale.txt"), b"old").unwrap();

        let stats = copy_tree(src.path(), dst.path(), &Options::mirror()).unwrap();

        // 源里不存在的旧文件必须被删除，镜像语义才算成立。
        assert!(
            !dst.path().join("stale.txt").exists(),
            "extra files must be deleted"
        );
        // sy 的计数器把删除数单独记一项，正好用于断言。
        assert_eq!(stats.files_deleted, 1);
    }

    #[test]
    fn deletion_threshold_stays_at_upstream_default() {
        // 上游默认 --max-delete 50%：几乎全部文件都该删时会拒绝执行。
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();
        // 源为空、目标有一个文件：删除比例 100%，远超 50% 的阈值。
        std::fs::write(dst.path().join("stale.txt"), b"old").unwrap();

        // `unwrap_err()` 断言结果是错误，并把 `Err` 里的值取出来；
        // 与 `unwrap()` 相同，只是期望的分支相反。
        let err = copy_tree(src.path(), dst.path(), &Options::mirror()).unwrap_err();
        // `format!("{err}")` 调用 `Display` 得到可搜索的文本。
        let message = format!("{err}");
        assert!(
            message.contains("Deletion threshold"),
            "sy's delete guard should stay in place: {message}"
        );
        // 保护生效时目标必须原封不动，一个文件都不能少。
        assert!(
            dst.path().join("stale.txt").exists(),
            "nothing may be deleted when the guard refuses"
        );
    }
}
