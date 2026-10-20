use std::fmt;

/// 顶层错误分类，直接映射到退出码。
///
/// 退出码约定：
/// 0 成功；1 复制或校验失败；2 用法错误；3 预检失败；130 被中断。
#[derive(Debug)]
pub enum Error {
    Usage(String),
    Preflight(String),
    Copy(String),
    Verify(String),
    Interrupted,
}

impl Error {
    pub fn exit_code(&self) -> i32 {
        match self {
            Error::Usage(_) => 2,
            Error::Preflight(_) => 3,
            Error::Copy(_) | Error::Verify(_) => 1,
            Error::Interrupted => 130,
        }
    }

    pub fn usage(msg: impl Into<String>) -> Self {
        Error::Usage(msg.into())
    }

    pub fn preflight(msg: impl Into<String>) -> Self {
        Error::Preflight(msg.into())
    }

    pub fn copy(msg: impl Into<String>) -> Self {
        Error::Copy(msg.into())
    }

    pub fn verify(msg: impl Into<String>) -> Self {
        Error::Verify(msg.into())
    }

    pub fn preflight_io(context: impl fmt::Display, err: std::io::Error) -> Self {
        Error::Preflight(format!("{context}：{err}"))
    }

    pub fn copy_io(context: impl fmt::Display, err: std::io::Error) -> Self {
        Error::Copy(format!("{context}：{err}"))
    }

    pub fn verify_io(context: impl fmt::Display, err: std::io::Error) -> Self {
        Error::Verify(format!("{context}：{err}"))
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Usage(m) => write!(f, "用法错误：{m}"),
            Error::Preflight(m) => write!(f, "预检失败：{m}"),
            Error::Copy(m) => write!(f, "复制失败：{m}"),
            Error::Verify(m) => write!(f, "校验失败：{m}"),
            Error::Interrupted => write!(f, "已中断"),
        }
    }
}

impl std::error::Error for Error {}

pub type Result<T> = std::result::Result<T, Error>;
