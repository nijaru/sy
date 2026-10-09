use crate::endpoint::local_identity::metadata_identity;
#[path = "local_entry_scan/traversal.rs"]
mod traversal;
use std::path::{Path, PathBuf};
use sy::engine::domain::{
    Entry, EntryIdentity, EntryKind, InvalidRelativePath, InvalidTimestamp, RelativePath, Timestamp,
};
use sy::engine::reconcile::{BoxError, EntryStream};
use sy::engine::scan::ScanRequest;

#[cfg(unix)]
use std::os::unix::fs::MetadataExt;

const CHANNEL_CAPACITY: usize = 256;

#[derive(Debug, thiserror::Error)]
enum LocalScanError {
    #[error("failed to walk local tree: {0}")]
    Walk(#[from] std::io::Error),

    #[error("local scan directory changed: {0}")]
    DirectoryChanged(PathBuf),

    #[error("local scan symlink loop: {0}")]
    SymlinkLoop(PathBuf),

    #[error("local scan scratch cleanup failed (scan error: {operation:?}): {source}")]
    Cleanup {
        operation: Option<Box<LocalScanError>>,
        #[source]
        source: std::io::Error,
    },

    #[error("failed to read metadata for {path}: {source}")]
    Metadata {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("failed to read symlink target for {path}: {source}")]
    SymlinkTarget {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("path escaped local scan root: {path}")]
    OutsideRoot { path: PathBuf },

    #[error("invalid relative path {path}: {source}")]
    RelativePath {
        path: PathBuf,
        #[source]
        source: InvalidRelativePath,
    },

    #[error("invalid modification timestamp for {path}: {source}")]
    Timestamp {
        path: PathBuf,
        #[source]
        source: InvalidTimestamp,
    },

    #[cfg(not(unix))]
    #[error("modification timestamp for {path} is outside the supported i64-second range")]
    TimestampRange { path: PathBuf },

    #[error("invalid modification timestamp nanoseconds for {path}: {nanoseconds}")]
    TimestampNanoseconds { path: PathBuf, nanoseconds: i64 },

    #[error("unsupported special filesystem entry: {path}")]
    UnsupportedFileType { path: PathBuf },
}

/// Reject unsafe temporary storage before the controller creates any session
/// journals. Scanner workers repeat the check before creating their own scratch.
pub async fn validate_scratch_location(root: PathBuf) -> std::io::Result<()> {
    tokio::task::spawn_blocking(move || traversal::validate_scratch_location(&root))
        .await
        .map_err(std::io::Error::other)?
}

/// Scan a local endpoint into the engine's lean, strictly ordered entry stream.
///
/// Blocking filesystem work runs on a blocking worker. Both the output channel
/// and per-directory sort are bounded; inactive traversal continuations and
/// ancestor identities live on disk, not in an in-memory depth stack. Session
/// owners must first validate source scratch placement before spawning other
/// endpoint scans or creating journals in the same temporary directory.
pub fn local_entry_stream(root: PathBuf, request: ScanRequest) -> EntryStream {
    EntryStream::spawn_blocking(CHANNEL_CAPACITY, move |sender| {
        scan_worker(root, request, sender)
    })
}

/// Observe only one physical endpoint name, with the same owned producer and
/// ignore semantics as a tree scan. Missing source observations remain errors.
pub fn selected_leaf_stream(
    root: PathBuf,
    path: RelativePath,
    request: ScanRequest,
    missing_allowed: bool,
) -> EntryStream {
    EntryStream::spawn_blocking(1, move |sender| {
        traversal::selected_leaf(&root, &path, request, missing_allowed, &sender)
            .map_err(|error| Box::new(error) as BoxError)
    })
}

fn scan_worker(
    root: PathBuf,
    request: ScanRequest,
    sender: tokio::sync::mpsc::Sender<Result<Entry, BoxError>>,
) -> Result<(), BoxError> {
    match traversal::walk_tree(&root, request, &sender, Default::default()) {
        Ok(()) => Ok(()),
        Err(LocalScanError::Walk(error))
            if sender.is_closed() && error.kind() == std::io::ErrorKind::Interrupted =>
        {
            Ok(())
        }
        Err(error) => Err(Box::new(error)),
    }
}

fn entry_metadata(path: &Path, request: ScanRequest) -> Result<std::fs::Metadata, LocalScanError> {
    let lstat = std::fs::symlink_metadata(path).map_err(|source| LocalScanError::Metadata {
        path: path.to_path_buf(),
        source,
    })?;
    // Under --copy-links the entry is its target, not the link: stat through
    // the link so kind/size/mtime describe what a transfer would copy. A
    // dangling link is a loud scan error.
    if request.follow_symlinks && lstat.file_type().is_symlink() {
        std::fs::metadata(path).map_err(|source| LocalScanError::Metadata {
            path: path.to_path_buf(),
            source,
        })
    } else {
        Ok(lstat)
    }
}

fn engine_entry(
    root: &Path,
    path: &Path,
    metadata: &std::fs::Metadata,
    request: ScanRequest,
) -> Result<Entry, LocalScanError> {
    let relative = path
        .strip_prefix(root)
        .map_err(|_| LocalScanError::OutsideRoot {
            path: path.to_path_buf(),
        })?
        .to_path_buf();
    let relative =
        RelativePath::new(relative.clone()).map_err(|source| LocalScanError::RelativePath {
            path: relative,
            source,
        })?;
    let modified = metadata_timestamp(path, metadata)?;
    let file_type = metadata.file_type();

    #[cfg(unix)]
    if !file_type.is_file() && !file_type.is_dir() && !file_type.is_symlink() {
        // FIFOs, sockets and device nodes must never fall through to regular
        // file transfer, where opening them could block or mutate device state.
        return Err(LocalScanError::UnsupportedFileType {
            path: path.to_path_buf(),
        });
    }

    let mut entry = if file_type.is_dir() {
        Entry::directory(relative, modified)
    } else if file_type.is_symlink() {
        let target = if request.metadata.symlink_target {
            std::fs::read_link(path).map_err(|source| LocalScanError::SymlinkTarget {
                path: path.to_path_buf(),
                source,
            })?
        } else {
            PathBuf::new()
        };
        let mut entry = Entry::symlink(relative, target, modified);
        if !request.metadata.symlink_target {
            entry.symlink_target = None;
        }
        entry
    } else {
        Entry::file(relative, metadata.len(), modified)
    };

    #[cfg(unix)]
    if request.metadata.unix_mode {
        entry.unix_mode = Some(metadata.mode() & 0o7777);
    }

    if request.metadata.identity {
        entry.identity = metadata_identity(metadata, entry.kind);
    }
    if request.metadata.hardlink_group {
        entry.hardlink_group = hardlink_group(metadata, entry.kind);
    }

    Ok(entry)
}

#[cfg(unix)]
fn metadata_timestamp(
    path: &Path,
    metadata: &std::fs::Metadata,
) -> Result<Timestamp, LocalScanError> {
    let nanoseconds = metadata.mtime_nsec();
    let nanoseconds =
        u32::try_from(nanoseconds).map_err(|_| LocalScanError::TimestampNanoseconds {
            path: path.to_path_buf(),
            nanoseconds,
        })?;
    Timestamp::new(metadata.mtime(), nanoseconds).map_err(|source| LocalScanError::Timestamp {
        path: path.to_path_buf(),
        source,
    })
}

#[cfg(not(unix))]
fn metadata_timestamp(
    path: &Path,
    metadata: &std::fs::Metadata,
) -> Result<Timestamp, LocalScanError> {
    let modified = metadata
        .modified()
        .map_err(|source| LocalScanError::Metadata {
            path: path.to_path_buf(),
            source,
        })?;
    system_time_to_timestamp(path, modified)
}

#[cfg(not(unix))]
fn system_time_to_timestamp(
    path: &Path,
    time: std::time::SystemTime,
) -> Result<Timestamp, LocalScanError> {
    use std::time::UNIX_EPOCH;

    match time.duration_since(UNIX_EPOCH) {
        Ok(duration) => {
            let seconds =
                i64::try_from(duration.as_secs()).map_err(|_| LocalScanError::TimestampRange {
                    path: path.to_path_buf(),
                })?;
            Timestamp::new(seconds, duration.subsec_nanos()).map_err(|source| {
                LocalScanError::Timestamp {
                    path: path.to_path_buf(),
                    source,
                }
            })
        }
        Err(before_epoch) => {
            let duration = before_epoch.duration();
            let seconds =
                i64::try_from(duration.as_secs()).map_err(|_| LocalScanError::TimestampRange {
                    path: path.to_path_buf(),
                })?;
            let nanos = duration.subsec_nanos();
            let (seconds, nanos) = if nanos == 0 {
                (
                    seconds
                        .checked_neg()
                        .ok_or_else(|| LocalScanError::TimestampRange {
                            path: path.to_path_buf(),
                        })?,
                    0,
                )
            } else {
                (
                    seconds
                        .checked_neg()
                        .and_then(|value| value.checked_sub(1))
                        .ok_or_else(|| LocalScanError::TimestampRange {
                            path: path.to_path_buf(),
                        })?,
                    1_000_000_000 - nanos,
                )
            };
            Timestamp::new(seconds, nanos).map_err(|source| LocalScanError::Timestamp {
                path: path.to_path_buf(),
                source,
            })
        }
    }
}

#[cfg(unix)]
fn hardlink_group(metadata: &std::fs::Metadata, kind: EntryKind) -> Option<EntryIdentity> {
    if kind != EntryKind::File || metadata.nlink() <= 1 {
        return None;
    }

    let mut hasher = blake3::Hasher::new();
    hasher.update(b"sy-hardlink-group-v1\0");
    hasher.update(&metadata.dev().to_le_bytes());
    hasher.update(&metadata.ino().to_le_bytes());
    Some(EntryIdentity::from_bytes(*hasher.finalize().as_bytes()))
}

#[cfg(not(unix))]
fn hardlink_group(_metadata: &std::fs::Metadata, _kind: EntryKind) -> Option<EntryIdentity> {
    None
}

#[cfg(test)]
#[path = "local_entry_scan/resource_tests.rs"]
mod resource_tests;

#[cfg(test)]
#[path = "local_entry_scan/differential.rs"]
mod differential;

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;
    use tempfile::TempDir;

    async fn collect(root: &Path, request: ScanRequest) -> Vec<Entry> {
        local_entry_stream(root.to_path_buf(), request)
            .map(|entry| entry.unwrap())
            .collect::<Vec<_>>()
            .await
    }

    #[tokio::test]
    async fn emits_strictly_ordered_lean_entries() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir(dir.path().join("b")).unwrap();
        std::fs::create_dir(dir.path().join("a")).unwrap();
        std::fs::write(dir.path().join("z"), b"z").unwrap();
        std::fs::write(dir.path().join("a").join("file"), b"a").unwrap();

        let entries = collect(dir.path(), ScanRequest::default()).await;
        assert!(entries.windows(2).all(|pair| pair[0].path < pair[1].path));
        assert!(entries.iter().all(|entry| entry.unix_mode.is_none()));
        #[cfg(unix)]
        assert!(entries.iter().all(|entry| entry.identity.is_some()));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn hardlinks_share_requested_group_without_global_scan_state() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("first"), b"content").unwrap();
        std::fs::hard_link(dir.path().join("first"), dir.path().join("second")).unwrap();
        let request = ScanRequest {
            metadata: sy::engine::scan::EntryMetadataRequest {
                hardlink_group: true,
                ..Default::default()
            },
            ..Default::default()
        };

        let entries = collect(dir.path(), request).await;
        assert_eq!(entries.len(), 2);
        assert!(entries[0].hardlink_group.is_some());
        assert_eq!(entries[0].hardlink_group, entries[1].hardlink_group);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn mode_and_symlink_target_are_request_driven() {
        let dir = TempDir::new().unwrap();
        std::os::unix::fs::symlink("missing", dir.path().join("link")).unwrap();
        let request = ScanRequest {
            metadata: sy::engine::scan::EntryMetadataRequest {
                unix_mode: true,
                symlink_target: false,
                ..Default::default()
            },
            ..Default::default()
        };

        let entries = collect(dir.path(), request).await;
        assert_eq!(entries.len(), 1);
        assert!(entries[0].unix_mode.is_some());
        assert!(entries[0].symlink_target.is_none());
    }
}
