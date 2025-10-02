use std::path::PathBuf;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum SyncError {
    #[allow(dead_code)] // Used in future phases (network sync)
    #[error("Source path not found: {path}")]
    SourceNotFound { path: PathBuf },

    #[allow(dead_code)] // Used in future phases (network sync)
    #[error("Destination path not found: {path}")]
    DestinationNotFound { path: PathBuf },

    #[allow(dead_code)] // Used in future phases (permission handling)
    #[error("Permission denied: {path}")]
    PermissionDenied { path: PathBuf },

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Failed to read directory: {path}")]
    ReadDirError { path: PathBuf, source: std::io::Error },

    #[error("Failed to copy file: {path}")]
    CopyError { path: PathBuf, source: std::io::Error },

    #[error("Invalid path: {path}")]
    InvalidPath { path: PathBuf },
}

pub type Result<T> = std::result::Result<T, SyncError>;
