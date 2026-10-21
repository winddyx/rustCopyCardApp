//! rcca 入口：预检与遍历、交给内嵌的 sy 复制、rcca 校验、退出码映射。

use std::fs;
use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Instant;

use clap::Parser;

use rcca::cli::Cli;
use rcca::engine::{self, Options as EngineOptions};
use rcca::error::{Error, Result};
use rcca::paths;
use rcca::report::{self, Summary};
use rcca::scan;
use rcca::stats::{human_bytes, Stats};
use rcca::ui::{self, Live, Phase};
use rcca::verify;

/// 校验阶段每次读取的块大小。
const VERIFY_CHUNK: usize = 1024 * 1024;

fn main() {
    let cli = Cli::parse();
    let code = match run(cli) {
        Ok(code) => code,
        Err(err) => {
            eprintln!("rcca: {err}");
            err.exit_code()
        }
    };
    std::process::exit(code);
}

fn run(cli: Cli) -> Result<i32> {
    let started = Instant::now();
    let stats = Arc::new(Stats::new());
    let style = ui::Style {
        color: ui::color_enabled(cli.color),
    };
    // 运行期只有这一行文字进度：阶段、全程百分比、已完成/总量、当前文件。
    let (mut live, view) = Live::start(Arc::clone(&stats), ui::interactive(cli.quiet), style);

    // ---------- 阶段 1：参数与卷预检 ----------
    let source = fs::canonicalize(&cli.source).map_err(|err| {
        Error::preflight_io(format!("源目录不可用：{}", cli.source.display()), err)
    })?;
    if !source.is_dir() {
        return Err(Error::preflight(format!(
            "源不是目录：{}",
            source.display()
        )));
    }
    fs::create_dir_all(&cli.target).map_err(|err| {
        Error::preflight_io(format!("无法创建目标目录：{}", cli.target.display()), err)
    })?;

    let target_root = if cli.keep_source_name {
        let name = source
            .file_name()
            .ok_or_else(|| Error::usage("源目录没有目录名，无法使用 --keep-source-name"))?;
        cli.target.join(name)
    } else {
        cli.target.clone()
    };
    fs::create_dir_all(&target_root)
        .map_err(|err| Error::preflight_io("无法创建目标根目录", err))?;
    let target_root = fs::canonicalize(&target_root)
        .map_err(|err| Error::preflight_io("无法解析目标根目录", err))?;

    if let Some(problem) = paths::nesting_problem(&source, &target_root) {
        // 路径布局错误属于参数错误（退出码 2），不是运行期预检失败。
        return Err(Error::usage(problem));
    }
    check_writable(&target_root)?;
    if cli.clean_stale {
        let removed = clean_stale(&target_root);
        if removed > 0 && !cli.quiet {
            eprintln!("rcca: 已清理 {removed} 个残留暂存文件");
        }
    }

    let fs_type = fs_type_name(&target_root);

    // ---------- 阶段 2：遍历 source，检查 target ----------
    let mut manifest =
        scan::scan_with_progress(&source, &target_root, &mut |files| view.set_scanned(files))
            .map_err(|err| Error::preflight_io("遍历源目录失败", err))?;

    if !manifest.errors.is_empty() {
        return Err(Error::preflight(format!(
            "遍历源目录出现 {} 个错误，首条：{}",
            manifest.errors.len(),
            manifest.errors[0]
        )));
    }
    if !manifest.conflicts.is_empty() {
        return Err(Error::preflight(format!(
            "路径规则冲突 {} 条，首条：{}",
            manifest.conflicts.len(),
            manifest.conflicts[0]
        )));
    }

    let unreadable = scan::probe_readable(&source, &manifest.files);
    if !unreadable.is_empty() {
        if cli.skip_unreadable {
            let bad: Vec<String> = unreadable.iter().map(|(rel, _)| rel.display()).collect();
            manifest
                .files
                .retain(|entry| !bad.contains(&entry.rel.display()));
            manifest.total_bytes = manifest.files.iter().map(|e| e.size).sum();
            for (rel, err) in &unreadable {
                stats.files_skipped.fetch_add(1, Ordering::Relaxed);
                stats.note_failure(format!("跳过不可读文件 {}：{err}", rel.display()));
            }
            if !cli.quiet {
                eprintln!("rcca: 已跳过 {} 个不可读文件", unreadable.len());
            }
        } else {
            return Err(Error::preflight(format!(
                "{} 个源文件不可读（可用 --skip-unreadable 跳过），首条：{}",
                unreadable.len(),
                unreadable[0].0.display()
            )));
        }
    }

    if let Some(free) = free_space(&target_root) {
        let needed = manifest.total_bytes.saturating_mul(102) / 100 + 64 * 1024 * 1024;
        if free < needed && !cli.force {
            return Err(Error::preflight(format!(
                "目标空间不足：可用 {}，需要 {}（可用 --force 跳过检查）",
                human_bytes(free),
                human_bytes(needed)
            )));
        }
    }

    if is_fat_family(fs_type.as_deref()) {
        const FAT32_MAX: u64 = 4 * 1024 * 1024 * 1024 - 1;
        if let Some(big) = manifest.files.iter().find(|e| e.size > FAT32_MAX) {
            return Err(Error::preflight(format!(
                "目标卷为 FAT 系列文件系统，单文件上限 4 GiB，清单中存在 {}（{}）",
                human_bytes(big.size),
                big.rel.display()
            )));
        }
    }

    if manifest.files.is_empty() {
        live.finish();
        let summary = Summary {
            source: source.clone(),
            target: target_root.clone(),
            files_total: 0,
            bytes_total: 0,
            created: 0,
            updated: 0,
            skipped: 0,
            deleted: 0,
            bytes_transferred: 0,
            copy_errors: 0,
            verified: 0,
            verify_failed: 0,
            elapsed: started.elapsed(),
            failures: Vec::new(),
            verbose: cli.verbose,
            interrupted: false,
        };
        report::print(&summary);
        return Ok(0);
    }

    // ---------- 阶段 3：交给内嵌的 sy 引擎复制 ----------
    view.set_total(manifest.total_bytes);
    view.set_phase(Phase::Copy);

    let engine_opts = EngineOptions {
        delete: true,
        max_errors: cli.fail_fast.then_some(1),
    };
    {
        // sy 不打印任何进度；文件名与字节数通过两个回调交给界面。
        let stats_for_name = Arc::clone(&stats);
        engine::set_file_observer(move |path| {
            if let Some(name) = path.file_name() {
                stats_for_name.set_current(name.to_string_lossy().into_owned());
            }
        });
        let stats_for_bytes = Arc::clone(&stats);
        engine::set_bytes_observer(move |bytes| {
            stats_for_bytes
                .copied_bytes
                .fetch_add(bytes, Ordering::Relaxed);
        });
    }
    let copy = engine::copy_tree(&source, &target_root, &engine_opts)?;
    for err in &copy.errors {
        stats.note_failure(format!(
            "复制 {}：{}（{}）",
            err.path.display(),
            err.error,
            err.action
        ));
    }

    // ---------- 阶段 4：rcca 遍历并逐文件 XXH64 校验 ----------
    // 复制阶段不拦截 Ctrl-C（交给内嵌 sy 自行结束），校验阶段才接管。
    install_signal_handler(&stats);

    let indices: Vec<usize> = (0..manifest.files.len()).collect();
    let verify_opts = verify::Options {
        chunk: VERIFY_CHUNK,
        keep_bad: cli.keep_bad,
        no_cache: cli.verify_no_cache,
    };
    view.set_phase(Phase::Verify);
    let verify_report = verify::run(
        &manifest.files,
        &indices,
        &source,
        &target_root,
        &verify_opts,
        &stats,
    );
    live.finish();

    let interrupted = stats.cancel.load(Ordering::SeqCst);
    if interrupted {
        stats.note_failure("校验阶段被中断".to_string());
    }

    // ---------- 汇总 ----------
    let mut failures = stats.failure_examples();
    failures.extend(verify_report.mismatched.clone());
    failures.extend(
        verify_report
            .missing
            .iter()
            .map(|p| format!("{p}：目标文件缺失")),
    );
    failures.extend(verify_report.errors.clone());
    if let Some(reason) = stats.abort_reason() {
        failures.push(format!("主动中止：{reason}"));
    }

    let summary = Summary {
        source: source.clone(),
        target: target_root.clone(),
        files_total: manifest.files.len(),
        bytes_total: manifest.total_bytes,
        created: copy.files_created,
        updated: copy.files_updated,
        skipped: copy.files_skipped + stats.files_skipped.load(Ordering::Relaxed),
        deleted: copy.files_deleted,
        bytes_transferred: copy.bytes_transferred,
        copy_errors: copy.errors.len(),
        verified: verify_report.matched,
        verify_failed: verify_report.mismatched.len()
            + verify_report.missing.len()
            + verify_report.errors.len(),
        elapsed: started.elapsed(),
        failures,
        verbose: cli.verbose,
        interrupted,
    };
    report::print(&summary);

    if interrupted {
        return Ok(130);
    }
    if summary.failures.is_empty() {
        Ok(0)
    } else {
        Ok(1)
    }
}

fn install_signal_handler(stats: &Arc<Stats>) {
    let stats = Arc::clone(stats);
    let _ = ctrlc::set_handler(move || {
        stats.cancel.store(true, Ordering::SeqCst);
    });
}

fn check_writable(root: &Path) -> Result<()> {
    let probe = root.join(format!(".rcca-probe-{}", std::process::id()));
    match fs::write(&probe, b"probe") {
        Ok(()) => {
            let _ = fs::remove_file(&probe);
            Ok(())
        }
        Err(err) => Err(Error::preflight_io(
            format!("目标目录不可写：{}", root.display()),
            err,
        )),
    }
}

/// 清理目标卷上遗留的 sy 暂存文件，返回删除数量。
fn clean_stale(root: &Path) -> usize {
    let mut removed = 0usize;
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        let Ok(read) = fs::read_dir(&dir) else {
            continue;
        };
        for item in read.flatten() {
            let path = item.path();
            let Ok(meta) = fs::symlink_metadata(&path) else {
                continue;
            };
            if meta.is_dir() {
                pending.push(path);
            } else if paths::looks_like_tmp(&item.file_name()) && fs::remove_file(&path).is_ok() {
                removed += 1;
            }
        }
    }
    removed
}

#[cfg(target_os = "macos")]
fn fs_type_name(path: &Path) -> Option<String> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let c = CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut buf: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statfs(c.as_ptr(), &mut buf) } != 0 {
        return None;
    }
    let name = unsafe { std::ffi::CStr::from_ptr(buf.f_fstypename.as_ptr()) };
    Some(name.to_string_lossy().into_owned())
}

#[cfg(target_os = "linux")]
fn fs_type_name(path: &Path) -> Option<String> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    const MSDOS_SUPER_MAGIC: i64 = 0x4d44;
    let c = CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut buf: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statfs(c.as_ptr(), &mut buf) } != 0 {
        return None;
    }
    if buf.f_type as i64 == MSDOS_SUPER_MAGIC {
        Some("msdos".to_string())
    } else {
        Some(format!("magic:{:#x}", buf.f_type))
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn fs_type_name(_path: &Path) -> Option<String> {
    None
}

fn is_fat_family(name: Option<&str>) -> bool {
    matches!(name, Some("msdos") | Some("vfat") | Some("fat"))
}

fn free_space(path: &Path) -> Option<u64> {
    #[cfg(unix)]
    {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;
        let c = CString::new(path.as_os_str().as_bytes()).ok()?;
        let mut buf: libc::statvfs = unsafe { std::mem::zeroed() };
        if unsafe { libc::statvfs(c.as_ptr(), &mut buf) } != 0 {
            return None;
        }
        Some(buf.f_bavail as u64 * buf.f_frsize as u64)
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        None
    }
}
