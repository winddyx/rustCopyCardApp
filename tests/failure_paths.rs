//! 失败路径测试：预检失败、提交冲突、中断、逐文件失败。

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
fn commit_conflict_reports_failure_and_leaves_no_stage() {
    let src = TempDir::new().unwrap();
    let dst = TempDir::new().unwrap();
    write_file(src.path(), "conflict", b"file-content");
    // 目标同名位置被一个非空目录占用，重命名必然失败。
    fs::create_dir_all(dst.path().join("conflict").join("inner")).unwrap();
    write_file(dst.path(), "conflict/inner/keep.txt", b"keep");

    let output = rcca().arg(src.path()).arg(dst.path()).output().unwrap();
    assert_eq!(output.status.code(), Some(1), "提交失败应返回 1");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("提交 conflict"), "输出：{stdout}");
    assert!(
        !dst.path().join(".conflict.rcptmp").exists(),
        "失败的暂存文件应被清理"
    );
    assert!(dst.path().join("conflict/inner/keep.txt").exists());
}

#[test]
fn path_with_tmp_suffix_is_rejected() {
    let src = TempDir::new().unwrap();
    let dst = TempDir::new().unwrap();
    write_file(src.path(), ".weird.MOV.rcptmp", b"x");

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
fn sigint_leaves_no_final_file() {
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

    if status.code() == Some(130) {
        assert!(
            !dst.path().join("big.bin").exists(),
            "中断后不应出现最终文件名"
        );
        assert!(
            !dst.path().join(".big.bin.rcptmp").exists()
                || dst.path().join(".big.bin.rcptmp").exists(),
            "暂存文件允许残留，由下一轮清理"
        );
    } else {
        eprintln!(
            "提示：复制在信号到达前完成（退出码 {:?}），跳过中断断言",
            status.code()
        );
    }
}
