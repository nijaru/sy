use sy::engine::domain::{EntryIdentity, EntryKind};

#[cfg(unix)]
use std::os::unix::fs::MetadataExt;

/// Build the opaque identity used to detect scan/open races for local entries.
///
/// The token deliberately includes metadata that changes when a regular file is
/// replaced or modified. Callers that opened the file securely should compute
/// the token from that opened handle's metadata rather than re-resolving a path.
#[cfg(unix)]
pub(crate) fn metadata_identity(
    metadata: &std::fs::Metadata,
    kind: EntryKind,
) -> Option<EntryIdentity> {
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
pub(crate) fn metadata_identity(
    _metadata: &std::fs::Metadata,
    _kind: EntryKind,
) -> Option<EntryIdentity> {
    // A robust Windows identity should use a file ID from an opened handle, not
    // a best-effort size/time fingerprint. Until that endpoint implementation is
    // added, do not advertise a token with stronger semantics than it has.
    None
}

#[cfg(unix)]
const fn entry_kind_tag(kind: EntryKind) -> u8 {
    match kind {
        EntryKind::File => 1,
        EntryKind::Directory => 2,
        EntryKind::Symlink => 3,
    }
}
