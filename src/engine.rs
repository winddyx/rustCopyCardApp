//! Copy phase: drive the embedded sy engine (vendor/sy, upstream nijaru/sy v0.4.1).
//!
//! rcca only maps options and path semantics: the source directory is passed by
//! content (equivalent to `sy <source>/ <target> --delete`); traversal and
//! verification stay inside rcca.

use std::path::Path;

use sy::sync::{DeleteMode, SyncConfig, SyncEngine, SyncStats};
use sy::transport::local::LocalTransport;

use crate::error::{Error, Result};

/// Upstream sy version embedded here.
pub const VERSION: &str = "v0.4.1";

/// Delete guard threshold in percent, same as upstream `--max-delete 50%`.
const DELETE_THRESHOLD: u8 = 50;

#[derive(Debug, Clone)]
pub struct Options {
    /// Delete files in the target that no longer exist in the source (mirror semantics).
    pub delete: bool,
    /// Number of parallel transfers handed to sy (its `-j`); 1 means serial.
    pub jobs: usize,
    /// Error limit, used to implement --fail-fast.
    pub max_errors: Option<usize>,
}

impl Options {
    /// Default mirror options: keep the delete semantics, everything else upstream default.
    pub fn mirror() -> Self {
        Self {
            delete: true,
            jobs: 1,
            max_errors: None,
        }
    }
}

/// Register a callback fired when a file starts copying; the interface shows its name.
///
/// sy prints no progress of its own: everything visible during the copy is drawn by rcca.
pub fn set_file_observer<F>(observer: F)
where
    F: Fn(&Path) + Send + Sync + 'static,
{
    sy::sync::progress::set_file_observer(Box::new(observer));
}

/// Register a callback fired for every finished task: the bytes it accounted for and
/// whether it transferred one regular file.
pub fn set_bytes_observer<F>(observer: F)
where
    F: Fn(u64, bool) + Send + Sync + 'static,
{
    sy::sync::progress::set_bytes_observer(Box::new(observer));
}

/// Mirror the source directory into the target and return sy's counters.
pub fn copy_tree(source: &Path, target: &Path, opts: &Options) -> Result<SyncStats> {
    let mut config = SyncConfig::test_default();
    // sy only moves bytes here: bars, planning messages and per-file progress are off,
    // the interface is drawn entirely by rcca.
    config.quiet = true;
    config.progress = false;
    config.max_concurrent = opts.jobs.max(1);
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
        .map_err(|err| Error::copy(format!("cannot create the async runtime: {err}")))?;

    let engine = SyncEngine::with_config(LocalTransport::new(), config);
    runtime
        .block_on(engine.sync(source, target))
        .map_err(|err| Error::copy(format!("sy: {err}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mirror_options_enable_delete() {
        let opts = Options::mirror();
        assert!(opts.delete);
        assert_eq!(opts.jobs, 1);
        assert!(opts.max_errors.is_none());
    }

    #[test]
    fn file_observer_sees_each_copied_file() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        let seen = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&seen);
        set_file_observer(move |_path| {
            counter.fetch_add(1, Ordering::Relaxed);
        });

        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();
        std::fs::write(src.path().join("a.txt"), b"a").unwrap();
        std::fs::write(src.path().join("b.txt"), b"b").unwrap();

        copy_tree(src.path(), dst.path(), &Options::mirror()).unwrap();
        assert!(
            seen.load(Ordering::Relaxed) >= 2,
            "every file should be reported: {}",
            seen.load(Ordering::Relaxed)
        );
    }

    #[test]
    fn copies_directory_contents() {
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();

        std::fs::create_dir_all(src.path().join("a/b")).unwrap();
        std::fs::write(src.path().join("a/b/clip.MP4"), b"payload").unwrap();
        std::fs::write(src.path().join("root.txt"), b"root").unwrap();

        let stats = copy_tree(src.path(), dst.path(), &Options::mirror()).unwrap();

        // The contents of the source land in the target root, not one level deeper.
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

        let stats = copy_tree(src.path(), dst.path(), &Options::mirror()).unwrap();

        assert!(
            !dst.path().join("stale.txt").exists(),
            "extra files must be deleted"
        );
        assert_eq!(stats.files_deleted, 1);
    }

    #[test]
    fn deletion_threshold_stays_at_upstream_default() {
        // Upstream defaults to --max-delete 50%: refuse when almost everything would go.
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();
        std::fs::write(dst.path().join("stale.txt"), b"old").unwrap();

        let err = copy_tree(src.path(), dst.path(), &Options::mirror()).unwrap_err();
        let message = format!("{err}");
        assert!(
            message.contains("Deletion threshold"),
            "sy's delete guard should stay in place: {message}"
        );
        assert!(
            dst.path().join("stale.txt").exists(),
            "nothing may be deleted when the guard refuses"
        );
    }
}
