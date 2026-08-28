use crate::engine::domain::{
    Entry, EntryIdentity, EntryKind, InvalidRelativePath, InvalidTimestamp, RelativePath, Timestamp,
};
use crate::engine::reconcile::{BoxError, EntryStream};
use crate::engine::scan::ScanRequest;
use ignore::WalkBuilder;
use std::path::{Path, PathBuf};

#[cfg(unix)]
use std::os::unix::fs::{FileTypeExt, MetadataExt};

const CHANNEL_CAPACITY: usize = 256;

#[derive(Debug, thiserror::Error)]
enum LocalScanError {
    #[error("failed to walk local tree: {0}")]
    Walk(String),

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

    #[error("invalid modification timestamp nanoseconds for {path}: {nanoseconds}")]
    TimestampNanoseconds { path: PathBuf, nanoseconds: i64 },

    #[error("unsupported special filesystem entry: {path}")]
    UnsupportedFileType { path: PathBuf },
}

/// Scan a local endpoint into the engine's lean, strictly ordered entry stream.
///
/// Directory walking and metadata syscalls run on a blocking worker. The bounded
/// channel is the backpressure boundary: a slow reconciler cannot cause the
/// scanner to accumulate an unbounded tree in memory.
pub fn local_entry_stream(root: PathBuf, request: ScanRequest) -> EntryStream {
    let (sender, receiver) = tokio::sync::mpsc::channel(CHANNEL_CAPACITY);
    let join_sender = sender.clone();

    tokio::spawn(async move {
        let scan = tokio::task::spawn_blocking(move || scan_worker(root, request, sender)).await;
        if let Err(error) = scan {
            let _ = join_sender.send(Err(Box::new(error) as BoxError)).await;
        }
    });

    Box::pin(futures::stream::unfold(receiver, |mut receiver| async move {
        receiver.recv().await.map(|entry| (entry, receiver))
    }))
}

fn scan_worker(
    root: PathBuf,
    request: ScanRequest,
    sender: tokio::sync::mpsc::Sender<Result<Entry, BoxError>>,
) {
    let mut builder = WalkBuilder::new(&root);
    builder
        .hidden(false)
        .git_ignore(request.respect_gitignore)
        .git_global(request.respect_gitignore)
        .git_exclude(request.respect_gitignore)
        .follow_links(false)
        // The engine validates strict ordering again at the trust boundary. The
        // local walker supplies that order without whole-tree materialization.
        .sort_by_file_path(|left, right| left.cmp(right));

    if !request.include_git_dir {
        builder.filter_entry(|entry| entry.file_name() != ".git");
    }
    if request.respect_gitignore {
        let gitignore = root.join(".gitignore");
        if gitignore.exists() {
            builder.add_ignore(&gitignore);
        }
    }
    if let Some(max_depth) = request.max_depth {
        builder.max_depth(Some(max_depth));
    }

    for result in builder.build() {
        let dir_entry = match result {
            Ok(entry) => entry,
            Err(error) => {
                send_error(&sender, LocalScanError::Walk(error.to_string()));
                return;
            }
        };
        if dir_entry.path() == root {
            continue;
        }

        match engine_entry(&root, dir_entry.path(), request) {
            Ok(entry) => {
                if sender.blocking_send(Ok(entry)).is_err() {
                    return;
                }
            }
            Err(error) => {
                send_error(&sender, error);
                return;
            }
        }
    }
}

fn send_error(sender: &tokio::sync::mpsc::Sender<Result<Entry, BoxError>>, error: LocalScanError) {
    let _ = sender.blocking_send(Err(Box::new(error)));
}

fn engine_entry(root: &Path, path: &Path, request: ScanRequest) -> Result<Entry, LocalScanError> {
    let metadata = std::fs::symlink_metadata(path).map_err(|source| LocalScanError::Metadata {
        path: path.to_path_buf(),
        source,
    })?;
    let relative = path
        .strip_prefix(root)
        .map_err(|_| LocalScanError::OutsideRoot {
            path: path.to_path_buf(),
        })?
        .to_path_buf();
    let relative = RelativePath::new(relative.clone()).map_err(|source| {
        LocalScanError::RelativePath {
            path: relative,
            source,
        }
    })?;
    let modified = metadata_timestamp(path, &metadata)?;
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
        entry.identity = metadata_identity(&metadata, entry.kind);
    }
    if request.metadata.hardlink_group {
        entry.hardlink_group = hardlink_group(&metadata, entry.kind);
    }

    Ok(entry)
}

#[cfg(unix)]
fn metadata_timestamp(path: &Path, metadata: &std::fs::Metadata) -> Result<Timestamp, LocalScanError> {
    let nanoseconds = metadata.mtime_nsec();
    let nanoseconds = u32::try_from(nanoseconds).map_err(|_| {
        LocalScanError::TimestampNanoseconds {
            path: path.to_path_buf(),
            nanoseconds,
        }
    })?;
    Timestamp::new(metadata.mtime(), nanoseconds).map_err(|source| LocalScanError::Timestamp {
        path: path.to_path_buf(),
        source,
    })
}

#[cfg(not(unix))]
fn metadata_timestamp(path: &Path, metadata: &std::fs::Metadata) -> Result<Timestamp, LocalScanError> {
    let modified = metadata.modified().map_err(|source| LocalScanError::Metadata {
        path: path.to_path_buf(),
        source,
    })?;
    system_time_to_timestamp(path, modified)
}

#[cfg(not(unix))]
fn system_time_to_timestamp(path: &Path, time: std::time::SystemTime) -> Result<Timestamp, LocalScanError> {
    use std::time::UNIX_EPOCH;

    match time.duration_since(UNIX_EPOCH) {
        Ok(duration) => {
            let seconds = i64::try_from(duration.as_secs()).map_err(|_| {
                LocalScanError::Timestamp {
                    path: path.to_path_buf(),
                    source: InvalidTimestamp::range(),
                }
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
            let seconds = i64::try_from(duration.as_secs()).map_err(|_| {
                LocalScanError::Timestamp {
                    path: path.to_path_buf(),
                    source: InvalidTimestamp::range(),
                }
            })?;
            let nanos = duration.subsec_nanos();
            let (seconds, nanos) = if nanos == 0 {
                (-seconds, 0)
            } else {
                (seconds.checked_neg().and_then(|value| value.checked_sub(1)).ok_or_else(|| {
                    LocalScanError::Timestamp {
                        path: path.to_path_buf(),
                        source: InvalidTimestamp::range(),
                    }
                })?, 1_000_000_000 - nanos)
            };
            Timestamp::new(seconds, nanos).map_err(|source| LocalScanError::Timestamp {
                path: path.to_path_buf(),
                source,
            })
        }
    }
}

#[cfg(unix)]
fn metadata_identity(metadata: &std::fs::Metadata, kind: EntryKind) -> Option<EntryIdentity> {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"sy-entry-identity-v1\0");
    hasher.update(&metadata.dev().to_le_bytes());
    hasher.update(&metadata.ino().to_le_bytes());
    hasher.update(&metadata.len().to_le_bytes());
    hasher.update(&metadata.mode().to_le_bytes());
    hasher.update(&metadata.mtime().to_le_bytes());
    hasher.update(&metadata.mtime_nsec().to_le_bytes());
    hasher.update(&metadata.ctime().to_le_bytes());
    hasher.update(&metadata.ctime_nsec().to_le_bytes());
    hasher.update(&[entry_kind_tag(kind)]);
    Some(EntryIdentity::from_bytes(*hasher.finalize().as_bytes()))
}

#[cfg(not(unix))]
fn metadata_identity(_metadata: &std::fs::Metadata, _kind: EntryKind) -> Option<EntryIdentity> {
    // A robust Windows identity should use a file ID from an opened handle, not
    // a best-effort size/time fingerprint. Until that endpoint implementation is
    // added, do not advertise a token with stronger semantics than it has.
    None
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

const fn entry_kind_tag(kind: EntryKind) -> u8 {
    match kind {
        EntryKind::File => 1,
        EntryKind::Directory => 2,
        EntryKind::Symlink => 3,
    }
}

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
            metadata: crate::engine::scan::EntryMetadataRequest {
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
            metadata: crate::engine::scan::EntryMetadataRequest {
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
