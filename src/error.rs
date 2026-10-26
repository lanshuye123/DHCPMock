//! Unified error type and exit-code mapping.

use std::io;

/// Application error. The variants map to the exit codes documented in
/// `Design.md` §9.1.
#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("configuration error: {0}")]
    Config(String),

    #[error("permission denied: {0}")]
    Permission(String),

    #[error("netlink error: {0}")]
    Netlink(String),

    #[error("runtime error: {0}")]
    Runtime(String),

    #[error(transparent)]
    Io(#[from] io::Error),
}

impl AppError {
    /// Process exit code for this error.
    pub fn exit_code(&self) -> u8 {
        match self {
            AppError::Config(_) => 2,
            AppError::Permission(_) => 3,
            AppError::Netlink(_) => 4,
            AppError::Runtime(_) | AppError::Io(_) => 1,
        }
    }
}

pub type Result<T, E = AppError> = std::result::Result<T, E>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_codes_match_design() {
        assert_eq!(AppError::Config(String::new()).exit_code(), 2);
        assert_eq!(AppError::Permission(String::new()).exit_code(), 3);
        assert_eq!(AppError::Netlink(String::new()).exit_code(), 4);
        assert_eq!(AppError::Runtime(String::new()).exit_code(), 1);
    }
}
