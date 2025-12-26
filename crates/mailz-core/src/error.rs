//! Error types for the core library.

use thiserror::Error;

/// Core library error type.
#[derive(Debug, Error)]
pub enum CoreError {
    #[error("configuration error: {0}")]
    Config(String),

    #[error("path error: {0}")]
    Path(String),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("serialization error: {0}")]
    Serialization(String),
}

/// Result type alias using CoreError.
pub type Result<T> = std::result::Result<T, CoreError>;
