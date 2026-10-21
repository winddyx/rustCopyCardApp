//! rcca entry point: preflight and traversal, copy through the embedded sy engine,
//! rcca's own verification, then exit code mapping.

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

/// Chunk size used while hashing.
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
    let stats = Arc::new(if cli.verbose {
        Stats::verbose()
    } else {
        Stats::new()
    });
    let style = ui::Style {
        color: ui::color_enabled(cli.color),
    };
    // The only visible progress: phase, overall percentage, done/total and current file.
    let (mut live, view) = Live::start(Arc::clone(&stats), ui::interactive(cli.quiet), style);

    // ---------- stage 1: arguments and volume preflight ----------
    let source = fs::canonicalize(&cli.source).map_err(|err| {
        Error::preflight_io(format!("source unavailable: {}", cli.source.display()), err)
    })?;
    if !source.is_dir() {
        return Err(Error::preflight(format!(
            "source is not a directory: {}",
            source.display()
        )));
    }
    fs::create_dir_all(&cli.target).map_err(|err| {
        Error::preflight_io(
            format!("cannot create target directory: {}", cli.target.display()),
            err,
        )
    })?;

    let target_root = if cli.keep_source_name {
        let name = source.file_name().ok_or_else(|| {
            Error::usage("source has no directory name, cannot use --keep-source-name")
        })?;
        cli.target.join(name)
    } else {
        cli.target.clone()
    };
    fs::create_dir_all(&target_root)
        .map_err(|err| Error::preflight_io("cannot create the target root", err))?;
    let target_root = fs::canonicalize(&target_root)
        .map_err(|err| Error::preflight_io("cannot resolve the target root", err))?;

    if let Some(problem) = paths::nesting_problem(&source, &target_root) {
        // A bad layout is an argument error (exit code 2), not a runtime preflight failure.
        return Err(Error::usage(problem));
    }
    check_writable(&target_root)?;
    if cli.clean_stale {
        let removed = clean_stale(&target_root);
        if removed > 0 && !cli.quiet {
            eprintln!("rcca: removed {removed} leftover temp files");
        }
    }

    let fs_type = fs_type_name(&target_root);

    // ---------- stage 2: walk the source, check the target ----------
    let mut manifest =
        scan::scan_with_progress(&source, &target_root, &mut |files| view.set_scanned(files))
            .map_err(|err| Error::preflight_io("traversing the source failed", err))?;

    if !manifest.errors.is_empty() {
        return Err(Error::preflight(format!(
            "{} errors while traversing the source, first: {}",
            manifest.errors.len(),
            manifest.errors[0]
        )));
    }
    if !manifest.conflicts.is_empty() {
        return Err(Error::preflight(format!(
            "{} path rule conflicts, first: {}",
            manifest.conflicts.len(),
            manifest.conflicts[0]
        )));
    }

    // sy copies symlinks but rcca cannot hash them, and special files are skipped, so
    // both are called out instead of vanishing from the counts.
    if !cli.quiet && manifest.skipped_symlinks > 0 {
        let (noun, verb) = if manifest.skipped_symlinks == 1 {
            ("symlink is", "was")
        } else {
            ("symlinks are", "were")
        };
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

    let unreadable = scan::probe_readable(&source, &manifest.files);
    if !unreadable.is_empty() {
        if cli.skip_unreadable {
            let bad: std::collections::HashSet<&paths::RelPath> =
                unreadable.iter().map(|(rel, _)| rel).collect();
            manifest.files.retain(|entry| !bad.contains(&entry.rel));
            manifest.total_bytes = manifest.files.iter().map(|e| e.size).sum();
            for (rel, err) in &unreadable {
                stats.files_skipped.fetch_add(1, Ordering::Relaxed);
                stats.note_failure(format!("skipping unreadable file {}: {err}", rel.display()));
            }
            if !cli.quiet {
                eprintln!("rcca: skipped {} unreadable files", unreadable.len());
            }
        } else {
            return Err(Error::preflight(format!(
                "{} source files are unreadable (use --skip-unreadable to skip them), first: {}",
                unreadable.len(),
                unreadable[0].0.display()
            )));
        }
    }

    if let Some(free) = free_space(&target_root) {
        let needed = manifest.total_bytes.saturating_mul(102) / 100 + 64 * 1024 * 1024;
        if free < needed && !cli.force {
            return Err(Error::preflight(format!(
                "not enough space on the target: {} available, {} needed (use --force to continue)",
                human_bytes(free),
                human_bytes(needed)
            )));
        }
    }

    if is_fat_family(fs_type.as_deref()) {
        const FAT32_MAX: u64 = 4 * 1024 * 1024 * 1024 - 1;
        if let Some(big) = manifest.files.iter().find(|e| e.size > FAT32_MAX) {
            return Err(Error::preflight(format!(
                "the target volume is FAT, which caps files at 4 GiB, but the manifest holds {} ({})",
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
            failures: Vec::new(),
            verbose: cli.verbose,
            interrupted: false,
        };
        report::print(&summary);
        return Ok(0);
    }

    // ---------- stage 3: copy through the embedded sy engine ----------
    view.set_total(manifest.total_bytes);
    view.set_phase(Phase::Copy);

    let engine_opts = EngineOptions {
        delete: true,
        jobs: cli.jobs,
        max_errors: cli.fail_fast.then_some(1),
    };
    {
        // sy prints nothing; the file name and the byte count arrive through callbacks.
        let stats_for_name = Arc::clone(&stats);
        engine::set_file_observer(move |path| {
            if let Some(name) = path.file_name() {
                stats_for_name.set_current(name.to_string_lossy().into_owned());
            }
        });
        let stats_for_bytes = Arc::clone(&stats);
        engine::set_bytes_observer(move |bytes, transferred_file| {
            stats_for_bytes
                .copied_bytes
                .fetch_add(bytes, Ordering::Relaxed);
            // Only finished transfers of regular files count, so a failed copy does not
            // show up as a transferred file.
            if transferred_file {
                stats_for_bytes.copied_files.fetch_add(1, Ordering::Relaxed);
            }
        });
    }
    let copy = engine::copy_tree(&source, &target_root, &engine_opts)?;
    for err in &copy.errors {
        stats.note_failure(format!(
            "copying {}: {} ({})",
            err.path.display(),
            err.error,
            err.action
        ));
    }

    // ---------- stage 4: hash every file and compare ----------
    // Ctrl-C is not intercepted during the copy (sy ends on its own); from here rcca owns it.
    install_signal_handler(&stats);

    let verify_opts = verify::Options {
        chunk: VERIFY_CHUNK,
        keep_bad: cli.keep_bad,
        no_cache: cli.verify_no_cache,
        jobs: cli.jobs,
    };
    view.set_phase(Phase::Verify);
    let verify_report = verify::run(&manifest.files, &source, &target_root, &verify_opts, &stats);
    live.finish();

    let interrupted = stats.cancel.load(Ordering::SeqCst);
    if interrupted {
        stats.note_failure("verification was interrupted".to_string());
    }

    // ---------- summary ----------
    // The example list is bounded, the count is not: report the count, list the examples.
    let mut failures = stats.failure_examples();
    let mut failure_count = stats.failure_count();
    failures.extend(verify_report.mismatched.clone());
    failures.extend(
        verify_report
            .missing
            .iter()
            .map(|p| format!("{p}: target file missing")),
    );
    failures.extend(verify_report.errors.clone());
    failure_count += verify_failed(&verify_report);
    if let Some(reason) = stats.abort_reason() {
        failures.push(format!("aborted on request: {reason}"));
        failure_count += 1;
    }

    let summary = Summary {
        source: source.clone(),
        target: target_root.clone(),
        files_total: manifest.files.len(),
        bytes_total: manifest.total_bytes,
        elapsed: started.elapsed(),
        created: copy.files_created,
        updated: copy.files_updated,
        skipped: copy.files_skipped + stats.files_skipped.load(Ordering::Relaxed),
        deleted: copy.files_deleted,
        transferred_files: stats.copied_files.load(Ordering::Relaxed),
        bytes_transferred: copy.bytes_transferred,
        verified: verify_report.matched,
        verify_failed: verify_failed(&verify_report),
        checked_bytes: verify_report.checked_bytes,
        failure_count,
        failures,
        verbose: cli.verbose,
        interrupted,
    };
    report::print(&summary);

    if interrupted {
        return Ok(130);
    }
    if summary.failure_count == 0 {
        Ok(0)
    } else {
        Ok(1)
    }
}

/// Files that did not pass verification.
fn verify_failed(report: &verify::VerifyReport) -> usize {
    report.mismatched.len() + report.missing.len() + report.errors.len()
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
            format!("target directory is not writable: {}", root.display()),
            err,
        )),
    }
}

/// Remove leftover sy temp files from the target volume, returning how many went away.
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
    // Same names the macOS branch returns, so is_fat_family sees one vocabulary.
    const MSDOS_SUPER_MAGIC: i64 = 0x4d44; // FAT12/16/32
    const EXFAT_SUPER_MAGIC: i64 = 0x2011_bab0;
    let c = CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut buf: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statfs(c.as_ptr(), &mut buf) } != 0 {
        return None;
    }
    match buf.f_type as i64 {
        MSDOS_SUPER_MAGIC => Some("msdos".to_string()),
        EXFAT_SUPER_MAGIC => Some("exfat".to_string()),
        other => Some(format!("magic:{other:#x}")),
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
