//! End-to-end flows: plain copy, overwrite, mirror delete, layout, output modes.

use std::fs;
use std::path::{Path, PathBuf};

use assert_cmd::Command;
use tempfile::TempDir;

fn rcca() -> Command {
    Command::cargo_bin("rcca").expect("building the rcca binary")
}

fn write_file(root: &Path, rel: &str, data: &[u8]) {
    let path = root.join(rel);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, data).unwrap();
}

fn collect_files(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        for entry in fs::read_dir(&dir).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            if entry.file_type().unwrap().is_dir() {
                pending.push(path);
            } else {
                out.push(path);
            }
        }
    }
    out.sort();
    out
}

fn build_source() -> TempDir {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    write_file(root, "100MEDIA/camera_A001.MOV", &vec![7u8; 300_000]);
    write_file(root, "100MEDIA/camera_A002.MOV", &vec![9u8; 1234]);
    write_file(root, "sub/照片_0001.JPG", b"cjk-name-payload");
    write_file(root, "empty.bin", b"");
    dir
}

#[test]
fn progress_goes_to_stderr_and_summary_to_stdout() {
    let src = build_source();
    let dst = TempDir::new().unwrap();
    let output = rcca()
        .arg(src.path())
        .arg(dst.path())
        .arg("--color")
        .arg("always")
        .output()
        .unwrap();
    assert!(output.status.success());

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    // Piped stderr has no TTY: no progress line, only warnings if any.
    assert!(
        !stdout.contains('\u{1b}'),
        "stdout must not carry escape sequences: {stdout}"
    );
    assert!(
        !stderr.contains("\u{1b}["),
        "no progress line without a TTY: {stderr}"
    );
    assert!(
        !stderr.contains('\r'),
        "no carriage returns without a TTY: {stderr}"
    );
    assert!(
        !stderr.contains("COPY") && !stderr.contains("VERIFY"),
        "no progress text without a TTY: {stderr}"
    );

    // The result lives on stdout only.
    assert!(
        stdout.contains("✓ All files copied and verified"),
        "summary belongs on stdout: {stdout}"
    );
}

#[test]
fn copies_tree_and_verifies() {
    let src = build_source();
    let dst = TempDir::new().unwrap();
    let output = rcca().arg(src.path()).arg(dst.path()).output().unwrap();
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    for rel in [
        "100MEDIA/camera_A001.MOV",
        "100MEDIA/camera_A002.MOV",
        "sub/照片_0001.JPG",
        "empty.bin",
    ] {
        let a = fs::read(src.path().join(rel)).unwrap();
        let b = fs::read(dst.path().join(rel)).unwrap();
        assert_eq!(a, b, "{rel} content differs");
    }

    let leftovers: Vec<PathBuf> = collect_files(dst.path())
        .into_iter()
        .filter(|p| p.file_name().unwrap().to_string_lossy().starts_with(".sy-"))
        .collect();
    assert!(
        leftovers.is_empty(),
        "no sy temp files may be left behind: {leftovers:?}"
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("4/4 passed"), "output: {stdout}");
    assert!(stdout.contains("Transfer"), "output: {stdout}");
    assert!(
        stdout.contains("All files copied and verified"),
        "output: {stdout}"
    );
}

#[test]
fn transfer_counts_files_not_directories() {
    // sy counts directory entries in its own "created" figure, rcca counts transferred
    // files only, so the two rows differ by the two directories here.
    let src = TempDir::new().unwrap();
    let dst = TempDir::new().unwrap();
    write_file(src.path(), "a/b/one.bin", b"1");
    write_file(src.path(), "a/two.bin", b"22");

    let output = rcca().arg(src.path()).arg(dst.path()).output().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    assert!(output.status.success(), "output: {stdout}");
    assert!(
        stdout.contains("  Copy       4 created"),
        "two directories and two files, as sy counts them: {stdout}"
    );
    assert!(
        stdout.contains("  Transfer   2 files · 3 B"),
        "only the two files are transfers: {stdout}"
    );
}

#[test]
fn a_failed_copy_is_not_counted_as_transferred() {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let src = TempDir::new().unwrap();
        let dst = TempDir::new().unwrap();
        write_file(src.path(), "sub/locked.bin", b"blocked");
        write_file(src.path(), "ok.bin", b"fine");
        let locked = dst.path().join("sub");
        fs::create_dir_all(&locked).unwrap();
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o555)).unwrap();

        let output = rcca().arg(src.path()).arg(dst.path()).output().unwrap();
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();

        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        assert_eq!(output.status.code(), Some(1), "output: {stdout}");
        assert!(
            stdout.contains("  Transfer   1 file · 4 B"),
            "the blocked file must not count as transferred: {stdout}"
        );
    }
}

#[test]
fn overwrites_existing_file() {
    let src = TempDir::new().unwrap();
    let dst = TempDir::new().unwrap();
    write_file(src.path(), "a.bin", b"new-content");
    write_file(dst.path(), "a.bin", b"old-content-that-is-longer");

    rcca().arg(src.path()).arg(dst.path()).assert().success();

    assert_eq!(fs::read(dst.path().join("a.bin")).unwrap(), b"new-content");
}

#[test]
fn mirror_deletes_target_extras() {
    let src = TempDir::new().unwrap();
    let dst = TempDir::new().unwrap();
    write_file(src.path(), "a.bin", b"a");
    write_file(src.path(), "b.bin", b"b");
    write_file(dst.path(), "a.bin", b"a");
    write_file(dst.path(), "b.bin", b"b");
    write_file(dst.path(), "stale.bin", b"old");

    let output = rcca().arg(src.path()).arg(dst.path()).output().unwrap();
    assert!(output.status.success());
    assert!(
        !dst.path().join("stale.bin").exists(),
        "the mirror must delete extra files"
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("1 deleted"), "output: {stdout}");
}

#[test]
fn keep_source_name_changes_layout() {
    let src = build_source();
    let dst = TempDir::new().unwrap();
    rcca()
        .arg(src.path())
        .arg(dst.path())
        .arg("--keep-source-name")
        .assert()
        .success();
    let name = src.path().file_name().unwrap();
    assert!(dst
        .path()
        .join(name)
        .join("100MEDIA/camera_A001.MOV")
        .is_file());
}

#[test]
fn quiet_mode_prints_only_summary() {
    let src = build_source();
    let dst = TempDir::new().unwrap();
    let output = rcca()
        .arg(src.path())
        .arg(dst.path())
        .arg("--quiet")
        .output()
        .unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("4/4 passed"), "output: {stdout}");
    assert!(
        !stdout.contains("\u{1b}["),
        "quiet mode must not emit ANSI codes"
    );
    assert!(
        !stdout.contains("rcca:"),
        "quiet mode must not print notices"
    );
}

#[test]
fn rerun_skips_unchanged_files() {
    let src = build_source();
    let dst = TempDir::new().unwrap();
    rcca().arg(src.path()).arg(dst.path()).assert().success();

    let output = rcca()
        .arg(src.path())
        .arg(dst.path())
        .arg("--quiet")
        .output()
        .unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("0 created"),
        "the second run must not copy again: {stdout}"
    );
    assert!(
        stdout.contains("Transfer   0 files · 0 B"),
        "the second run must not transfer: {stdout}"
    );
}

#[test]
fn preserves_mtime_and_mode() {
    let src = TempDir::new().unwrap();
    let dst = TempDir::new().unwrap();
    write_file(src.path(), "a.bin", b"payload");
    let path = src.path().join("a.bin");

    let stamp = filetime::FileTime::from_unix_time(1_700_000_000, 0);
    filetime::set_file_mtime(&path, stamp).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();
    }

    rcca().arg(src.path()).arg(dst.path()).assert().success();

    let copied = dst.path().join("a.bin");
    let meta = fs::metadata(&copied).unwrap();
    let mtime = filetime::FileTime::from_last_modification_time(&meta);
    assert_eq!(mtime.unix_seconds(), 1_700_000_000);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(meta.permissions().mode() & 0o777, 0o640);
    }
}

#[cfg(unix)]
#[test]
fn symlinks_are_copied_but_not_verified() {
    let src = TempDir::new().unwrap();
    let dst = TempDir::new().unwrap();
    write_file(src.path(), "real.bin", b"data");
    std::os::unix::fs::symlink("real.bin", src.path().join("link.bin")).unwrap();

    let output = rcca().arg(src.path()).arg(dst.path()).output().unwrap();
    assert!(output.status.success());
    let link = dst.path().join("link.bin");
    assert!(
        fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink(),
        "sy copies symlinks as links by default"
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("1/1 passed"),
        "symlinks are not verified: {stdout}"
    );
    // Transfer 只算常规文件：符号链接不属于可校验的传输，计入就会与 Verify 行矛盾。
    assert!(
        stdout.contains("  Transfer   1 file · 4 B"),
        "a symlink is not a transferred file: {stdout}"
    );
}

#[cfg(unix)]
#[test]
fn a_symlink_only_source_is_copied_and_never_verified() {
    // 源里没有常规文件：清单为空，但 sy 仍要复制符号链接。
    let src = TempDir::new().unwrap();
    let dst = TempDir::new().unwrap();
    write_file(src.path(), "real.bin", b"data");
    std::os::unix::fs::symlink("real.bin", src.path().join("link.bin")).unwrap();
    // 目标里的真实文件不存在时链接是悬空的，但仍应作为链接原样复制。
    fs::remove_file(src.path().join("real.bin")).unwrap();

    let output = rcca().arg(src.path()).arg(dst.path()).output().unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "stderr: {stderr}");
    assert!(
        fs::symlink_metadata(dst.path().join("link.bin"))
            .unwrap()
            .file_type()
            .is_symlink(),
        "the symlink must actually be copied: {stdout}"
    );
    assert!(
        stdout.contains("  Verify     0 files"),
        "nothing can be verified without regular files: {stdout}"
    );
    // 提示语必须与实际行为一致：这一条只有在 sy 真的执行过之后才成立。
    assert!(
        stderr.contains("1 symlink is copied but not verified"),
        "the note follows the copy: {stderr}"
    );
}

#[test]
fn an_empty_source_still_runs_the_mirror() {
    // 空源目录意味着目标里的文件都该删除，这超过 50% 的保护阈值，sy 因此拒绝。
    let src = TempDir::new().unwrap();
    let dst = TempDir::new().unwrap();
    write_file(dst.path(), "stale.bin", b"old");

    let output = rcca().arg(src.path()).arg(dst.path()).output().unwrap();
    assert_eq!(
        output.status.code(),
        Some(1),
        "an empty source is not a silent no-op: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        dst.path().join("stale.bin").exists(),
        "the guard leaves the target untouched"
    );
}

#[test]
fn an_empty_source_into_an_empty_target_succeeds() {
    let src = TempDir::new().unwrap();
    let dst = TempDir::new().unwrap();

    let output = rcca().arg(src.path()).arg(dst.path()).output().unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("  Verify     0 files"), "{stdout}");
    assert!(stdout.contains("All files copied and verified"), "{stdout}");
}

#[test]
fn empty_directories_are_created_in_the_target() {
    // 没有常规文件的源也要建立目录结构：这条路径以前会整段跳过。
    let src = TempDir::new().unwrap();
    let dst = TempDir::new().unwrap();
    fs::create_dir_all(src.path().join("100MEDIA/EMPTY")).unwrap();

    rcca().arg(src.path()).arg(dst.path()).assert().success();
    assert!(
        dst.path().join("100MEDIA/EMPTY").is_dir(),
        "directories are part of the mirror"
    );
}

#[test]
fn clean_stale_removes_leftovers() {
    let src = TempDir::new().unwrap();
    let dst = TempDir::new().unwrap();
    write_file(src.path(), "a.bin", b"payload");
    write_file(dst.path(), ".sy-1a2b3c4d.tmp", b"stale");

    rcca()
        .arg(src.path())
        .arg(dst.path())
        .arg("--clean-stale")
        .assert()
        .success();

    assert!(!dst.path().join(".sy-1a2b3c4d.tmp").exists());
}

#[test]
fn rerun_is_idempotent() {
    let src = build_source();
    let dst = TempDir::new().unwrap();
    rcca().arg(src.path()).arg(dst.path()).assert().success();
    rcca().arg(src.path()).arg(dst.path()).assert().success();
    assert_eq!(
        fs::read(dst.path().join("100MEDIA/camera_A001.MOV")).unwrap(),
        fs::read(src.path().join("100MEDIA/camera_A001.MOV")).unwrap()
    );
}

/// 把目标文件改成同样大小、同样 mtime，但内容不同：sy 会认为它没变而跳过，
/// 只有 rcca 的哈希能发现差异。这是"最终文件名意味着已验证"的关键路径。
fn tamper_with_the_same_size_and_mtime(target: &Path, replacement: &[u8]) {
    let mtime = filetime::FileTime::from_last_modification_time(&fs::metadata(target).unwrap());
    fs::write(target, replacement).unwrap();
    filetime::set_file_mtime(target, mtime).unwrap();
}

#[test]
fn a_tampered_target_is_reported_and_removed() {
    let src = TempDir::new().unwrap();
    let dst = TempDir::new().unwrap();
    write_file(src.path(), "a.bin", b"AAAAAAAA");
    rcca().arg(src.path()).arg(dst.path()).assert().success();

    let target = dst.path().join("a.bin");
    tamper_with_the_same_size_and_mtime(&target, b"BBBBBBBB");

    let output = rcca().arg(src.path()).arg(dst.path()).output().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    assert_eq!(output.status.code(), Some(1), "output: {stdout}");
    assert!(
        stdout.contains("0 created"),
        "sy skips an unchanged size and mtime: {stdout}"
    );
    assert!(
        stdout.contains("hash mismatch"),
        "the hash is the only thing that can catch this: {stdout}"
    );
    assert!(
        stdout.contains("0/1 passed"),
        "the tampered file did not pass: {stdout}"
    );
    // 默认删除不一致的目标文件，这样留在磁盘上的最终文件名一定意味着"已验证"。
    assert!(!target.exists(), "a mismatching target file is removed");
}

#[test]
fn keep_bad_quarantines_the_tampered_target() {
    let src = TempDir::new().unwrap();
    let dst = TempDir::new().unwrap();
    write_file(src.path(), "a.bin", b"AAAAAAAA");
    rcca().arg(src.path()).arg(dst.path()).assert().success();

    let target = dst.path().join("a.bin");
    tamper_with_the_same_size_and_mtime(&target, b"BBBBBBBB");

    let output = rcca()
        .arg(src.path())
        .arg(dst.path())
        .arg("--keep-bad")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(
        !target.exists(),
        "the mismatching file must not keep its final name"
    );
    let quarantined = dst.path().join(".a.bin.badhash");
    assert_eq!(
        fs::read(&quarantined).unwrap(),
        b"BBBBBBBB",
        "the rejected content is kept for inspection"
    );
}
