//! Failure paths: preflight failures, copy conflicts, interrupts, per-file errors.

use std::fs;
use std::path::Path;
// 这两个 import 只服务于 Unix 上的 SIGINT 测试；不加 `#[cfg]` 会让其他目标上出现
// "unused import" 警告，`clippy -D warnings` 因此在 Windows 上失败。
#[cfg(unix)]
use std::process::{Command as StdCommand, Stdio};
#[cfg(unix)]
use std::time::Duration;

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

#[test]
fn target_inside_source_is_usage_error() {
    let src = TempDir::new().unwrap();
    write_file(src.path(), "a.bin", b"x");
    let nested = src.path().join("out");

    rcca().arg(src.path()).arg(&nested).assert().code(2);
    // 被拒绝的命令不能在源介质上留下痕迹：目标目录要到检查通过之后才创建。
    assert!(
        !nested.exists(),
        "a rejected target must not be created inside the source"
    );
}

#[test]
fn a_parent_component_in_the_target_cannot_smuggle_it_into_the_source() {
    // `missing/../card/out` 里的 `..` 必须先消解，否则嵌套判断与真实的创建位置对不上。
    let parent = TempDir::new().unwrap();
    let src = parent.path().join("card");
    fs::create_dir_all(&src).unwrap();
    write_file(&src, "a.bin", b"x");
    let target = parent.path().join("missing/../card/out");

    rcca().arg(&src).arg(&target).assert().code(2);
    assert!(
        !src.join("out").exists(),
        "the rejected target must not appear inside the source"
    );
    assert!(
        !parent.path().join("missing").exists(),
        "not even the intermediate directory may be created"
    );
}

#[test]
fn source_equals_target_is_usage_error() {
    let src = TempDir::new().unwrap();
    write_file(src.path(), "a.bin", b"x");

    rcca().arg(src.path()).arg(src.path()).assert().code(2);
}

#[test]
fn missing_source_is_preflight_error() {
    let dst = TempDir::new().unwrap();
    rcca()
        .arg("/nonexistent-card-path-xyz")
        .arg(dst.path())
        .assert()
        .code(3);
}

#[cfg(unix)]
#[test]
fn unreadable_source_fails_preflight() {
    use std::os::unix::fs::PermissionsExt;
    let src = TempDir::new().unwrap();
    let dst = TempDir::new().unwrap();
    write_file(src.path(), "secret.bin", b"data");
    let path = src.path().join("secret.bin");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).unwrap();

    let output = rcca().arg(src.path()).arg(dst.path()).output().unwrap();
    // Reading still succeeds as root; the run then completes normally.
    if output.status.code() != Some(0) {
        assert_eq!(output.status.code(), Some(3));
    }

    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
}

#[cfg(unix)]
#[test]
fn skip_unreadable_copies_the_rest() {
    use std::os::unix::fs::PermissionsExt;
    let src = TempDir::new().unwrap();
    let dst = TempDir::new().unwrap();
    write_file(src.path(), "ok.bin", b"good");
    write_file(src.path(), "secret.bin", b"data");
    let path = src.path().join("secret.bin");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).unwrap();

    let output = rcca()
        .arg(src.path())
        .arg(dst.path())
        .arg("--skip-unreadable")
        .output()
        .unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();

    if output.status.code() != Some(0) {
        assert_eq!(
            output.status.code(),
            Some(1),
            "with --skip-unreadable only individual files may fail"
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        // 一个文件只算一个问题：sy 对同一个文件的失败由 preflight 的记录去重。
        let listed: Vec<&str> = stdout
            .lines()
            .filter(|line| line.starts_with("    - "))
            .filter(|line| line.contains("secret.bin"))
            .collect();
        assert_eq!(listed.len(), 1, "one problem per skipped file: {stdout}");
    }
    assert!(dst.path().join("ok.bin").exists());
}

#[cfg(unix)]
#[test]
fn a_symlinked_target_is_reported_not_verified() {
    // 目标里放着指回源（或任意位置）的符号链接，且大小与 mtime 与源一致时，
    // sy 会跳过这一项；此时若跟着链接去哈希，就会把"没复制"报成"已验证"。
    let src = TempDir::new().unwrap();
    let dst = TempDir::new().unwrap();
    let source_file = src.path().join("f.bin");
    fs::write(&source_file, b"AAAAA").unwrap();
    let link = dst.path().join("f.bin");
    // 链接文本 "f.bin" 恰好 5 字节，与源文件大小一致。
    std::os::unix::fs::symlink("f.bin", &link).unwrap();
    // 链接自身的 mtime 与源文件对齐（`set_symlink_file_times` 不跟随链接）。
    let mtime =
        filetime::FileTime::from_last_modification_time(&fs::metadata(&source_file).unwrap());
    filetime::set_symlink_file_times(&link, mtime, mtime).unwrap();

    let output = rcca().arg(src.path()).arg(dst.path()).output().unwrap();
    assert_eq!(output.status.code(), Some(1), "output: {output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("target is not a regular file (symlink)"),
        "a symlink must not count as a verified copy: {stdout}"
    );
    assert!(
        fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink(),
        "the link is left in place for the user to inspect"
    );
}

#[cfg(unix)]
#[test]
fn skip_unreadable_reports_a_nested_file_once() {
    use std::os::unix::fs::PermissionsExt;

    // 多段相对路径：去重靠 `target_root.join(rel)`，嵌套路径也必须与 sy 的目标路径对上。
    let src = TempDir::new().unwrap();
    let dst = TempDir::new().unwrap();
    write_file(src.path(), "100MEDIA/ok.bin", b"good");
    write_file(src.path(), "100MEDIA/secret.bin", b"data");
    let path = src.path().join("100MEDIA/secret.bin");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).unwrap();

    let output = rcca()
        .arg(src.path())
        .arg(dst.path())
        .arg("--skip-unreadable")
        .output()
        .unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();

    if output.status.code() != Some(0) {
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert_eq!(output.status.code(), Some(1), "output: {stdout}");
        let listed = stdout
            .lines()
            .filter(|line| line.starts_with("    - "))
            .filter(|line| line.contains("100MEDIA/secret.bin"))
            .count();
        assert_eq!(listed, 1, "a nested skip is reported once: {stdout}");
    }
    assert!(dst.path().join("100MEDIA/ok.bin").exists());
}

#[cfg(unix)]
#[test]
fn fail_fast_aborts_on_the_first_error() {
    use std::os::unix::fs::PermissionsExt;

    // 默认不限错误数；--fail-fast 仍然映射成 sy 的 max_errors = 1，复制中途放弃。
    let src = TempDir::new().unwrap();
    let dst = TempDir::new().unwrap();
    for name in ["a.bin", "b.bin", "c.bin"] {
        write_file(src.path(), &format!("sub/{name}"), b"payload");
    }
    let locked = dst.path().join("sub");
    fs::create_dir_all(&locked).unwrap();
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o555)).unwrap();

    let output = rcca()
        .arg(src.path())
        .arg(dst.path())
        .arg("--fail-fast")
        .output()
        .unwrap();
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();

    // root 下写只读目录仍会成功，此时没有错误，断言跳过。
    if output.status.code() != Some(0) {
        assert_eq!(output.status.code(), Some(1));
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("Max errors exceeded"),
            "the copy aborts on the first error: {stderr}"
        );
        assert!(
            output.stdout.is_empty(),
            "an aborted copy prints no summary: {}",
            String::from_utf8_lossy(&output.stdout)
        );
    }
}

#[cfg(unix)]
#[test]
fn the_problem_count_matches_the_listed_problems() {
    use std::os::unix::fs::PermissionsExt;

    // One copy error plus the resulting missing target file: two problems, listed twice.
    // A copy error used to be counted a second time through its own counter.
    let src = TempDir::new().unwrap();
    let dst = TempDir::new().unwrap();
    write_file(src.path(), "sub/a.bin", b"payload");
    let locked = dst.path().join("sub");
    fs::create_dir_all(&locked).unwrap();
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o555)).unwrap();

    let output = rcca().arg(src.path()).arg(dst.path()).output().unwrap();
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    assert_eq!(output.status.code(), Some(1), "output: {stdout}");
    assert!(
        stdout.contains("! Failed: 2 problems"),
        "one copy error and one missing target is two problems: {stdout}"
    );
    let listed = stdout.lines().filter(|l| l.starts_with("    - ")).count();
    assert_eq!(listed, 2, "the count and the list must agree: {stdout}");
}

#[cfg(unix)]
#[test]
fn wrapped_copy_errors_stay_on_one_line() {
    use std::os::unix::fs::PermissionsExt;

    let src = TempDir::new().unwrap();
    let dst = TempDir::new().unwrap();
    write_file(src.path(), "sub/a.bin", b"payload");
    let locked = dst.path().join("sub");
    fs::create_dir_all(&locked).unwrap();
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o555)).unwrap();

    let output = rcca().arg(src.path()).arg(dst.path()).output().unwrap();
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    // sy wraps its errors over several lines; each bullet must stay one line so the
    // list keeps its indentation.
    for line in stdout.lines() {
        assert!(
            !line.starts_with("Cause:") && !line.starts_with("Check disk space"),
            "an unterminated detail line leaked into the summary: {stdout}"
        );
    }
    let bullet = stdout
        .lines()
        .find(|l| l.starts_with("    - copying "))
        .expect("copy error listed");
    assert!(
        bullet.contains("Cause:") && bullet.contains("Permission denied"),
        "the whole message stays on the bullet: {bullet}"
    );
}

#[cfg(unix)]
#[test]
fn a_fifo_in_the_source_fails_preflight() {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let src = TempDir::new().unwrap();
    let dst = TempDir::new().unwrap();
    write_file(src.path(), "real.bin", b"data");
    let fifo = src.path().join("pipe.fifo");
    let path = CString::new(fifo.as_os_str().as_bytes()).unwrap();
    // 管道节点交给 sy 会被当作普通文件读取并永久阻塞，因此必须在 preflight 拒绝。
    let created = unsafe { libc::mkfifo(path.as_ptr(), 0o644) };
    assert_eq!(created, 0, "could not create the fifo");

    let output = rcca().arg(src.path()).arg(dst.path()).output().unwrap();
    assert_eq!(
        output.status.code(),
        Some(3),
        "a special file is a preflight failure: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("special file"), "stderr: {stderr}");
    assert!(stderr.contains("pipe.fifo"), "name the file: {stderr}");
    assert!(
        !dst.path().join("real.bin").exists(),
        "nothing may be copied when the source is refused"
    );
}

#[cfg(unix)]
#[test]
fn a_socket_in_the_source_fails_preflight() {
    use std::os::unix::net::UnixListener;

    let src = TempDir::new().unwrap();
    let dst = TempDir::new().unwrap();
    write_file(src.path(), "real.bin", b"data");
    // `UnixListener` 在文件系统里留下一个套接字节点；绑定成功即创建。
    let _socket = UnixListener::bind(src.path().join("ipc.sock")).unwrap();

    let output = rcca().arg(src.path()).arg(dst.path()).output().unwrap();
    assert_eq!(
        output.status.code(),
        Some(3),
        "a socket is a preflight failure: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn deletion_threshold_aborts_mirror() {
    // Most target files are absent from the source: sy's default --max-delete 50%
    // refuses and reports an error.
    let src = TempDir::new().unwrap();
    let dst = TempDir::new().unwrap();
    write_file(src.path(), "keep.bin", b"keep");
    write_file(dst.path(), "keep.bin", b"keep");
    write_file(dst.path(), "other1.bin", b"stale");
    write_file(dst.path(), "other2.bin", b"stale");

    let output = rcca().arg(src.path()).arg(dst.path()).output().unwrap();
    assert_eq!(output.status.code(), Some(1), "a copy failure returns 1");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("copy failed"), "stderr: {stderr}");
    assert!(
        stderr.contains("Deletion threshold"),
        "sy's delete guard must stay: {stderr}"
    );
    assert!(
        dst.path().join("other1.bin").exists() && dst.path().join("other2.bin").exists(),
        "the target must stay untouched when deletion is refused"
    );
}

#[test]
fn path_with_tmp_suffix_is_rejected() {
    let src = TempDir::new().unwrap();
    let dst = TempDir::new().unwrap();
    write_file(src.path(), ".sy-0badc0de.tmp", b"x");

    let output = rcca().arg(src.path()).arg(dst.path()).output().unwrap();
    assert_eq!(output.status.code(), Some(3));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("path rule conflicts"), "stderr: {stderr}");
}

#[test]
fn nested_source_inside_target_is_usage_error() {
    let parent = TempDir::new().unwrap();
    let src = parent.path().join("card");
    fs::create_dir_all(&src).unwrap();
    write_file(&src, "a.bin", b"x");

    rcca().arg(&src).arg(parent.path()).assert().code(2);
}

#[cfg(unix)]
#[test]
fn sigint_never_leaves_a_partial_final_file() {
    const SIZE: usize = 256 * 1024 * 1024;
    let src = TempDir::new().unwrap();
    let dst = TempDir::new().unwrap();
    let big = src.path().join("big.bin");
    {
        let mut file = fs::File::create(&big).unwrap();
        use std::io::Write;
        let chunk = vec![0xABu8; 1024 * 1024];
        for _ in 0..SIZE / chunk.len() {
            file.write_all(&chunk).unwrap();
        }
        file.sync_all().unwrap();
    }

    let child = StdCommand::new(assert_cmd::cargo::cargo_bin("rcca"))
        .arg(src.path())
        .arg(dst.path())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();

    std::thread::sleep(Duration::from_millis(60));
    unsafe {
        libc::kill(child.id() as i32, libc::SIGINT);
    }
    let status = child.wait_with_output().unwrap().status;

    let final_path = dst.path().join("big.bin");
    if final_path.exists() {
        // The final file name may only appear after a complete copy.
        assert_eq!(
            fs::metadata(&final_path).unwrap().len(),
            SIZE as u64,
            "no incomplete final file may survive an interrupt"
        );
    } else {
        assert!(
            status.code().is_none() || status.code() == Some(130),
            "an interrupt exits with 130 or is killed by the signal: {:?}",
            status.code()
        );
    }
}
