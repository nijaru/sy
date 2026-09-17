use sy::engine::domain::{EntryIdentity, EntryKind};

#[cfg(unix)]
use std::os::unix::fs::MetadataExt;

/// Build the opaque identity used to detect scan/open races for local entries.
///
/// The token deliberately includes metadata that changes when a regular file is
/// replaced or modified. Callers that opened the file securely should compute
/// the token from that opened handle's metadata rather than re-resolving a path.
///
/// For directories, the token includes device, inode, and mode, but deliberately
/// excludes child-modified timestamps (`mtime`/`ctime`) and size so that
/// delete-replay can validate the destination directory inode has not been
/// replaced even after unlinking children updates its timestamps.
#[cfg(unix)]
pub(crate) fn metadata_identity(
    metadata: &std::fs::Metadata,
    kind: EntryKind,
) -> Option<EntryIdentity> {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"sy-entry-identity-v1\0");
    hasher.update(&metadata.dev().to_le_bytes());
    hasher.update(&metadata.ino().to_le_bytes());
    if kind == EntryKind::Directory {
        hasher.update(&metadata.mode().to_le_bytes());
        hasher.update(&[entry_kind_tag(kind)]);
        return Some(EntryIdentity::from_bytes(*hasher.finalize().as_bytes()));
    }
    hasher.update(&metadata.len().to_le_bytes());
    hasher.update(&metadata.mode().to_le_bytes());
    hasher.update(&metadata.mtime().to_le_bytes());
    hasher.update(&metadata.mtime_nsec().to_le_bytes());
    hasher.update(&metadata.ctime().to_le_bytes());
    hasher.update(&metadata.ctime_nsec().to_le_bytes());
    hasher.update(&[entry_kind_tag(kind)]);
    Some(EntryIdentity::from_bytes(*hasher.finalize().as_bytes()))
}

#[cfg(unix)]
#[allow(clippy::unnecessary_cast)]
pub(crate) fn stat_identity(stat: &libc::stat, kind: EntryKind) -> Option<EntryIdentity> {
    let dev = stat.st_dev as u64;
    let ino = stat.st_ino as u64;
    let mode = stat.st_mode as u32;

    let mut hasher = blake3::Hasher::new();
    hasher.update(b"sy-entry-identity-v1\0");
    hasher.update(&dev.to_le_bytes());
    hasher.update(&ino.to_le_bytes());
    if kind == EntryKind::Directory {
        hasher.update(&mode.to_le_bytes());
        hasher.update(&[entry_kind_tag(kind)]);
        return Some(EntryIdentity::from_bytes(*hasher.finalize().as_bytes()));
    }
    let len = u64::try_from(stat.st_size).ok()?;
    let mtime = stat.st_mtime as i64;
    let mtime_nsec = stat.st_mtime_nsec as i64;
    let ctime = stat.st_ctime as i64;
    let ctime_nsec = stat.st_ctime_nsec as i64;

    hasher.update(&len.to_le_bytes());
    hasher.update(&mode.to_le_bytes());
    hasher.update(&mtime.to_le_bytes());
    hasher.update(&mtime_nsec.to_le_bytes());
    hasher.update(&ctime.to_le_bytes());
    hasher.update(&ctime_nsec.to_le_bytes());
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

#[cfg(not(unix))]
pub(crate) fn stat_identity(_stat: &(), _kind: EntryKind) -> Option<EntryIdentity> {
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

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn metadata_and_stat_identity_agree() {
        let temp = tempfile::TempDir::new().unwrap();
        let file_path = temp.path().join("file");
        std::fs::write(&file_path, b"test content").unwrap();

        let metadata = std::fs::symlink_metadata(&file_path).unwrap();
        let mut stat = std::mem::MaybeUninit::<libc::stat>::zeroed();
        let c_path = std::ffi::CString::new(file_path.as_os_str().as_encoded_bytes()).unwrap();
        let ret = unsafe { libc::lstat(c_path.as_ptr(), stat.as_mut_ptr()) };
        assert_eq!(ret, 0);
        let stat = unsafe { stat.assume_init() };

        let from_meta = metadata_identity(&metadata, EntryKind::File).unwrap();
        let from_stat = stat_identity(&stat, EntryKind::File).unwrap();
        assert_eq!(from_meta, from_stat);

        let dir_metadata = std::fs::symlink_metadata(temp.path()).unwrap();
        let mut dir_stat = std::mem::MaybeUninit::<libc::stat>::zeroed();
        let dir_c_path =
            std::ffi::CString::new(temp.path().as_os_str().as_encoded_bytes()).unwrap();
        let ret = unsafe { libc::lstat(dir_c_path.as_ptr(), dir_stat.as_mut_ptr()) };
        assert_eq!(ret, 0);
        let dir_stat = unsafe { dir_stat.assume_init() };

        let dir_from_meta = metadata_identity(&dir_metadata, EntryKind::Directory).unwrap();
        let dir_from_stat = stat_identity(&dir_stat, EntryKind::Directory).unwrap();
        assert_eq!(dir_from_meta, dir_from_stat);
    }
}
