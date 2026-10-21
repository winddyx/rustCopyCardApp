use std::fmt;

/// Top-level error classes, mapped directly to exit codes.
///
/// Exit codes: 0 success; 1 copy or verify failure; 2 usage error; 3 preflight failure;
/// 130 interrupted.
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
        Error::Preflight(format!("{context}: {err}"))
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Usage(m) => write!(f, "usage error: {m}"),
            Error::Preflight(m) => write!(f, "preflight failed: {m}"),
            Error::Copy(m) => write!(f, "copy failed: {m}"),
        }
    }
}

impl std::error::Error for Error {}

pub type Result<T> = std::result::Result<T, Error>;
