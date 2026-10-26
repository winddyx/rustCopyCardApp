//! 校验阶段：对每个文件分别哈希源与目标，比较 XXH64 摘要。
//!
//! 这个循环沿用复制时相同的 `-j` 参数：`jobs > 1` 时同时校验多个文件，
//! 这样源端读取和目标端读取可以重叠。结果按文件逐个收集，最后按清单顺序输出，
//! 因此输出不取决于哪个工作线程先完成任务。

// 这里用线程而不是异步任务，是因为工作量集中在阻塞式文件读取上：
// 每个线程各自持有一块缓冲区顺序读文件，逻辑简单且不依赖运行时调度。

// `fs`/`File` 用于元数据与打开文件；`io` 提供 `Result` 与 `ErrorKind`，`Read` 是读取 trait。
use std::fs::{self, File};
use std::io::{self, Read};
// 路径只用借用形式，不需要拥有所有权。
use std::path::Path;
// 原子计数器用 `Relaxed` 内存序即可。
use std::sync::atomic::Ordering;
// 结果槽位需要互斥访问，因此引入 `Mutex`。
use std::sync::Mutex;

// 流式 XXH64 哈希器，可以分块 `update` 后 `digest`。
use xxhash_rust::xxh64::Xxh64;

// 失败文件的隔离路径规则、清单条目类型、共享统计。
use crate::paths::bad_rel_path;
use crate::scan::Entry;
use crate::stats::Stats;

/// 校验结果汇总。
///
/// 三个 `Vec<String>` 分别对应"摘要不一致"、"目标缺失"和"读取或检查出错"，
/// 之所以存成字符串而不是枚举，是因为它们最终会直接拼进报告的失败列表。
#[derive(Debug, Default)]
pub struct VerifyReport {
    // 通过校验的文件数。
    pub matched: usize,
    /// 所有被检查过的文件字节数，与结果无关。
    pub checked_bytes: u64,
    // 摘要不一致的文件，每项是"路径: 原因"。
    pub mismatched: Vec<String>,
    // 目标文件缺失的文件。
    pub missing: Vec<String>,
    // 读取或检查过程中出错的文件。
    pub errors: Vec<String>,
}

/// 校验选项，由命令行参数映射而来。
#[derive(Debug, Clone)]
pub struct Options {
    // 每次读取的块大小，也决定每个工作线程的缓冲区大小。
    pub chunk: usize,
    // 是否把坏文件改名隔离而不是删除。
    pub keep_bad: bool,
    // 是否绕过页缓存读取（`--verify-no-cache`）。
    pub no_cache: bool,
    /// 同时校验的文件数，1 表示串行。
    pub jobs: usize,
}

/// 计算文件的 XXH64（种子为 0），不上报进度。
///
/// 这里临时分配了一个缓冲区交给 `hash_stream`；需要复用缓冲区以提升性能的调用方
/// （下面的并发循环）直接调用 `hash_stream`。
pub fn hash_file(path: &Path, chunk: usize, no_cache: bool) -> io::Result<u64> {
    // `vec![0u8; chunk]` 按块大小分配零初始化缓冲区，`u8` 保证每个字节都被定义。
    let mut buffer = vec![0u8; chunk];
    // 不需要进度的场合传一个空回调，避免把两种用法写成两份实现。
    hash_stream(path, &mut buffer, no_cache, &mut |_| {})
}

/// 分块读取文件并计算哈希，每读完一块回调一次。
///
/// 参数设计值得注意：缓冲区由调用方提供（`&mut [u8]`），因此同一块内存能在多个文件之间
/// 复用，不必每个文件重新分配。回调写成 `&mut dyn FnMut(u64)`，用动态分发换取
/// 更少的单态化代码；`u64` 是本次读到的字节数。
pub fn hash_stream(
    // 要读取的文件路径。
    path: &Path,
    // 复用的缓冲区，长度决定每次最多读多少字节。
    buffer: &mut [u8],
    // 是否绕过页缓存。
    no_cache: bool,
    // 每块读取后的回调，参数是本次读到的字节数。
    on_chunk: &mut dyn FnMut(u64),
) -> io::Result<u64> {
    // `File::open` 失败会作为 `Err` 由 `?` 直接向上返回，错误类型都是 `io::Error`。
    let file = File::open(path)?;
    if no_cache {
        // 平台相关：尽力让读取穿透页缓存。
        disable_read_cache(&file);
    }
    // 种子为 0，与 `hash_file` 以及外部工具的口径保持一致。
    let mut hasher = Xxh64::new(0);
    // 把文件移动进局部变量，后面用它的 `Read` 实现。
    let mut reader = file;
    loop {
        // `read` 尽量填满缓冲区，返回实际读到的字节数；不保证一次读完。
        let n = match reader.read(buffer) {
            // 返回 0 表示到达文件末尾，跳出循环。
            Ok(0) => break,
            Ok(n) => n,
            // `Interrupted` 表示读到一半被信号打断，重试即可，不算错误。
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            // 其他错误原样向上抛。
            Err(err) => return Err(err),
        };
        // `&buffer[..n]` 只取实际读到的部分；`Read::read` 不保证填满整个缓冲区。
        hasher.update(&buffer[..n]);
        // `n as u64` 显式转换，回调参数统一用 `u64`。
        on_chunk(n as u64);
    }
    // 取出最终摘要，作为函数返回值。
    Ok(hasher.digest())
}

/// 在不支持该功能的平台上保留同名函数，函数体为空。
///
/// 三个同名函数靠 `#[cfg]` 在编译期只保留一个，调用点不必区分平台。
#[cfg(target_os = "macos")]
fn disable_read_cache(file: &File) {
    // `AsRawFd` 提供 `as_raw_fd()`，把 `File` 变成原始文件描述符。
    use std::os::fd::AsRawFd;
    // F_NOCACHE = 48：绕过统一缓冲区缓存，让哈希结果来自设备本身。
    //
    // 这里必须写 `unsafe`：调用 C 函数属于编译器无法验证的契约。
    // 安全性来自参数本身有效（文件描述符由 `File` 持有），返回值也只是提示，因此忽略。
    unsafe {
        libc::fcntl(file.as_raw_fd(), libc::F_NOCACHE, 1);
    }
}

// Linux 上能力更弱的等价实现：只能"建议"内核丢弃已缓存的数据。
#[cfg(target_os = "linux")]
fn disable_read_cache(file: &File) {
    use std::os::fd::AsRawFd;
    // POSIX_FADV_DONTNEED 提示内核这段数据用完即可丢弃，效果类似绕过页缓存。
    unsafe {
        libc::posix_fadvise(file.as_raw_fd(), 0, 0, libc::POSIX_FADV_DONTNEED);
    }
}

// 其他平台没有可用的手段，保留空实现让调用点无需 `#[cfg]`。
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn disable_read_cache(_file: &File) {}

/// 本平台是否真的有办法让哈希绕过页缓存。
///
/// `disable_read_cache` 在没有实现的平台上是个空函数，`--verify-no-cache` 因此不会
/// 改变任何结果（Windows 上的哈希仍然来自系统缓存）。调用方据此打印提示，而不是
/// 让这个开关看起来生效了。
pub fn no_cache_supported() -> bool {
    // `cfg!` 在编译期展开成 `true`/`false`，不会被优化成一个运行时分支。
    cfg!(any(target_os = "macos", target_os = "linux"))
}

/// 单个文件的校验结果，先存起来，等全部结束后再按顺序组装报告。
///
/// 用枚举而不是元组，是为了让"匹配/缺失/不一致/出错"这四种情况在类型层面互斥。
enum Outcome {
    // 两侧摘要一致。
    Matched,
    // 目标文件不存在，载荷是相对路径。
    Missing(String),
    // 摘要不一致，载荷是解释文本。
    Mismatch(String),
    // 读取或检查出错，载荷是解释文本。
    Failed(String),
}

/// 按清单逐项校验目标文件，`jobs > 1` 时并发执行。
pub fn run(
    // 待校验的文件清单，顺序决定最终报告顺序。
    files: &[Entry],
    // 源根目录。
    source: &Path,
    // 目标根目录。
    target: &Path,
    // 校验选项。
    opts: &Options,
    // 共享统计，用于进度行与取消标志。
    stats: &Stats,
) -> VerifyReport {
    // 线程数不超过文件数，避免起一堆空转线程；`max(1)` 保证清单为空时也至少是 1。
    let jobs = opts.jobs.max(1).min(files.len().max(1));
    // 结果按清单下标存放，`None` 表示该位置还没被处理。用 `Mutex` 保护整个向量，
    // 因为多个线程会写不同位置，凭"下标不同"推理安全性对编译器来说不够。
    // 每个槽位是"结果 + 该文件是否真的被读过"的字节数，后者用于 `checked_bytes`。
    let outcomes: Mutex<Vec<Option<(Outcome, u64)>>> =
        Mutex::new((0..files.len()).map(|_| None).collect());
    // 共享的任务游标：每个线程用 `fetch_add` 原子地领取下一个下标，天然实现负载均衡。
    let cursor = std::sync::atomic::AtomicUsize::new(0);

    // `thread::scope` 是标准库的作用域线程：线程借用 `files`、`source` 等局部数据，
    // 作用域结束前会 join 全部线程，因此这些引用不需要 `'static` 或 `Arc`。
    std::thread::scope(|scope| {
        // `_` 表示循环变量本身用不到，只需要重复 `jobs` 次。
        for _ in 0..jobs {
            // 闭包借用外层变量（`cursor`、`outcomes`、`files` 等），
            // 作用域线程允许这种借用，只要线程不活过作用域。
            scope.spawn(|| {
                // 每个工作线程保留自己的读缓冲区，避免加锁共享。
                let mut buffer = vec![0u8; opts.chunk];
                loop {
                    // `fetch_add` 返回增加前的值，因此每个下标只会被一个线程拿到。
                    let pos = cursor.fetch_add(1, Ordering::Relaxed);
                    // let-else 语法：`files.get` 返回 `Option`，取不到就跳出循环。
                    let Some(entry) = files.get(pos) else {
                        break;
                    };
                    // 用户中断时尽快收工；已经处理过的结果仍然会汇报。
                    if stats.stop_requested() {
                        break;
                    }
                    // 真正处理一个文件；缓冲区按引用传入，避免每轮重新分配。
                    let outcome = verify_one(entry, source, target, opts, stats, &mut buffer);
                    // 结果写回自己占用的下标；锁中毒时丢弃这次结果。
                    if let Ok(mut slots) = outcomes.lock() {
                        slots[pos] = Some(outcome);
                    }
                }
            });
        }
        // 作用域结束时自动 join，`run` 因此不会提前返回。
    });

    // 汇总成对外报告。
    let mut report = VerifyReport::default();
    // `into_inner` 消费 `Mutex` 取出向量。`thread::scope` 会把工作线程的 panic 重新抛出，
    // 因此走到这里时锁不会中毒；`Err` 分支只是防备，取回内部数据继续汇总，不丢结果。
    let collected = outcomes
        .into_inner()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    for slot in collected.into_iter().flatten() {
        // 槽位里带着"该文件是否真的被读过"的字节数：只有读过才计入 `checked_bytes`。
        let (outcome, checked_bytes) = slot;
        report.checked_bytes += checked_bytes;
        // 四种结果分别累加到不同字段，报告因此能分开说明问题性质。
        match outcome {
            Outcome::Matched => report.matched += 1,
            Outcome::Missing(message) => report.missing.push(message),
            Outcome::Mismatch(message) => report.mismatched.push(message),
            Outcome::Failed(message) => report.errors.push(message),
        }
    }
    // 返回值即最终报告。
    report
}

/// 校验单个文件，返回结果以及"该文件是否真的被读过"的字节数。
///
/// 第二个返回值用于汇总里的 `checked_bytes`：目标缺失、源文件在扫描后变动、
/// 目标无法检查等情况下一个字节都没读，不能计入"已检查字节"。
/// 所有失败都以 `Outcome` 返回，不向外抛错误，
/// 这样一次运行可以汇报全部问题而不是在第一个问题上中断。
fn verify_one(
    // 清单条目：相对路径、预期大小与修改时间。
    entry: &Entry,
    source: &Path,
    target: &Path,
    opts: &Options,
    stats: &Stats,
    // 复用的读缓冲区。
    buffer: &mut [u8],
) -> (Outcome, u64) {
    // 相对路径的显示形式，用于拼错误信息。
    let rel = entry.rel.display();
    // 进度行只显示文件名，不显示完整路径。
    stats.set_current(entry.rel.file_name().to_string_lossy().into_owned());
    // 由相对路径还原出源端与目标端的绝对路径。
    let src = source.join(entry.rel.as_path());
    let dst = target.join(entry.rel.as_path());

    // 目标必须是常规文件，且必须在打开之前判掉：`metadata` 会跟随符号链接，
    // 于是"目标是指向源的链接"会把内容读成与源一致，把没复制报成已验证；
    // 目标若是管道，`File::open` 会永久阻塞。这两种情况都不能进哈希环节。
    match fs::symlink_metadata(&dst) {
        // 常规文件：继续后面的校验。
        Ok(meta) if meta.is_file() => {}
        // 其余类型（链接、目录、管道、设备）都不算复制成功。
        Ok(meta) => {
            return (
                Outcome::Failed(format!(
                    "{rel}: target is not a regular file ({})",
                    describe_type(&meta)
                )),
                0,
            )
        }
        // 目标不存在是另一种问题，单独归类。
        Err(err) if err.kind() == io::ErrorKind::NotFound => return (Outcome::Missing(rel), 0),
        // 其余读取失败（权限等）不能被误报成"复制没有发生"。
        Err(err) => {
            return (
                Outcome::Failed(format!("{rel}: cannot inspect target: {err}")),
                0,
            )
        }
    }

    // 源侧同样只接受常规文件：清单只收常规文件，扫描后被换成链接或管道就不该再读。
    if let Ok(meta) = fs::symlink_metadata(&src) {
        if !meta.is_file() {
            return (
                Outcome::Failed(format!(
                    "{rel}: source is not a regular file ({})",
                    describe_type(&meta)
                )),
                0,
            );
        }
    }

    // 源文件在扫描之后又变动过，就没有可比较的基准；此时按哈希结果行事可能
    // 删掉唯一一份好数据，所以先拒绝处理。
    if let Some(issue) = stability_issue(source, entry) {
        return (Outcome::Failed(format!("{rel}: {}", issue.describe())), 0);
    }

    // 读源并哈希，读到的字节数会累加到统计里。
    let src_hash = hash_counted(&src, buffer, opts, stats);
    // 源读成功说明这个文件确实参与了校验：计入"已检查字节"（两侧都读，只算一次大小）。
    let checked_bytes = if src_hash.is_ok() { entry.size } else { 0 };
    // 读完源之后、读目标之前再核对一次，覆盖"哈希期间源被改写"的情况。
    if let Some(issue) = stability_issue(source, entry) {
        return (
            Outcome::Failed(format!("{rel}: {}", issue.describe())),
            checked_bytes,
        );
    }
    // 再读目标并哈希。
    let dst_hash = hash_counted(&dst, buffer, opts, stats);

    // 元组匹配加上守卫条件，把"两侧都成功且相等"和其他情况一次列全。
    // `(Err(err), _) | (_, Err(err))` 是 or 模式：两种写法共用同一个分支体，
    // 并且两边绑定的变量名与类型必须一致。
    let outcome = match (src_hash, dst_hash) {
        // 守卫 `if a == b` 在同一分支内再加一个条件，不成立时会继续尝试后面的分支。
        (Ok(a), Ok(b)) if a == b => Outcome::Matched,
        (Ok(a), Ok(b)) => {
            // `{a:016X}` 表示按十六进制大写输出并补足 16 位，方便人工比对。
            let mut message = format!("{rel}: hash mismatch (source {a:016X}, target {b:016X})");
            if opts.keep_bad {
                // 保留坏文件：改名隔离而不是删除。
                let bad = target.join(bad_rel_path(entry.rel.as_path()));
                match fs::rename(&dst, &bad) {
                    // `push_str` 在消息后追加说明，不额外分配新字符串。
                    Ok(()) => message.push_str(", quarantined as .badhash"),
                    Err(err) => {
                        message.push_str(&format!(", quarantine failed: {err}"));
                    }
                }
            } else {
                // 默认行为是删除：最终留在目标里的文件名意味着"已校验通过"。
                match fs::remove_file(&dst) {
                    Ok(()) => message.push_str(", target file deleted"),
                    Err(err) => message.push_str(&format!(", delete failed: {err}")),
                }
            }
            Outcome::Mismatch(message)
        }
        // 任一侧读取失败都归为 `Failed`；`_` 表示另一侧的结果无关紧要。
        (Err(err), _) | (_, Err(err)) => Outcome::Failed(format!("{rel}: read failed: {err}")),
    };

    // 结果与"是否真的读过"一起返回，调用方据此累计 `checked_bytes`。
    (outcome, checked_bytes)
}

/// 把单个条目与复制前记录的清单比较。
///
/// `std::slice::from_ref(entry)` 把一个 `&Entry` 变成仅含一个元素的切片，
/// 从而复用 `check_stability`，不必为单条记录再写一份实现。
fn stability_issue(source: &Path, entry: &Entry) -> Option<crate::scan::SourceIssue> {
    // 返回值是 `Vec<Option<SourceIssue>>`，三步取出第一项：转迭代器、取首个、摊平 `Option`。
    crate::scan::check_stability(source, std::slice::from_ref(entry))
        .into_iter()
        .next()
        .flatten()
}

/// 描述一个条目的类型，用于"不是常规文件"这类错误信息。
fn describe_type(meta: &fs::Metadata) -> &'static str {
    let kind = meta.file_type();
    if kind.is_symlink() {
        "symlink"
    } else if kind.is_dir() {
        "directory"
    } else {
        "special file"
    }
}

/// 计算哈希，并把读到的字节数累加到进度行读取的计数器上。
///
/// 先取出 `&stats.hashed_bytes`，再在闭包里使用，避免闭包捕获整个 `stats`；
/// 闭包是 `FnMut`，因此可以用 `fetch_add` 修改计数。
fn hash_counted(path: &Path, buffer: &mut [u8], opts: &Options, stats: &Stats) -> io::Result<u64> {
    // 借出字段的引用，闭包只捕获这一小段。
    let hashed = &stats.hashed_bytes;
    // 闭包按 `&mut dyn FnMut(u64)` 传入，`fetch_add` 原子累加读取字节数。
    hash_stream(path, buffer, opts.no_cache, &mut |n| {
        hashed.fetch_add(n, Ordering::Relaxed);
    })
}

// 测试模块只在 `cargo test` 时编译。
#[cfg(test)]
mod tests {
    use super::*;

    /// 测试用的默认选项：小块大小、串行、不保留坏文件、不绕过缓存。
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
        // 空文件：`b""` 是长度为 0 的字节切片。
        fs::write(&path, b"").unwrap();
        // XXH64 对空输入的已知结果，用下划线分组的十六进制字面量便于阅读。
        assert_eq!(
            hash_file(&path, 4096, false).unwrap(),
            0xEF46_DB37_51D8_E999
        );
    }

    #[test]
    fn hash_is_chunk_size_independent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("data");
        // 用取模构造一段可重复的伪随机数据，保证测试不依赖外部文件。
        // `0..100_000u32` 指定迭代元素的类型，`as u8` 截断成字节。
        let payload: Vec<u8> = (0..100_000u32).map(|i| (i % 253) as u8).collect();
        fs::write(&path, &payload).unwrap();
        // 极小的块、带余数的块和 1 MiB 的块，结果必须一致。
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
        // 源与目标内容只差一个字节，摘要必然不同。
        fs::write(src.path().join("f.bin"), b"abc").unwrap();
        fs::write(dst.path().join("f.bin"), b"abd").unwrap();
        // 用真实扫描生成清单，避免手工构造 `Entry`。
        let manifest = crate::scan::scan(src.path(), dst.path()).unwrap();
        let stats = Stats::new();
        let report = run(
            &manifest.files,
            src.path(),
            dst.path(),
            &base_options(),
            &stats,
        );
        assert_eq!(report.matched, 0);
        assert_eq!(report.mismatched.len(), 1);
        // 默认策略：校验失败的目标文件被删除。
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
        // 结构体更新语法 `..base_options()` 表示其余字段沿用另一份实例的值。
        let report = run(
            &manifest.files,
            src.path(),
            dst.path(),
            &Options {
                keep_bad: true,
                ..base_options()
            },
            &stats,
        );
        assert_eq!(report.mismatched.len(), 1);
        // 隔离文件名是在原名前加点、后加 .badhash。
        assert!(dst.path().join(".f.bin.badhash").exists());
    }

    /// 构造一对源/目标目录：`good` 个内容一致的文件，`bad` 个内容不一致的。
    fn fixture(good: usize, bad: usize) -> (tempfile::TempDir, tempfile::TempDir) {
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();
        // `{i:02}` 把序号补成两位，保证文件名排序与生成顺序一致。
        for i in 0..good {
            fs::write(src.path().join(format!("g{i:02}.bin")), b"same").unwrap();
            fs::write(dst.path().join(format!("g{i:02}.bin")), b"same").unwrap();
        }
        for i in 0..bad {
            fs::write(src.path().join(format!("b{i:02}.bin")), b"left").unwrap();
            fs::write(dst.path().join(format!("b{i:02}.bin")), b"right").unwrap();
        }
        // 返回元组，配套的临时目录会一起被移动出去，生命周期由调用方持有。
        (src, dst)
    }

    #[test]
    fn parallel_verification_matches_the_serial_result() {
        let (src, dst) = fixture(12, 3);

        // 先用串行选项跑一遍。
        let manifest = crate::scan::scan(src.path(), dst.path()).unwrap();
        let serial = run(
            &manifest.files,
            src.path(),
            dst.path(),
            &base_options(),
            &Stats::new(),
        );

        // 串行那一次已经删掉了不一致的目标文件，所以重新造一份数据给并发版本用。
        let (src2, dst2) = fixture(12, 3);
        let manifest2 = crate::scan::scan(src2.path(), dst2.path()).unwrap();
        let parallel = run(
            &manifest2.files,
            src2.path(),
            dst2.path(),
            &Options {
                jobs: 4,
                ..base_options()
            },
            &Stats::new(),
        );

        // 两种并发度的结果必须完全一致。
        assert_eq!(serial.matched, parallel.matched);
        assert_eq!(serial.mismatched.len(), parallel.mismatched.len());
        assert_eq!(serial.missing.len(), parallel.missing.len());
        assert_eq!(serial.errors.len(), parallel.errors.len());
        // 同时确认这份数据本身的预期结果，避免"两边都错成一样"。
        assert_eq!(parallel.matched, 12);
        assert_eq!(parallel.mismatched.len(), 3);
    }

    #[test]
    fn mismatches_are_reported_in_manifest_order() {
        let (src, dst) = fixture(4, 6);
        let manifest = crate::scan::scan(src.path(), dst.path()).unwrap();
        // 用 8 个线程制造乱序完成的条件。
        let report = run(
            &manifest.files,
            src.path(),
            dst.path(),
            &Options {
                jobs: 8,
                ..base_options()
            },
            &Stats::new(),
        );
        // 每条消息形如 "路径: 原因"，这里只取冒号前的路径部分。
        let names: Vec<&str> = report
            .mismatched
            .iter()
            .map(|m| m.split(':').next().unwrap())
            .collect();
        // 复制一份排序后比较：相等说明原顺序本来就是升序。
        let mut sorted = names.clone();
        sorted.sort();
        assert_eq!(
            names, sorted,
            "output order must not depend on worker order"
        );
    }

    #[test]
    fn checked_bytes_cover_every_file_that_was_looked_at() {
        let (src, dst) = fixture(3, 1);
        let manifest = crate::scan::scan(src.path(), dst.path()).unwrap();
        let report = run(
            &manifest.files,
            src.path(),
            dst.path(),
            &base_options(),
            &Stats::new(),
        );
        assert_eq!(report.matched, 3);
        assert_eq!(report.mismatched.len(), 1);
        // 四个文件各 4 字节：失败的那一个也被读过，同样计入。
        assert_eq!(report.checked_bytes, 16);
    }

    #[test]
    fn a_missing_target_adds_no_checked_bytes() {
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();
        fs::write(src.path().join("f.bin"), b"abc").unwrap();
        let manifest = crate::scan::scan(src.path(), dst.path()).unwrap();
        let report = run(
            &manifest.files,
            src.path(),
            dst.path(),
            &base_options(),
            &Stats::new(),
        );
        assert_eq!(report.missing.len(), 1);
        // 目标不存在时一个字节都没读，"已检查字节"必须为 0。
        assert_eq!(report.checked_bytes, 0);
    }

    #[test]
    fn an_uninspectable_target_is_an_error_not_a_missing_file() {
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();
        fs::write(src.path().join("f.bin"), b"abc").unwrap();
        // 目标位置上放一个目录：元数据能读到，但按文件读取会失败。
        fs::create_dir(dst.path().join("f.bin")).unwrap();
        let manifest = crate::scan::scan(src.path(), dst.path()).unwrap();
        let report = run(
            &manifest.files,
            src.path(),
            dst.path(),
            &base_options(),
            &Stats::new(),
        );
        // 应当归入 errors 而不是 missing。
        assert!(report.missing.is_empty(), "{:?}", report.missing);
        assert_eq!(report.errors.len(), 1, "{:?}", report.errors);
    }

    #[test]
    fn a_symlinked_target_is_not_verified() {
        #[cfg(unix)]
        {
            let src = tempfile::tempdir().unwrap();
            let dst = tempfile::tempdir().unwrap();
            let source_file = src.path().join("f.bin");
            fs::write(&source_file, b"payload").unwrap();
            // 目标位置上放一个指回源的符号链接：跟过去哈希必然相同，
            // 但那不是"已经复制到目标卷"的证据。
            std::os::unix::fs::symlink(&source_file, dst.path().join("f.bin")).unwrap();

            let manifest = crate::scan::scan(src.path(), dst.path()).unwrap();
            let report = run(
                &manifest.files,
                src.path(),
                dst.path(),
                &base_options(),
                &Stats::new(),
            );
            assert_eq!(report.matched, 0, "{:?}", report.mismatched);
            assert_eq!(report.errors.len(), 1, "{:?}", report.errors);
            assert!(
                report.errors[0].contains("not a regular file"),
                "{:?}",
                report.errors
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_fifo_target_is_reported_instead_of_blocking() {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;

        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();
        // 0 字节的源文件与 0 字节的管道节点大小相同，sy 会跳过它，
        // 于是打开目标做哈希时会永久阻塞——类型检查必须在这之前拦下。
        fs::write(src.path().join("f.bin"), b"").unwrap();
        let fifo = dst.path().join("f.bin");
        let path = CString::new(fifo.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o644) }, 0);

        let manifest = crate::scan::scan(src.path(), dst.path()).unwrap();
        let report = run(
            &manifest.files,
            src.path(),
            dst.path(),
            &base_options(),
            &Stats::new(),
        );
        assert_eq!(report.matched, 0);
        assert_eq!(report.errors.len(), 1, "{:?}", report.errors);
        assert!(
            report.errors[0].contains("not a regular file"),
            "{:?}",
            report.errors
        );
    }

    #[test]
    fn jobs_greater_than_file_count_is_fine() {
        let (src, dst) = fixture(3, 0);
        let manifest = crate::scan::scan(src.path(), dst.path()).unwrap();
        let report = run(
            &manifest.files,
            src.path(),
            dst.path(),
            &Options {
                jobs: 16,
                ..base_options()
            },
            &Stats::new(),
        );
        // 线程数被压到文件数，多余的空线程不会被创建。
        assert_eq!(report.matched, 3);
    }
}
