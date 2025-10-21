use std::path::PathBuf;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum SyncError {
    #[allow(dead_code)] // Used in future phases (network sync)
    #[error(
        "Source path not found: {path}\nMake sure the path exists and you have read permissions."
    )]
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

    #[error("Delta sync failed for {path}\nStrategy: {strategy}\nCause: {source}\n{hint}")]
    #[allow(clippy::enum_variant_names)]
    DeltaSyncError {
        path: PathBuf,
        strategy: String,
        source: std::io::Error,
        hint: String,
    },

    #[error("Invalid path: {path}\nPaths must be valid UTF-8 and not contain invalid characters.")]
    InvalidPath { path: PathBuf },

    #[error("Insufficient disk space: {path}\nRequired: {required} bytes ({required_fmt})\nAvailable: {available} bytes ({available_fmt})\nFree up space or reduce the amount of data to sync.",
        required_fmt = format_bytes(*required),
        available_fmt = format_bytes(*available))]
    InsufficientDiskSpace {
        path: PathBuf,
        required: u64,
        available: u64,
    },

    #[allow(dead_code)] // Used in future phases (network sync)
    #[error("Network error: {message}\nCheck your network connection and try again.")]
    NetworkError { message: String },

    #[error("Hook execution failed: {0}\nCheck your hook script for errors or use --no-hooks to disable.")]
    Hook(String),

    #[error("Configuration error: {0}")]
    Config(String),
}

pub type Result<T> = std::result::Result<T, SyncError>;

/// Format bytes for human-readable display in error messages
pub fn format_bytes(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;
    const TB: u64 = GB * 1024;

    if bytes >= TB {
        format!("{:.2} TB", bytes as f64 / TB as f64)
    } else if bytes >= GB {
        format!("{:.2} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.2} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.2} KB", bytes as f64 / KB as f64)
    } else {
        format!("{} B", bytes)
    }
}
