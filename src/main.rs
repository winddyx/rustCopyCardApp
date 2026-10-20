//! rcca 入口：阶段编排、退出码映射、Ctrl-C 处理。

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Instant;

use clap::Parser;

use rcca::cli::Cli;
use rcca::commit;
use rcca::copy::Outcome;
use rcca::error::{Error, Result};
use rcca::paths;
use rcca::pipeline;
use rcca::report::{self, Summary};
use rcca::scan;
use rcca::stats::{human_bytes, human_speed, Stats};
use rcca::ui::{Phase, Ui};
use rcca::verify;

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
    install_signal_handler(&stats);

    let mut ui = Ui::new(cli.quiet, cli.verbose);
    for note in cli.effective_io().2 {
        eprintln!("rcca: 提示：{note}");
    }

    // ---------- 阶段 0：参数与卷预检 ----------
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
        if removed > 0 {
            eprintln!("rcca: 已清理 {removed} 个残留暂存文件");
        }
    }

    if same_device(&source, &target_root) {
        eprintln!(
            "rcca: 提示：源与目标在同一文件系统，读写重叠可能降低吞吐；同盘机械盘建议加 --serial-io 与 --chunk 4M"
        );
    }
    let fs_type = fs_type_name(&target_root);
    if let Some(free) = free_space(&target_root) {
        let _ = free;
    }

    // ---------- 阶段 1：遍历 source，检查 target ----------
    ui.set_phase(Phase::Scan, 0, 0);
    let mut manifest = {
        let mut progress = |count: usize| {
            ui.set_progress(count as u64, 0);
            ui.tick(&stats);
        };
        scan::scan_with_progress(&source, &target_root, &mut progress)
            .map_err(|err| Error::preflight_io("遍历源目录失败", err))?
    };
    ui.set_phase(Phase::Scan, manifest.files.len() as u64, 0);
    ui.set_progress(manifest.files.len() as u64, 0);
    ui.tick_force(&stats);

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

    scan::create_dirs(&target_root, &manifest.dirs)
        .map_err(|err| Error::preflight_io("创建目标目录结构失败", err))?;

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
            eprintln!("rcca: 已跳过 {} 个不可读文件", unreadable.len());
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
            copied: 0,
            copy_failed: 0,
            skipped: 0,
            committed: 0,
            commit_failed: 0,
            verified: 0,
            verify_failed: 0,
            elapsed: started.elapsed(),
            read_bytes: 0,
            written_bytes: 0,
            failures: Vec::new(),
            verbose: cli.verbose,
            interrupted: false,
            committed_unverified: 0,
        };
        report::print(&summary);
        return Ok(0);
    }

    // ---------- 阶段 2：Copy all files（流水线写 tmp） ----------
    let (chunk, buffer_bytes, _) = cli.effective_io();
    // 非交互模式下逐文件即时输出，交互模式由 TUI 呈现。
    let on_file_done = if ui.interactive() {
        None
    } else {
        let names: Vec<String> = manifest
            .files
            .iter()
            .map(|entry| entry.rel.display())
            .collect();
        let verbose = ui.verbose();
        Some(std::sync::Arc::new(move |index: usize, outcome: &Outcome| {
            let rel = names.get(index).cloned().unwrap_or_default();
            match outcome {
                Outcome::Ok(_) => println!("COPY {rel}"),
                Outcome::Failed { reason } => println!("COPY {rel} FAILED {reason}"),
            }
            let _ = verbose;
        })
            as std::sync::Arc<dyn Fn(usize, &Outcome) + Send + Sync>)
    };
    let opts = pipeline::Options {
        chunk,
        buffer_bytes,
        fsync: !cli.no_fsync,
        serial_io: cli.serial_io,
        fail_fast: cli.fail_fast,
        on_file_done,
    };
    ui.set_buffer_bytes((opts.slots() * chunk) as u64);
    ui.set_phase(
        Phase::Copy,
        manifest.files.len() as u64,
        manifest.total_bytes,
    );

    let copy_report = {
        let mut tick = || ui.tick(&stats);
        pipeline::run(
            &manifest.files,
            &source,
            &target_root,
            &opts,
            &stats,
            &mut tick,
        )
    };
    ui.set_buffer_bytes(copy_report.buffer_bytes_effective);
    ui.set_progress(manifest.files.len() as u64, manifest.total_bytes);
    ui.tick_force(&stats);

    let interrupted_after_copy = stats.cancel.load(Ordering::SeqCst);

    // ---------- 阶段 3：再次遍历、校验长度、原子重命名 ----------
    let commit_report = if interrupted_after_copy {
        commit::CommitReport::default()
    } else {
        ui.set_phase(Phase::Commit, manifest.files.len() as u64, 0);
        ui.set_progress(manifest.files.len() as u64, 0);
        let report = rcca::commit::run(
            &manifest.files,
            &copy_report.outcomes,
            &source,
            &target_root,
            !cli.no_fsync,
        );
        ui.tick_force(&stats);
        report
    };

    // ---------- 阶段 4：XXH64 all files ----------
    let committed_bytes: u64 = commit_report
        .committed
        .iter()
        .map(|&index| manifest.files[index].size)
        .sum();
    let verify_report = if interrupted_after_copy {
        verify::VerifyReport::default()
    } else {
        ui.set_phase(
            Phase::Verify,
            commit_report.committed.len() as u64,
            committed_bytes.saturating_mul(2),
        );
        let verify_opts = verify::Options {
            chunk,
            keep_bad: cli.keep_bad,
            no_cache: cli.verify_no_cache,
        };
        let mut tick = |stats: &Stats| ui.tick(stats);
        let report = verify::run(
            &manifest.files,
            &commit_report.committed,
            &source,
            &target_root,
            &verify_opts,
            &stats,
            &mut tick,
        );
        ui.set_progress(
            report.matched as u64 + report.mismatched.len() as u64,
            committed_bytes.saturating_mul(2),
        );
        ui.tick_force(&stats);
        report
    };

    // 校验阶段也可能被 Ctrl-C 打断，结束时再判定一次。
    let interrupted = stats.cancel.load(Ordering::SeqCst);

    let dir_warnings = scan::restore_dir_modes(&target_root, &manifest.dirs);
    for warning in &dir_warnings {
        eprintln!("rcca: 警告：{warning}");
    }

    ui.finish();

    // ---------- 汇总 ----------
    let mut failures: Vec<String> = Vec::new();
    for (index, outcome) in copy_report.outcomes.iter().enumerate() {
        if let Some(Outcome::Failed { reason }) = outcome {
            failures.push(format!(
                "复制 {}：{reason}",
                manifest.files[index].rel.display()
            ));
        }
    }
    for (index, failure) in &commit_report.failures {
        failures.push(format!(
            "提交 {}：{}",
            manifest.files[*index].rel.display(),
            failure.describe()
        ));
    }
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
        copied: stats.files_ok.load(Ordering::Relaxed),
        copy_failed: copy_report
            .outcomes
            .iter()
            .filter(|o| matches!(o, Some(Outcome::Failed { .. })))
            .count(),
        skipped: stats.files_skipped.load(Ordering::Relaxed),
        committed: commit_report.committed_count(),
        commit_failed: commit_report.failure_count(),
        verified: verify_report.matched,
        verify_failed: verify_report.mismatched.len()
            + verify_report.missing.len()
            + verify_report.errors.len(),
        elapsed: started.elapsed(),
        read_bytes: stats.read_bytes.load(Ordering::Relaxed),
        written_bytes: stats.written_bytes.load(Ordering::Relaxed),
        failures,
        verbose: cli.verbose,
        interrupted,
        committed_unverified: commit_report
            .committed_count()
            .saturating_sub(verify_report.matched + verify_report.mismatched.len()),
    };
    report::print(&summary);

    let seconds = summary.elapsed.as_secs_f64().max(0.001);
    if ui.verbose() {
        println!(
            "细节：读速 {}，写速 {}，缓冲 {}，块 {}",
            human_speed(stats.read_bytes.load(Ordering::Relaxed) as f64 / seconds),
            human_speed(stats.written_bytes.load(Ordering::Relaxed) as f64 / seconds),
            human_bytes(copy_report.buffer_bytes_effective),
            human_bytes(copy_report.chunk_effective as u64)
        );
    }

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

/// 清理目标卷上遗留的暂存文件，返回删除数量。
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
