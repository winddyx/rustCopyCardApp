//! rcca 入口：预检与遍历、交给内嵌的 sy 复制、rcca 校验、退出码映射。

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Instant;

use clap::Parser;

use rcca::cli::{Cli, ColorChoice};
use rcca::engine::{self, Options as EngineOptions};
use rcca::error::{Error, Result};
use rcca::paths;
use rcca::report::{self, Summary};
use rcca::scan;
use rcca::stats::{human_bytes, Stats};
use rcca::ui::{CopyLine, CopySlot, Phase, Ui};
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
    let mut ui = Ui::new(cli.quiet, color_enabled(cli.color), ascii_output(cli.ascii));

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

    if !cli.quiet && same_device(&source, &target_root) {
        eprintln!("rcca: 提示：源与目标在同一文件系统，读写互相争用可能降低吞吐");
    }
    let fs_type = fs_type_name(&target_root);

    // ---------- 阶段 2：遍历 source，检查 target ----------
    let mut manifest = scan::scan(&source, &target_root)
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
        ui.finish();
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

    // ---------- 阶段 3：交给内嵌的 sy 引擎复制（输出透传） ----------
    if !cli.quiet {
        eprintln!(
            "rcca: 复制 {} 个文件，共 {}，引擎 sy {}（镜像，删除目标中多余文件）",
            manifest.files.len(),
            human_bytes(manifest.total_bytes),
            engine::VERSION
        );
    }
    let engine_opts = EngineOptions {
        delete: true,
        max_errors: cli.fail_fast.then_some(1),
    };

    // 复制阶段只报当前文件：sy 不打印任何进度，这一行由 rcca 原地刷新。
    let copy_slot = CopySlot::default();
    {
        let slot = copy_slot.clone();
        engine::set_file_observer(move |path| {
            if let Some(name) = path.file_name() {
                slot.set(name.to_string_lossy().into_owned());
            }
        });
    }
    let mut copy_line = CopyLine::start(copy_slot, ui.active(), ui.style());
    let copy = engine::copy_tree(&source, &target_root, &engine_opts)?;
    copy_line.finish();

    if !cli.quiet {
        eprintln!(
            "rcca: 复制完成（sy）：新建 {} 个，更新 {} 个，跳过 {} 个，删除 {} 个，传输 {}",
            copy.files_created,
            copy.files_updated,
            copy.files_skipped,
            copy.files_deleted,
            human_bytes(copy.bytes_transferred)
        );
        eprintln!();
    }
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
    ui.set_copy_totals(manifest.files.len() as u64, manifest.total_bytes);
    ui.set_total_bytes(manifest.total_bytes);
    ui.set_phase(Phase::Verify, manifest.files.len() as u64);

    let indices: Vec<usize> = (0..manifest.files.len()).collect();
    let verify_opts = verify::Options {
        chunk: VERIFY_CHUNK,
        keep_bad: cli.keep_bad,
        no_cache: cli.verify_no_cache,
    };
    let verify_report = {
        let mut tick = |stats: &Stats| ui.tick(stats);
        verify::run(
            &manifest.files,
            &indices,
            &source,
            &target_root,
            &verify_opts,
            &stats,
            &mut tick,
        )
    };
    ui.set_progress((verify_report.matched + verify_report.mismatched.len()) as u64);
    ui.tick_force(&stats);

    let interrupted = stats.cancel.load(Ordering::SeqCst);
    if interrupted {
        stats.note_failure("校验阶段被中断".to_string());
    }
    ui.finish();

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

/// 颜色策略：显式开关优先，其次 NO_COLOR、TERM=dumb 与非 TTY。
fn color_enabled(choice: ColorChoice) -> bool {
    match choice {
        ColorChoice::Always => true,
        ColorChoice::Never => false,
        ColorChoice::Auto => {
            if std::env::var_os("NO_COLOR").is_some() {
                return false;
            }
            if matches!(std::env::var("TERM").as_deref(), Ok("dumb")) {
                return false;
            }
            std::io::IsTerminal::is_terminal(&std::io::stderr())
        }
    }
}

/// 是否只能用 ASCII 字形：显式开关或 TERM=dumb。不猜本地化环境——多数终端是 UTF-8，
/// 但 LANG 常为空，按 locale 判断会误回退。
fn ascii_output(flag: bool) -> bool {
    flag || matches!(std::env::var("TERM").as_deref(), Ok("dumb"))
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

#[cfg(unix)]
fn same_device(a: &Path, b: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    match (fs::metadata(a), fs::metadata(b)) {
        (Ok(ma), Ok(mb)) => ma.dev() == mb.dev(),
        _ => false,
    }
}

#[cfg(not(unix))]
fn same_device(_a: &Path, _b: &Path) -> bool {
    false
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

#[allow(dead_code)]
fn target_display(path: &Path) -> String {
    path.display().to_string()
}

#[allow(dead_code)]
fn join(root: &Path, rel: &Path) -> PathBuf {
    root.join(rel)
}
