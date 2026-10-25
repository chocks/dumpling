use thiserror::Error;

/// Application exit codes for the CLI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i32)]
pub enum ExitCode {
    Success = 0,
    Usage = 2,
    Config = 3,
    Safety = 4,
    Database = 5,
    Validation = 6,
    Internal = 1,
}

impl ExitCode {
    pub fn as_i32(self) -> i32 {
        self as i32
    }
}

#[derive(Debug, Error)]
pub enum DumplingError {
    #[error("{0}")]
    Config(String),

    #[error("safety check failed: {0}")]
    Safety(String),

    #[error("database error: {0}")]
    Database(String),

    #[error("validation failed: {0}")]
    Validation(String),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("{0}")]
    Other(#[from] anyhow::Error),
}

impl DumplingError {
    pub fn exit_code(&self) -> ExitCode {
        match self {
            Self::Config(_) => ExitCode::Config,
            Self::Safety(_) => ExitCode::Safety,
            Self::Database(_) => ExitCode::Database,
            Self::Validation(_) => ExitCode::Validation,
            Self::Io(_) => ExitCode::Internal,
            Self::Other(_) => ExitCode::Internal,
        }
    }
}

pub type Result<T> = std::result::Result<T, DumplingError>;
