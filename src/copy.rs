//! 阶段 2 的单文件写入单元：创建 tmp、写数据、fsync、元数据、结果记录。
//!
//! 流水线的写线程与 `--serial-io` 的同步路径共用这里的逻辑，保证两种模式产物完全一致。

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::PathBuf;
use std::time::SystemTime;

/// 提交阶段需要的身份信息：写入时的 dev/ino 与长度。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileInfo {
    pub dev: u64,
    pub ino: u64,
    pub size: u64,
}

#[derive(Debug, Clone)]
pub enum Outcome {
    Ok(FileInfo),
    Failed { reason: String },
}

impl Outcome {
    pub fn is_ok(&self) -> bool {
        matches!(self, Outcome::Ok(_))
    }
}

/// 一个正在写入的文件。
pub struct ActiveFile {
    pub index: usize,
    pub tmp_path: PathBuf,
    file: Option<File>,
    error: Option<String>,
    error_kind: Option<io::ErrorKind>,
    error_raw: Option<i32>,
    written: u64,
    pending_mode: u32,
    pending_mtime: Option<SystemTime>,
}

impl ActiveFile {
    pub fn begin(index: usize, tmp_path: PathBuf) -> Self {
        if let Some(parent) = tmp_path.parent() {
            if !parent.as_os_str().is_empty() && !parent.exists() {
                let _ = fs::create_dir_all(parent);
            }
        }
        match OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&tmp_path)
        {
            Ok(file) => Self {
                index,
                tmp_path,
                file: Some(file),
                error: None,
                error_kind: None,
                error_raw: None,
                written: 0,
                pending_mode: 0o644,
                pending_mtime: None,
            },
            Err(err) => Self {
                index,
                tmp_path,
                file: None,
                error: Some(format!("创建暂存文件失败：{err}")),
                error_kind: Some(err.kind()),
                error_raw: err.raw_os_error(),
                written: 0,
                pending_mode: 0o644,
                pending_mtime: None,
            },
        }
    }

    pub fn set_pending_meta(&mut self, mode: u32, mtime: Option<SystemTime>) {
        self.pending_mode = mode;
        self.pending_mtime = mtime;
    }

    pub fn pending_meta(&self) -> (u32, Option<SystemTime>) {
        (self.pending_mode, self.pending_mtime)
    }

    /// 写入一个数据块，返回实际写入的字节数。失败后进入错误状态，后续写入被忽略。
    pub fn write(&mut self, buf: &[u8]) -> usize {
        if self.error.is_some() {
            return 0;
        }
        let Some(file) = self.file.as_mut() else {
            return 0;
        };
        match file.write_all(buf) {
            Ok(()) => {
                self.written += buf.len() as u64;
                buf.len()
            }
            Err(err) => {
                self.error = Some(format!("写入失败：{err}"));
                self.error_kind = Some(err.kind());
                self.error_raw = err.raw_os_error();
                // 关闭句柄，finish 时统一删除 tmp。
                self.file = None;
                0
            }
        }
    }

    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    pub fn error_kind(&self) -> Option<io::ErrorKind> {
        self.error_kind
    }

    pub fn error_raw(&self) -> Option<i32> {
        self.error_raw
    }

    pub fn written(&self) -> u64 {
        self.written
    }

    /// 结束写入：落盘、复制元数据、返回身份信息。失败时删除 tmp。
    ///
    /// `read_error` 为读线程报告的错误（读失败时目标文件同样不可信）。
    pub fn finish(
        mut self,
        fsync: bool,
        mode: u32,
        mtime: Option<SystemTime>,
        read_error: Option<String>,
    ) -> Outcome {
        if let Some(reason) = read_error {
            if self.error.is_none() {
                self.error = Some(reason);
            }
        }

        if let Some(reason) = self.error.take() {
            self.file = None;
            let _ = fs::remove_file(&self.tmp_path);
            return Outcome::Failed { reason };
        }

        let Some(file) = self.file.take() else {
            let _ = fs::remove_file(&self.tmp_path);
            return Outcome::Failed {
                reason: "暂存文件句柄已关闭".to_string(),
            };
        };

        let result = (|| -> io::Result<FileInfo> {
            if fsync {
                file.sync_all()?;
            }
            apply_metadata(&file, mode, mtime)?;
            let meta = file.metadata()?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                Ok(FileInfo {
                    dev: meta.dev(),
                    ino: meta.ino(),
                    size: meta.len(),
                })
            }
            #[cfg(not(unix))]
            {
                Ok(FileInfo {
                    dev: 0,
                    ino: 0,
                    size: meta.len(),
                })
            }
        })();

        match result {
            Ok(info) => {
                drop(file);
                Outcome::Ok(info)
            }
            Err(err) => {
                drop(file);
                let _ = fs::remove_file(&self.tmp_path);
                Outcome::Failed {
                    reason: format!("收尾失败：{err}"),
                }
            }
        }
    }

    /// 中断或中止时放弃当前文件。
    pub fn abandon(self, reason: &str) -> Outcome {
        drop(self.file);
        let _ = fs::remove_file(&self.tmp_path);
        Outcome::Failed {
            reason: reason.to_string(),
        }
    }
}

fn apply_metadata(file: &File, mode: u32, mtime: Option<SystemTime>) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(mode & 0o777))?;
    }
    #[cfg(not(unix))]
    {
        let _ = (file, mode);
    }
    if let Some(mtime) = mtime {
        let ft = filetime::FileTime::from_system_time(mtime);
        filetime::set_file_handle_times(file, None, Some(ft))?;
    }
    Ok(())
}
