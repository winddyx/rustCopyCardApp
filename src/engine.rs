//! 复制阶段：调用内嵌的 sy 引擎（vendor/sy，上游 nijaru/sy v0.4.1）。
//!
//! rcca 只负责参数映射与路径语义：源目录按“复制内容”传递（等价于上游
//! `sy <source>/ <target> --delete`），遍历与校验仍由 rcca 自己做。

use std::path::Path;

use sy::sync::{DeleteMode, SyncConfig, SyncEngine, SyncStats};
use sy::transport::local::LocalTransport;

use crate::error::{Error, Result};

/// 内嵌 sy 的上游版本。
pub const VERSION: &str = "v0.4.1";

/// 删除保护阈值（百分比），与上游 `--max-delete 50%` 的默认值一致。
const DELETE_THRESHOLD: u8 = 50;

#[derive(Debug, Clone)]
pub struct Options {
    /// 删除目标中源里不存在的文件（镜像语义）。
    pub delete: bool,
    /// 抑制 sy 自身的输出（对应 rcca 的 --quiet）。
    pub quiet: bool,
    /// 显示 sy 的逐文件进度。
    pub progress: bool,
    /// 出错上限，用于实现 --fail-fast。
    pub max_errors: Option<usize>,
}

impl Options {
    /// 默认镜像参数：保留删除镜像语义，输出随 rcca 的安静/详细设置。
    pub fn mirror(quiet: bool) -> Self {
        Self {
            delete: true,
            quiet,
            progress: !quiet,
            max_errors: None,
        }
    }
}

/// 把源目录内容同步到目标目录，返回 sy 的统计。
pub fn copy_tree(source: &Path, target: &Path, opts: &Options) -> Result<SyncStats> {
    let mut config = SyncConfig::test_default();
    config.quiet = opts.quiet;
    config.progress = opts.progress;
    config.delete = if opts.delete {
        DeleteMode::Enabled {
            threshold: DELETE_THRESHOLD,
            force: false,
        }
    } else {
        DeleteMode::Disabled
    };
    config.max_delete = Some(format!("{DELETE_THRESHOLD}%"));
    if let Some(limit) = opts.max_errors {
        config.max_errors = limit;
    }

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|err| Error::copy(format!("无法创建异步运行时：{err}")))?;

    let engine = SyncEngine::with_config(LocalTransport::new(), config);
    runtime
        .block_on(engine.sync(source, target))
        .map_err(|err| Error::copy(format!("sy：{err}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mirror_options_enable_delete_and_progress() {
        let opts = Options::mirror(false);
        assert!(opts.delete);
        assert!(!opts.quiet);
        assert!(opts.progress);

        let quiet = Options::mirror(true);
        assert!(quiet.quiet);
        assert!(!quiet.progress);
    }

    #[test]
    fn copies_directory_contents() {
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();

        std::fs::create_dir_all(src.path().join("a/b")).unwrap();
        std::fs::write(src.path().join("a/b/clip.MP4"), b"payload").unwrap();
        std::fs::write(src.path().join("root.txt"), b"root").unwrap();

        let stats = copy_tree(src.path(), dst.path(), &Options::mirror(true)).unwrap();

        // 源目录的内容落在目标根目录下，而不是再套一层源目录名。
        assert_eq!(
            std::fs::read(dst.path().join("a/b/clip.MP4")).unwrap(),
            b"payload"
        );
        assert_eq!(std::fs::read(dst.path().join("root.txt")).unwrap(), b"root");
        assert!(stats.files_created + stats.files_updated >= 2);
        assert_eq!(stats.errors.len(), 0);
    }

    #[test]
    fn mirror_deletes_files_missing_in_source() {
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();

        std::fs::write(src.path().join("keep1.txt"), b"1").unwrap();
        std::fs::write(src.path().join("keep2.txt"), b"2").unwrap();
        std::fs::write(dst.path().join("keep1.txt"), b"1").unwrap();
        std::fs::write(dst.path().join("keep2.txt"), b"2").unwrap();
        std::fs::write(dst.path().join("stale.txt"), b"old").unwrap();

        let stats = copy_tree(src.path(), dst.path(), &Options::mirror(true)).unwrap();

        assert!(!dst.path().join("stale.txt").exists(), "多余文件应被删除");
        assert_eq!(stats.files_deleted, 1);
    }

    #[test]
    fn deletion_threshold_stays_at_upstream_default() {
        // 上游默认 --max-delete 50%：目标里几乎全是多余文件时会拒绝删除。
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();
        std::fs::write(dst.path().join("stale.txt"), b"old").unwrap();

        let err = copy_tree(src.path(), dst.path(), &Options::mirror(true)).unwrap_err();
        let message = format!("{err}");
        assert!(
            message.contains("Deletion threshold"),
            "应保留 sy 的删除保护：{message}"
        );
        assert!(dst.path().join("stale.txt").exists(), "被拒绝时不应删除");
    }
}
