//! 端到端流程测试：正常复制、覆盖、镜像删除、布局、输出模式。

use std::fs;
use std::path::{Path, PathBuf};

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
        assert_eq!(a, b, "{rel} 内容不一致");
    }

    let leftovers: Vec<PathBuf> = collect_files(dst.path())
        .into_iter()
        .filter(|p| p.file_name().unwrap().to_string_lossy().starts_with(".sy-"))
        .collect();
    assert!(leftovers.is_empty(), "不应残留 sy 暂存文件：{leftovers:?}");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("校验（XXH64）：通过 4 个"),
        "输出：{stdout}"
    );
    assert!(stdout.contains("复制（sy）：新建 4 个"), "输出：{stdout}");
    assert!(stdout.contains("全部文件已复制"), "输出：{stdout}");
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
    assert!(!dst.path().join("stale.bin").exists(), "镜像应删除多余文件");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("删除 1 个"), "输出：{stdout}");
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
    assert!(
        stdout.contains("校验（XXH64）：通过 4 个"),
        "输出：{stdout}"
    );
    assert!(!stdout.contains("\u{1b}["), "quiet 模式不应有 ANSI 控制符");
    assert!(!stdout.contains("rcca: 提示"), "quiet 模式不应有提示行");
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
    assert!(stdout.contains("新建 0 个"), "第二遍不应重复复制：{stdout}");
    assert!(stdout.contains("传输 0 B"), "第二遍不应传输数据：{stdout}");
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
        "sy 默认按符号链接复制"
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("校验（XXH64）：通过 1 个"),
        "符号链接不进入校验清单：{stdout}"
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
