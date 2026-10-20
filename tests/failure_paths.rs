//! 失败路径测试：预检失败、复制冲突、中断、逐文件失败。

use std::fs;
use std::path::Path;
use std::process::{Command as StdCommand, Stdio};
use std::time::Duration;

use assert_cmd::Command;
use tempfile::TempDir;

fn rcca() -> Command {
    Command::cargo_bin("rcca").expect("构建 rcca 二进制")
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
    // root 用户下读取仍会成功，此时任务正常完成。
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
            "跳过不可读文件后应仅失败在个别文件"
        );
    }
    assert!(dst.path().join("ok.bin").exists());
}

#[test]
fn deletion_threshold_aborts_mirror() {
    // 目标里大部分文件在源中不存在：sy 默认 --max-delete 50% 会拒绝删除并报错。
    let src = TempDir::new().unwrap();
    let dst = TempDir::new().unwrap();
    write_file(src.path(), "keep.bin", b"keep");
    write_file(dst.path(), "keep.bin", b"keep");
    write_file(dst.path(), "other1.bin", b"stale");
    write_file(dst.path(), "other2.bin", b"stale");

    let output = rcca().arg(src.path()).arg(dst.path()).output().unwrap();
    assert_eq!(output.status.code(), Some(1), "复制失败应返回 1");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("复制失败"), "stderr：{stderr}");
    assert!(
        stderr.contains("Deletion threshold"),
        "应保留 sy 的删除保护：{stderr}"
    );
    assert!(
        dst.path().join("other1.bin").exists() && dst.path().join("other2.bin").exists(),
        "拒绝删除时目标文件应保持原样"
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
    assert!(stderr.contains("路径规则冲突"), "stderr：{stderr}");
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
        // 只有完整复制后才允许出现最终文件名。
        assert_eq!(
            fs::metadata(&final_path).unwrap().len(),
            SIZE as u64,
            "中断后不应留下不完整的最终文件"
        );
    } else {
        assert!(
            status.code().is_none() || status.code() == Some(130),
            "被中断时退出码应为 130 或被信号终止：{:?}",
            status.code()
        );
    }
}
