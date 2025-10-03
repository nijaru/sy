use std::path::PathBuf;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum SyncError {
    #[allow(dead_code)] // Used in future phases (network sync)
    #[error("Source path not found: {path}\nMake sure the path exists and you have read permissions.")]
    SourceNotFound { path: PathBuf },

    #[allow(dead_code)] // Used in future phases (network sync)
    #[error("Destination path not found: {path}\nThe parent directory must exist before syncing.")]
    DestinationNotFound { path: PathBuf },

    #[allow(dead_code)] // Used in future phases (permission handling)
    #[error("Permission denied: {path}\nTry checking file ownership or running with appropriate permissions.")]
    PermissionDenied { path: PathBuf },

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Failed to read directory: {path}\nCause: {source}\nCheck that the directory exists and you have read permissions.")]
    ReadDirError {
        path: PathBuf,
        source: std::io::Error,
    },

    #[error("Failed to copy file: {path}\nCause: {source}\nCheck disk space and write permissions on the destination.")]
    CopyError {
        path: PathBuf,
        source: std::io::Error,
    },

    #[error("Invalid path: {path}\nPaths must be valid UTF-8 and not contain invalid characters.")]
    InvalidPath { path: PathBuf },

    #[allow(dead_code)] // Used in future phases (disk space checking)
    #[error("Insufficient disk space\nThe destination drive does not have enough free space for this operation.")]
    InsufficientDiskSpace,

    #[allow(dead_code)] // Used in future phases (network sync)
    #[error("Network error: {message}\nCheck your network connection and try again.")]
    NetworkError { message: String },
}

pub type Result<T> = std::result::Result<T, SyncError>;
