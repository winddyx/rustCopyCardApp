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
}

impl Error {
    pub fn exit_code(&self) -> i32 {
        match self {
            Error::Usage(_) => 2,
            Error::Preflight(_) => 3,
            Error::Copy(_) => 1,
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

    pub fn preflight_io(context: impl fmt::Display, err: std::io::Error) -> Self {
        Error::Preflight(format!("{context}：{err}"))
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Usage(m) => write!(f, "用法错误：{m}"),
            Error::Preflight(m) => write!(f, "预检失败：{m}"),
            Error::Copy(m) => write!(f, "复制失败：{m}"),
        }
    }
}

impl std::error::Error for Error {}

pub type Result<T> = std::result::Result<T, Error>;
