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

    #[error("Network timeout after {duration:?}\nThe connection timed out. This is usually temporary - retry with --retry flag.")]
    NetworkTimeout { duration: std::time::Duration },

    #[error("Network disconnected: {reason}\nThe SSH connection was lost. This is usually temporary - retry with --retry flag.")]
    NetworkDisconnected { reason: String },

    #[error("Network error (retryable): {message}\nAttempts: {attempts}/{max_attempts}\nThis error may succeed if retried.")]
    NetworkRetryable {
        message: String,
        attempts: u32,
        max_attempts: u32,
    },

    #[error("Network error (fatal): {message}\nThis error cannot be resolved by retrying. Check your configuration.")]
    NetworkFatal { message: String },

    #[error("Hook execution failed: {0}\nCheck your hook script for errors or use --no-hooks to disable.")]
    Hook(String),

    #[error("Configuration error: {0}")]
    Config(String),

    #[error("Bisync state file corrupted: {path}\nReason: {reason}\n\nTo recover:\n  1. Backup the corrupt file (optional): cp {path} {path}.backup\n  2. Rebuild state from scratch: sy --force-resync <source> <dest>\n\nNote: First sync after recovery will treat all differences as new changes.")]
    StateCorruption { path: PathBuf, reason: String },

    #[error("Sync already in progress for this directory pair:\n  Source: {source_path}\n  Dest: {dest_path}\n  Lock file: {lock_file}\n\nAnother sy process is currently syncing these directories.\nWait for it to complete or check if the process is still running.\n\nIf no sync is running and the lock is stale:\n  rm {lock_file}")]
    SyncLocked {
        source_path: String,
        dest_path: String,
        lock_file: String,
    },

    #[error("Database error: {0}\nCheck that the destination directory is writable.")]
    Database(String),

    #[error("Data corruption detected: {path}\nBlock {block_number} checksum mismatch after write.\nExpected: {expected_checksum}\nActual: {actual_checksum}\nThis indicates storage or memory corruption. The transfer has been aborted.")]
    BlockCorruption {
        path: PathBuf,
        block_number: usize,
        expected_checksum: String,
        actual_checksum: String,
    },
}

impl From<fjall::Error> for SyncError {
    fn from(err: fjall::Error) -> Self {
        SyncError::Database(err.to_string())
    }
}

impl From<bincode::Error> for SyncError {
    fn from(err: bincode::Error) -> Self {
        SyncError::Database(err.to_string())
    }
}

impl SyncError {
    /// Check if this error is retryable (network issues that might succeed on retry)
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            SyncError::NetworkTimeout { .. }
                | SyncError::NetworkDisconnected { .. }
                | SyncError::NetworkRetryable { .. }
        )
    }

    /// Check if this error requires reconnection (session is dead)
    #[allow(dead_code)] // Reserved for future connection pool health checks
    pub fn requires_reconnection(&self) -> bool {
        matches!(self, SyncError::NetworkDisconnected { .. })
    }

    /// Classify an IO error from SSH operations into appropriate network error types
    pub fn from_ssh_io_error(err: std::io::Error, context: &str) -> Self {
        use std::io::ErrorKind;

        match err.kind() {
            // Definitely retryable - connection issues
            ErrorKind::ConnectionRefused
            | ErrorKind::ConnectionReset
            | ErrorKind::ConnectionAborted
            | ErrorKind::BrokenPipe
            | ErrorKind::NotConnected => SyncError::NetworkDisconnected {
                reason: format!("{}: {}", context, err),
            },

            // Timeout - retryable
            ErrorKind::TimedOut => SyncError::NetworkTimeout {
                duration: std::time::Duration::from_secs(30), // Default, can be made configurable
            },

            // Temporary failures - retryable
            ErrorKind::Interrupted | ErrorKind::WouldBlock => SyncError::NetworkRetryable {
                message: format!("{}: {}", context, err),
                attempts: 0,
                max_attempts: 3, // Will be updated by retry logic
            },

            // Fatal - configuration or permission issues
            ErrorKind::PermissionDenied => SyncError::PermissionDenied {
                path: std::path::PathBuf::from(context),
            },

            ErrorKind::NotFound => SyncError::SourceNotFound {
                path: std::path::PathBuf::from(context),
            },

            // Everything else - fatal network error
            _ => SyncError::NetworkFatal {
                message: format!("{}: {}", context, err),
            },
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::ErrorKind;
    use std::time::Duration;

    #[test]
    fn test_is_retryable_network_timeout() {
        let err = SyncError::NetworkTimeout {
            duration: Duration::from_secs(30),
        };
        assert!(err.is_retryable());
    }

    #[test]
    fn test_is_retryable_network_disconnected() {
        let err = SyncError::NetworkDisconnected {
            reason: "Connection lost".to_string(),
        };
        assert!(err.is_retryable());
    }

    #[test]
    fn test_is_retryable_network_retryable() {
        let err = SyncError::NetworkRetryable {
            message: "Temporary failure".to_string(),
            attempts: 1,
            max_attempts: 3,
        };
        assert!(err.is_retryable());
    }

    #[test]
    fn test_is_retryable_network_fatal() {
        let err = SyncError::NetworkFatal {
            message: "Fatal error".to_string(),
        };
        assert!(!err.is_retryable());
    }

    #[test]
    fn test_is_retryable_other_errors() {
        let err = SyncError::Io(std::io::Error::other("Some IO error"));
        assert!(!err.is_retryable());

        let err = SyncError::Config("Invalid config".to_string());
        assert!(!err.is_retryable());
    }

    #[test]
    fn test_requires_reconnection_network_disconnected() {
        let err = SyncError::NetworkDisconnected {
            reason: "Connection lost".to_string(),
        };
        assert!(err.requires_reconnection());
    }

    #[test]
    fn test_requires_reconnection_other_errors() {
        let err = SyncError::NetworkTimeout {
            duration: Duration::from_secs(30),
        };
        assert!(!err.requires_reconnection());

        let err = SyncError::NetworkRetryable {
            message: "Temporary failure".to_string(),
            attempts: 1,
            max_attempts: 3,
        };
        assert!(!err.requires_reconnection());

        let err = SyncError::NetworkFatal {
            message: "Fatal error".to_string(),
        };
        assert!(!err.requires_reconnection());
    }

    #[test]
    fn test_from_ssh_io_error_connection_errors() {
        let test_cases = vec![
            ErrorKind::ConnectionRefused,
            ErrorKind::ConnectionReset,
            ErrorKind::ConnectionAborted,
            ErrorKind::BrokenPipe,
            ErrorKind::NotConnected,
        ];

        for kind in test_cases {
            let io_err = std::io::Error::new(kind, "test");
            let sync_err = SyncError::from_ssh_io_error(io_err, "test context");
            assert!(
                matches!(sync_err, SyncError::NetworkDisconnected { .. }),
                "Expected NetworkDisconnected for {:?}, got {:?}",
                kind,
                sync_err
            );
            assert!(sync_err.is_retryable());
            assert!(sync_err.requires_reconnection());
        }
    }

    #[test]
    fn test_from_ssh_io_error_timeout() {
        let io_err = std::io::Error::new(ErrorKind::TimedOut, "timeout");
        let sync_err = SyncError::from_ssh_io_error(io_err, "test context");
        assert!(matches!(sync_err, SyncError::NetworkTimeout { .. }));
        assert!(sync_err.is_retryable());
        assert!(!sync_err.requires_reconnection());
    }

    #[test]
    fn test_from_ssh_io_error_temporary_failures() {
        let test_cases = vec![ErrorKind::Interrupted, ErrorKind::WouldBlock];

        for kind in test_cases {
            let io_err = std::io::Error::new(kind, "test");
            let sync_err = SyncError::from_ssh_io_error(io_err, "test context");
            assert!(
                matches!(sync_err, SyncError::NetworkRetryable { .. }),
                "Expected NetworkRetryable for {:?}, got {:?}",
                kind,
                sync_err
            );
            assert!(sync_err.is_retryable());
            assert!(!sync_err.requires_reconnection());
        }
    }

    #[test]
    fn test_from_ssh_io_error_permission_denied() {
        let io_err = std::io::Error::new(ErrorKind::PermissionDenied, "access denied");
        let sync_err = SyncError::from_ssh_io_error(io_err, "test context");
        assert!(matches!(sync_err, SyncError::PermissionDenied { .. }));
        assert!(!sync_err.is_retryable());
    }

    #[test]
    fn test_from_ssh_io_error_not_found() {
        let io_err = std::io::Error::new(ErrorKind::NotFound, "not found");
        let sync_err = SyncError::from_ssh_io_error(io_err, "test context");
        assert!(matches!(sync_err, SyncError::SourceNotFound { .. }));
        assert!(!sync_err.is_retryable());
    }

    #[test]
    fn test_from_ssh_io_error_fatal() {
        let io_err = std::io::Error::other("unknown error");
        let sync_err = SyncError::from_ssh_io_error(io_err, "test context");
        assert!(matches!(sync_err, SyncError::NetworkFatal { .. }));
        assert!(!sync_err.is_retryable());
    }

    #[test]
    fn test_from_ssh_io_error_context_preserved() {
        let io_err = std::io::Error::new(ErrorKind::ConnectionReset, "reset");
        let sync_err = SyncError::from_ssh_io_error(io_err, "reading file");

        if let SyncError::NetworkDisconnected { reason } = sync_err {
            assert!(reason.contains("reading file"));
            assert!(reason.contains("reset"));
        } else {
            panic!("Expected NetworkDisconnected");
        }
    }
}
