//! rcca 的入口：前置检查与遍历，经内嵌 sy 引擎复制，再做 rcca 自己的校验，
//! 最后把结果映射成退出码。

// 标准库：文件操作、路径借用、原子内存序、共享所有权、计时器。
use std::fs;
use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Instant;

// clap 的 `Parser` trait 提供 `Cli::parse()`。
use clap::Parser;

// 库 crate（`lib.rs` 声明的 `rcca`）里的各模块。二进制把它当外部依赖引用，
// 因此路径前缀是 `rcca::` 而不是 `crate::`。
use rcca::cli::Cli;
// `Options as EngineOptions` 起了别名：本文件后面还有一个 `verify::Options`，
// 重名时用别名能避免写全路径。
use rcca::engine::{self, Options as EngineOptions};
use rcca::error::{Error, Result};
use rcca::paths;
use rcca::report::{self, Summary};
use rcca::scan;
use rcca::stats::{human_bytes, Stats};
use rcca::ui::{self, Live, Phase};
use rcca::verify;

/// 计算哈希时使用的分块大小。
///
/// `1024 * 1024` 在编译期就求得结果，不会产生运行时代价。
const VERIFY_CHUNK: usize = 1024 * 1024;

fn main() {
    // `parse()` 在参数非法时会打印帮助并直接结束进程，因此返回类型是 `Cli` 而不是 `Result`。
    let cli = Cli::parse();
    // 失败统一在这里打印并转成退出码，`run` 内部因此只负责返回错误。
    let code = match run(cli) {
        // 成功时 `run` 直接给出退出码（0 或 130 等）。
        Ok(code) => code,
        Err(err) => {
            // 错误信息统一带 `rcca:` 前缀，便于在日志里和其他行区分。
            eprintln!("rcca: {err}");
            // `err` 在上一行被借用打印后，仍然可以在下一行按引用调用方法。
            err.exit_code()
        }
    };
    // 用 `std::process::exit` 而不是从 `main` 返回：它会立刻结束进程，不执行析构函数。
    // 因此所有需要收尾的资源（比如进度行）都必须在 `run` 里显式结束。
    std::process::exit(code);
}

fn run(cli: Cli) -> Result<i32> {
    // 计时从函数入口开始，覆盖扫描、复制、校验三个阶段。
    let started = Instant::now();
    // `Arc<Stats>` 让主流程、进度线程、sy 的复制回调共用同一份统计。
    // `Arc` 是原子引用计数，可以被多个线程同时持有（`Rc` 不能跨线程）。
    let stats = Arc::new(if cli.verbose {
        Stats::verbose()
    } else {
        Stats::new()
    });
    // `--color` 只是策略，这里换算成"是否真的输出颜色"。
    let style = ui::Style {
        color: ui::color_enabled(cli.color),
    };
    // 唯一的可见进度：阶段、整体百分比、已完成/总量和当前文件。
    // 返回两个对象：`live` 负责停止刷新，`view` 供本函数更新数值。
    // 三个参数：共享统计、是否绘制、绘制样式。`live` 需要 `mut` 才能调用 `finish`。
    let (mut live, view) = Live::start(Arc::clone(&stats), ui::interactive(cli.quiet), style);

    // ---------- 阶段 1：参数与卷的前置检查 ----------
    // `canonicalize` 把路径转成绝对路径并解析符号链接与 `.`/`..`，
    // 这样后续的嵌套判断和哈希都基于同一份真实路径。
    // `map_err` 把 `io::Error` 换成带上下文的 `Error::Preflight`，再用 `?` 提前返回。
    let source = fs::canonicalize(&cli.source).map_err(|err| {
        Error::preflight_io(format!("source unavailable: {}", cli.source.display()), err)
    })?;
    // 源必须是目录：文件、设备节点等都不能作为复制来源。
    if !source.is_dir() {
        return Err(Error::preflight(format!(
            "source is not a directory: {}",
            source.display()
        )));
    }
    // 目标目录不存在就创建，让用户不必事先建好。
    fs::create_dir_all(&cli.target).map_err(|err| {
        Error::preflight_io(
            format!("cannot create target directory: {}", cli.target.display()),
            err,
        )
    })?;

    // `--keep-source-name` 时多一层以源目录名命名的目录，否则目标根就是用户给的目标目录。
    let target_root = if cli.keep_source_name {
        // `file_name()` 返回 `Option`：像 `/` 这样的路径没有最后一段名字。
        // `ok_or_else` 把 `None` 变成 `Err`，闭包只在需要时报错信息才执行。
        let name = source.file_name().ok_or_else(|| {
            Error::usage("source has no directory name, cannot use --keep-source-name")
        })?;
        cli.target.join(name)
    } else {
        // 默认按内容复制，目标根就是用户给的目标目录。
        cli.target.clone()
    };
    // 先建目录，再做规范化：`canonicalize` 要求路径存在。
    fs::create_dir_all(&target_root)
        .map_err(|err| Error::preflight_io("cannot create the target root", err))?;
    // 目标也规范一次，否则同一个目录的两种写法会让嵌套判断失效。
    let target_root = fs::canonicalize(&target_root)
        .map_err(|err| Error::preflight_io("cannot resolve the target root", err))?;

    // 源与目标不能相同、不能互相嵌套，否则复制会递归或互相污染。
    if let Some(problem) = paths::nesting_problem(&source, &target_root) {
        // 布局错误属于参数问题（退出码 2），不是运行期的前置检查失败。
        return Err(Error::usage(problem));
    }
    // 真正写一个探测文件来确认可写，而不是只看权限位。
    check_writable(&target_root)?;
    if cli.clean_stale {
        // 清理上次中断留下的临时文件；返回删除数量，仅用于提示。
        let removed = clean_stale(&target_root);
        // `--quiet` 时连这类提示也不打印。
        if removed > 0 && !cli.quiet {
            eprintln!("rcca: removed {removed} leftover temp files");
        }
    }

    // 取目标卷的文件系统类型：FAT 系列有 4 GiB 单文件上限，需要提前拦截。
    let fs_type = fs_type_name(&target_root);

    // ---------- 阶段 2：遍历源目录，检查目标 ----------
    // 传入闭包把扫描进度接到进度行上；`|files|` 借用外层的 `view`。
    // `&mut` 是因为回调参数的类型是 `&mut dyn FnMut(usize)`，闭包要能被反复调用。
    let mut manifest =
        scan::scan_with_progress(&source, &target_root, &mut |files| view.set_scanned(files))
            .map_err(|err| Error::preflight_io("traversing the source failed", err))?;

    // 遍历本身出错（目录读不到等）：报出总数与第一条原因。
    if !manifest.errors.is_empty() {
        return Err(Error::preflight(format!(
            "{} errors while traversing the source, first: {}",
            manifest.errors.len(),
            // 索引取第一个元素；前面的 `is_empty` 判断保证不会越界。
            manifest.errors[0]
        )));
    }
    // 路径规则冲突（过长、非法名、与临时文件规则撞车）同样挡住。
    if !manifest.conflicts.is_empty() {
        return Err(Error::preflight(format!(
            "{} path rule conflicts, first: {}",
            manifest.conflicts.len(),
            manifest.conflicts[0]
        )));
    }

    // sy 会复制符号链接但 rcca 无法对其计算哈希，特殊文件则完全跳过，
    // 两者都要明确提示，不能从计数里悄悄消失。
    if !cli.quiet && manifest.skipped_symlinks > 0 {
        // 单复数分开处理，让提示读起来自然。
        let (noun, verb) = if manifest.skipped_symlinks == 1 {
            ("symlink is", "was")
        } else {
            ("symlinks are", "were")
        };
        // 元组解构后按位置插入消息，避免为两种情况各写一条 `eprintln!`。
        eprintln!(
            "rcca: {} {noun} copied but not verified ({verb} not checked)",
            manifest.skipped_symlinks
        );
    }
    if !cli.quiet && manifest.skipped_special > 0 {
        let noun = if manifest.skipped_special == 1 {
            "special file (fifo, socket, device) is"
        } else {
            "special files (fifos, sockets, devices) are"
        };
        eprintln!("rcca: {} {noun} not copied", manifest.skipped_special);
    }

    // 逐个打开源文件确认可读；结果只包含失败项，因此列表为空即全部可读。
    let unreadable = scan::probe_readable(&source, &manifest.files);
    if !unreadable.is_empty() {
        if cli.skip_unreadable {
            // 用 `HashSet<&RelPath>` 做集合查表：元素是引用，因此不复制路径本身，
            // 其生命周期受 `unreadable` 的约束，正好覆盖下面 `retain` 的范围。
            let bad: std::collections::HashSet<&paths::RelPath> =
                unreadable.iter().map(|(rel, _)| rel).collect();
            // `retain` 原地删除不满足条件的元素，闭包拿到的是 `&Entry`。
            manifest.files.retain(|entry| !bad.contains(&entry.rel));
            // 清单变了，总字节数必须重算，否则进度分母会偏大。
            manifest.total_bytes = manifest.files.iter().map(|e| e.size).sum();
            // 被跳过的文件也要计入统计与失败样例，让汇总里能看到它们。
            for (rel, err) in &unreadable {
                stats.files_skipped.fetch_add(1, Ordering::Relaxed);
                stats.note_failure(format!("skipping unreadable file {}: {err}", rel.display()));
            }
            if !cli.quiet {
                eprintln!("rcca: skipped {} unreadable files", unreadable.len());
            }
        } else {
            // 默认策略：源有读不了的文件就整体拒绝，避免复制出残缺的目标。
            return Err(Error::preflight(format!(
                "{} source files are unreadable (use --skip-unreadable to skip them), first: {}",
                unreadable.len(),
                // `(RelPath, String)` 元组用 `.0` 取路径部分。
                unreadable[0].0.display()
            )));
        }
    }

    // 剩余空间检查：`saturating_mul` 在上溢时停在最大值，不会 panic。
    if let Some(free) = free_space(&target_root) {
        // 需要 102%（给临时文件和原地替换留余量）再加 64 MiB 余量。
        let needed = manifest.total_bytes.saturating_mul(102) / 100 + 64 * 1024 * 1024;
        // `--force` 可以跳过这个检查，用于用户确知空间足够的情形。
        if free < needed && !cli.force {
            return Err(Error::preflight(format!(
                "not enough space on the target: {} available, {} needed (use --force to continue)",
                human_bytes(free),
                human_bytes(needed)
            )));
        }
    }

    // FAT 系列文件系统单文件上限 4 GiB，超过就提前拒绝，避免复制到一半失败。
    if is_fat_family(fs_type.as_deref()) {
        // `4 * 1024 * 1024 * 1024 - 1`：4 GiB 减一字节，是能写入的最大值。
        const FAT32_MAX: u64 = 4 * 1024 * 1024 * 1024 - 1;
        // `find` 找到第一个超限文件就停，返回 `Option`。
        if let Some(big) = manifest.files.iter().find(|e| e.size > FAT32_MAX) {
            return Err(Error::preflight(format!(
                "the target volume is FAT, which caps files at 4 GiB, but the manifest holds {} ({})",
                human_bytes(big.size),
                big.rel.display()
            )));
        }
    }

    // 空清单不必启动复制和校验，直接给出一份零值汇总。
    if manifest.files.is_empty() {
        // 先停掉进度行，避免它与汇总输出抢 stderr。
        live.finish();
        // 字段逐一写 0：这份结构体没有实现 `Default`，必须把 18 个字段写全。
        let summary = Summary {
            // 路径需要拥有所有权，这里克隆一份（`source` 后面还要用）。
            source: source.clone(),
            target: target_root.clone(),
            files_total: 0,
            bytes_total: 0,
            // `elapsed()` 返回从 `started` 到现在经过的时长。
            elapsed: started.elapsed(),
            created: 0,
            updated: 0,
            skipped: 0,
            deleted: 0,
            transferred_files: 0,
            bytes_transferred: 0,
            verified: 0,
            verify_failed: 0,
            checked_bytes: 0,
            failure_count: 0,
            // 空的失败列表用 `Vec::new()`，不需要类型标注。
            failures: Vec::new(),
            // 这两个字段决定打印行为，直接沿用命令行选择。
            verbose: cli.verbose,
            interrupted: false,
        };
        // 汇总写到 stdout，然后按成功退出。
        report::print(&summary);
        return Ok(0);
    }

    // ---------- 阶段 3：经内嵌 sy 引擎复制 ----------
    // 总数拿到后才能计算百分比，因此在这一步才告诉进度行。
    view.set_total(manifest.total_bytes);
    view.set_phase(Phase::Copy);

    let engine_opts = EngineOptions {
        // 镜像语义固定开启，命令行不提供关闭的开关。
        delete: true,
        jobs: cli.jobs,
        // `then_some(1)`：条件成立得到 `Some(1)`，否则是 `None`。
        // 这样 --fail-fast 就直接变成"最多允许一个错误"。
        max_errors: cli.fail_fast.then_some(1),
    };
    {
        // 这层花括号让回调注册代码自成一段，也让临时变量尽快离开作用域。
        // sy 自己不打印任何东西：文件名和字节数都通过回调送达。
        let stats_for_name = Arc::clone(&stats);
        // `move` 把克隆出来的 `Arc` 移进闭包；闭包必须满足 `'static`，
        // 所以不能直接借用外层的 `stats`。
        engine::set_file_observer(move |path| {
            // 没有文件名的路径（例如根目录）就不更新显示。
            if let Some(name) = path.file_name() {
                // 文件名可能不是 UTF-8，`to_string_lossy()` 做有损转换，
                // `into_owned()` 把 `Cow` 转成独立的 `String`。
                stats_for_name.set_current(name.to_string_lossy().into_owned());
            }
        });
        // 第二个回调要自己的一份 `Arc`，前一个已经被移动进上面的闭包。
        let stats_for_bytes = Arc::clone(&stats);
        engine::set_bytes_observer(move |bytes, transferred_file| {
            // 每完成一个任务就累加它计入的字节数。
            stats_for_bytes
                .copied_bytes
                .fetch_add(bytes, Ordering::Relaxed);
            // 只统计已完成的普通文件传输，失败的任务不计入"已传输文件数"。
            if transferred_file {
                stats_for_bytes.copied_files.fetch_add(1, Ordering::Relaxed);
            }
        });
    }
    // 复制失败会作为 `Err` 直接返回（退出码 1）；单个文件的错误则收集在 `copy.errors` 里。
    let copy = engine::copy_tree(&source, &target_root, &engine_opts)?;
    // sy 的逐文件错误逐条并入失败列表，`note_failure` 负责计数与截断。
    for err in &copy.errors {
        stats.note_failure(format!(
            "copying {}: {} ({})",
            err.path.display(),
            err.error,
            err.action
        ));
    }

    // ---------- 阶段 4：逐个文件计算哈希并比较 ----------
    // 复制期间不拦截 Ctrl-C（由 sy 自行结束）；从这一步开始由 rcca 负责。
    install_signal_handler(&stats);

    let verify_opts = verify::Options {
        // 每次读取 1 MiB，兼顾系统调用次数与内存占用。
        chunk: VERIFY_CHUNK,
        // 校验失败时是删除还是隔离，由命令行决定。
        keep_bad: cli.keep_bad,
        // 是否绕过页缓存，影响"从设备真读"的严格程度。
        no_cache: cli.verify_no_cache,
        // 与复制相同的并发度：`-j` 同时作用于两个阶段。
        jobs: cli.jobs,
    };
    // 进度进入后半程（校验占另外一半）。
    view.set_phase(Phase::Verify);
    // 这里同时借用清单、两个根目录、选项与统计；返回一份报告，不抛错误。
    let verify_report = verify::run(&manifest.files, &source, &target_root, &verify_opts, &stats);
    // 校验结束后立刻停掉进度行，让随后的汇总独占输出。
    live.finish();

    // 中断标志会被校验线程写入，这里用 `SeqCst` 读取，确保看到的是最终值。
    let interrupted = stats.cancel.load(Ordering::SeqCst);
    if interrupted {
        // 中断本身也算一个问题，因此计入失败列表。
        stats.note_failure("verification was interrupted".to_string());
    }

    // ---------- 汇总 ----------
    // 样例列表有上限，计数没有上限：因此计数用 `failure_count`，列表只作为样例。
    let mut failures = stats.failure_examples();
    let mut failure_count = stats.failure_count();
    // 校验阶段的三类问题也要并进同一份列表。
    // `clone()` 是因为 `verify_report` 后面还要用到（取 `matched` 等字段）。
    failures.extend(verify_report.mismatched.clone());
    failures.extend(
        verify_report
            .missing
            .iter()
            // `map` 把每个缺失路径转成一句话，`extend` 直接把这些项追加进 `Vec`。
            .map(|p| format!("{p}: target file missing")),
    );
    failures.extend(verify_report.errors.clone());
    // 计数同样要加上校验失败数，才能与列表口径一致。
    failure_count += verify_failed(&verify_report);
    // 主动中止时补一条说明，并把计数加一。
    if let Some(reason) = stats.abort_reason() {
        failures.push(format!("aborted on request: {reason}"));
        failure_count += 1;
    }

    // 汇总结构体在这里一次性组装好，随后交给 `report::print` 排版。
    let summary = Summary {
        source: source.clone(),
        target: target_root.clone(),
        // 清单里的文件数与总字节数即"应复制"的口径。
        files_total: manifest.files.len(),
        bytes_total: manifest.total_bytes,
        // 到此为止的总耗时。
        elapsed: started.elapsed(),
        // 下面四个计数来自 sy，包含目录条目。
        created: copy.files_created,
        updated: copy.files_updated,
        // 被跳过的文件来自两处：复制阶段的决定，以及 preflight 跳过的不可读文件。
        skipped: copy.files_skipped + stats.files_skipped.load(Ordering::Relaxed),
        deleted: copy.files_deleted,
        // 实际搬运的文件数与字节数来自 rcca 自己的回调统计。
        transferred_files: stats.copied_files.load(Ordering::Relaxed),
        bytes_transferred: copy.bytes_transferred,
        // 校验结果：通过数、失败数、检查过的字节数。
        verified: verify_report.matched,
        verify_failed: verify_failed(&verify_report),
        checked_bytes: verify_report.checked_bytes,
        failure_count,
        failures,
        verbose: cli.verbose,
        interrupted,
    };
    // 汇总写到 stdout；stderr 上此时已经没有进度行。
    report::print(&summary);

    // 中断优先：即使同时有失败，也返回 130 表明结果不完整。
    if interrupted {
        return Ok(130);
    }
    // 有失败返回 1，全部通过返回 0；这里返回退出码而不是结束进程，
    // 由 `main` 统一调用 `std::process::exit`。
    if summary.failure_count == 0 {
        Ok(0)
    } else {
        Ok(1)
    }
}

/// 统计未通过校验的文件数。
fn verify_failed(report: &verify::VerifyReport) -> usize {
    // 三类问题之和：摘要不一致、目标缺失、读取或检查出错。
    report.mismatched.len() + report.missing.len() + report.errors.len()
}

/// 安装 Ctrl-C 处理函数，只把取消标志置位，让校验循环自行收尾。
///
/// `ctrlc::set_handler` 要求闭包 `'static` 且可在其他线程执行，
/// 因此同样用 `move` 把 `Arc` 的克隆送入闭包。返回值忽略：
/// 重复安装会失败，但这里没有更好的补救措施。
fn install_signal_handler(stats: &Arc<Stats>) {
    // 克隆一份 `Arc` 交给闭包，原引用留给调用方。
    let stats = Arc::clone(stats);
    let _ = ctrlc::set_handler(move || {
        // `SeqCst` 保证校验线程一定能看到这个置位。
        stats.cancel.store(true, Ordering::SeqCst);
    });
}

/// 写入一个探测文件来确认目标目录可写。
///
/// 之所以真的写一次而不是检查权限位：ACL、只读挂载、磁盘配额等问题
/// 只有实际写入才会暴露。探测文件名带上进程号，避免并发运行时互相干扰。
fn check_writable(root: &Path) -> Result<()> {
    // `std::process::id()` 返回当前进程号，拼进文件名即可做到互不干扰。
    let probe = root.join(format!(".rcca-probe-{}", std::process::id()));
    // `b"probe"` 是字节字面量，`fs::write` 接受任意 `AsRef<[u8]>`。
    match fs::write(&probe, b"probe") {
        Ok(()) => {
            // 用完删除；即使删除失败也不影响可写性的判断，所以忽略结果。
            let _ = fs::remove_file(&probe);
            Ok(())
        }
        // 写入失败说明目标不可写，把上下文和原始错误一起返回（退出码 3）。
        Err(err) => Err(Error::preflight_io(
            format!("target directory is not writable: {}", root.display()),
            err,
        )),
    }
}

/// 删除目标卷上遗留的 sy 临时文件，返回删除的数量。
fn clean_stale(root: &Path) -> usize {
    // 计数器：只有确实删除成功的文件才加一。
    let mut removed = 0usize;
    // 同样是显式栈的深度优先遍历：`pending` 里放待处理的目录。
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        // `let ... else` 读目录失败就跳过这个目录，继续处理栈里的其他目录。
        let Ok(read) = fs::read_dir(&dir) else {
            continue;
        };
        // `flatten()` 丢弃迭代器里 `Err` 的项，只保留成功的条目。
        for item in read.flatten() {
            let path = item.path();
            // 用 `symlink_metadata` 不跟随链接：链接本身按文件处理，不当作目录展开。
            let Ok(meta) = fs::symlink_metadata(&path) else {
                continue;
            };
            if meta.is_dir() {
                // 目录入栈稍后展开。
                pending.push(path);
            } else if paths::looks_like_tmp(&item.file_name()) && fs::remove_file(&path).is_ok() {
                // 条件里直接做删除：只有名字像临时文件且删除成功才计数。
                removed += 1;
            }
        }
    }
    // 返回删除总数，供调用方决定是否打印提示。
    removed
}

/// 取目标卷的文件系统类型名。
///
/// 三个平台分支靠 `#[cfg]` 在编译期择一保留，函数签名保持一致，
/// 因此调用点（`fs_type_name(&target_root)`）无需关心平台。
#[cfg(target_os = "macos")]
fn fs_type_name(path: &Path) -> Option<String> {
    // C 字符串与把 `OsStr` 转成字节的扩展 trait。
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    // 调用 C 接口需要以空字节结尾的字符串；路径里含空字节时 `CString::new` 返回 `Err`，
    // 用 `ok()?` 直接放弃检查（`?` 在返回 `Option` 的函数里遇到 `None` 就提前返回）。
    let c = CString::new(path.as_os_str().as_bytes()).ok()?;
    // 这里的 `buf` 是 C 结构体，先全部置零再交给 C 填充。`unsafe` 是必须的：
    // 零初始化后立即由 C 写入，且字段类型都是整数数组，没有无效值风险。
    let mut buf: libc::statfs = unsafe { std::mem::zeroed() };
    // C 函数返回 0 表示成功，非 0 表示失败（通常路径不可访问）。
    if unsafe { libc::statfs(c.as_ptr(), &mut buf) } != 0 {
        return None;
    }
    // `from_ptr` 从 C 字符串构造 `&CStr`，其有效期依赖 `buf` 仍然存活。
    let name = unsafe { std::ffi::CStr::from_ptr(buf.f_fstypename.as_ptr()) };
    // 转成 `String` 后 `buf` 的生命周期就不再牵连返回值。
    Some(name.to_string_lossy().into_owned())
}

// Linux 分支：`statfs` 返回的是魔数而不是名字，需要自己映射。
#[cfg(target_os = "linux")]
fn fs_type_name(path: &Path) -> Option<String> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    // 这里返回的名字与 macOS 分支保持同一套词汇，`is_fat_family` 才能统一判断。
    const MSDOS_SUPER_MAGIC: i64 = 0x4d44; // FAT12/16/32
    const EXFAT_SUPER_MAGIC: i64 = 0x2011_bab0;
    let c = CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut buf: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statfs(c.as_ptr(), &mut buf) } != 0 {
        return None;
    }
    // Linux 用魔数标识文件系统类型；未知类型返回 `magic:0x...` 便于排查。
    // `{other:#x}` 是带 `0x` 前缀的十六进制输出。
    match buf.f_type as i64 {
        MSDOS_SUPER_MAGIC => Some("msdos".to_string()),
        EXFAT_SUPER_MAGIC => Some("exfat".to_string()),
        other => Some(format!("magic:{other:#x}")),
    }
}

/// 其他平台没有实现，返回 `None` 表示"未知"，后续检查因此会被跳过。
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn fs_type_name(_path: &Path) -> Option<String> {
    // 参数名带下划线前缀，明确表示未使用，避免编译警告。
    None
}

/// 判断是否属于 FAT 族文件系统，这类文件系统有 4 GiB 单文件上限。
///
/// `matches!` 把模式匹配写成表达式，等价于 `match ... { ... => true, _ => false }`。
fn is_fat_family(name: Option<&str>) -> bool {
    // `Some("msdos") | Some("vfat") | Some("fat")` 是 or 模式，命中任意一个即为真。
    matches!(name, Some("msdos") | Some("vfat") | Some("fat"))
}

/// 查询目标路径所在卷的可用字节数。
///
/// `#[cfg(unix)]` 写在函数体内部，两种实现都在同一函数里，避免为不同平台写两个签名相同的函数。
fn free_space(path: &Path) -> Option<u64> {
    #[cfg(unix)]
    {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;
        let c = CString::new(path.as_os_str().as_bytes()).ok()?;
        let mut buf: libc::statvfs = unsafe { std::mem::zeroed() };
        // `statvfs` 与 `statfs` 类似，但提供的是文件系统统计而不是类型信息。
        if unsafe { libc::statvfs(c.as_ptr(), &mut buf) } != 0 {
            return None;
        }
        // `f_bavail` 是当前用户可用的块数，`f_frsize` 是块大小；
        // 两者相乘得到可用字节，`as u64` 把 C 的整数类型统一成无符号 64 位。
        Some(buf.f_bavail as u64 * buf.f_frsize as u64)
    }
    #[cfg(not(unix))]
    {
        // 非 Unix 平台直接报告"未知"，前置检查会跳过容量判断。
        // `let _ = path;` 显式声明"参数未使用"，避免未使用变量的警告。
        let _ = path;
        None
    }
}
