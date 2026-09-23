use crate::resource::format_bytes;
use std::path::PathBuf;
use thiserror::Error;

#[derive(Error, Debug)]
#[allow(dead_code)]
pub enum SyncError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Deletion threshold exceeded: {percentage:.1}% > {threshold}% (use --force-delete to override)")]
    DeletionThresholdExceeded { percentage: f64, threshold: u8 },

    #[error(
        "Deletion count limit exceeded: {delete_candidates} > {limit} (use --force-delete to override)"
    )]
    DeletionCountExceeded { delete_candidates: u64, limit: u64 },

    #[error(
        "Destination namespace collision: '{colliding}' collides with '{existing}' under case-insensitive/normalizing semantics"
    )]
    NamespaceCollision {
        existing: PathBuf,
        colliding: PathBuf,
    },

    #[error(
        "Destination namespace ambiguity: '{colliding}' may collide with '{existing}'; destination name semantics could not be determined"
    )]
    NamespaceAmbiguity {
        existing: PathBuf,
        colliding: PathBuf,
    },

    #[error(
        "Unsupported type transition: '{path}' changes entry kind ({source_kind:?} over {destination_kind:?})\nTransactional directory replacement is not implemented; the destination was left unchanged."
    )]
    UnsupportedTypeTransition {
        path: PathBuf,
        source_kind: String,
        destination_kind: String,
    },

    #[error(
        "Preservation conflict: {path} ended with mode {actual:#o} after applying its ACL (expected {expected:#o})"
    )]
    PreservationConflict {
        path: PathBuf,
        expected: u32,
        actual: u32,
    },

    #[error(
        "Source changed during transfer: {path}\nThe source was modified or replaced after it was scanned; the destination was left unchanged."
    )]
    SourceChanged { path: PathBuf },

    #[error(
        "Destination changed during transfer: {path}\nThe destination was modified or replaced after it was scanned; the transfer was aborted to avoid overwriting it."
    )]
    DestinationChanged { path: PathBuf },

    #[error("Failed to read directory: {path}\nCause: {source}\nCheck that the directory exists and you have read permissions.")]
    ReadDirError {
        path: PathBuf,
        source: std::io::Error,
    },

    #[error("Invalid path: {path}\nPaths must be valid UTF-8 and not contain invalid characters.")]
    InvalidPath { path: PathBuf },

    #[error("Insufficient disk space: {path}\nRequired: {required} bytes ({required_fmt})\nAvailable: {available} bytes ({available_fmt})\nFree up space or retry with a different destination.",
        required_fmt = format_bytes(*required),
        available_fmt = format_bytes(*available))]
    InsufficientDiskSpace {
        path: PathBuf,
        required: u64,
        available: u64,
    },

    #[error("Hook execution failed: {0}\nCheck your hook script for errors or use --no-hooks to disable.")]
    Hook(String),

    #[error("Configuration error: {0}")]
    Config(String),
}

pub type Result<T> = std::result::Result<T, SyncError>;
