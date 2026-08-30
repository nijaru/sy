use std::borrow::Borrow;
use std::ffi::OsStr;
use std::fmt;
use std::path::{Component, Path, PathBuf};

/// A canonical, non-empty path relative to an endpoint root.
///
/// This type prevents absolute paths and lexical traversal from entering the
/// engine. Endpoint implementations still own secure rooted resolution; this
/// invariant is not a substitute for directory-FD/openat-style confinement.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RelativePath(PathBuf);

impl RelativePath {
    pub fn new(path: impl Into<PathBuf>) -> Result<Self, InvalidRelativePath> {
        let path = path.into();
        validate_relative_path(&path)?;
        Ok(Self(path))
    }

    pub fn as_path(&self) -> &Path {
        &self.0
    }

    pub fn into_path_buf(self) -> PathBuf {
        self.0
    }

    pub fn parent(&self) -> Option<Self> {
        let parent = self.0.parent()?;
        if parent.as_os_str().is_empty() {
            None
        } else {
            Some(Self(parent.to_path_buf()))
        }
    }
}

impl fmt::Debug for RelativePath {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("RelativePath")
            .field(&self.0)
            .finish()
    }
}

impl fmt::Display for RelativePath {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.display().fmt(formatter)
    }
}

impl AsRef<Path> for RelativePath {
    fn as_ref(&self) -> &Path {
        self.as_path()
    }
}

impl Borrow<Path> for RelativePath {
    fn borrow(&self) -> &Path {
        self.as_path()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum InvalidRelativePath {
    #[error("relative path is empty")]
    Empty,
    #[error("relative path contains an absolute/root component")]
    Absolute,
    #[error("relative path contains a parent-directory component")]
    ParentTraversal,
    #[error("relative path contains a current-directory component")]
    CurrentDirectory,
    #[error("relative path contains an unsupported platform prefix")]
    Prefix,
}

fn validate_relative_path(path: &Path) -> Result<(), InvalidRelativePath> {
    if path.as_os_str().is_empty() {
        return Err(InvalidRelativePath::Empty);
    }

    for component in path.components() {
        match component {
            Component::Normal(_) => {}
            Component::ParentDir => return Err(InvalidRelativePath::ParentTraversal),
            Component::CurDir => return Err(InvalidRelativePath::CurrentDirectory),
            Component::RootDir => return Err(InvalidRelativePath::Absolute),
            Component::Prefix(_) => return Err(InvalidRelativePath::Prefix),
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Timestamp {
    seconds: i64,
    nanoseconds: u32,
}

impl Timestamp {
    pub const UNIX_EPOCH: Self = Self {
        seconds: 0,
        nanoseconds: 0,
    };

    pub fn new(seconds: i64, nanoseconds: u32) -> Result<Self, InvalidTimestamp> {
        if nanoseconds >= 1_000_000_000 {
            return Err(InvalidTimestamp { nanoseconds });
        }
        Ok(Self {
            seconds,
            nanoseconds,
        })
    }

    pub const fn seconds(self) -> i64 {
        self.seconds
    }

    pub const fn nanoseconds(self) -> u32 {
        self.nanoseconds
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("timestamp nanoseconds must be below 1,000,000,000, got {nanoseconds}")]
pub struct InvalidTimestamp {
    nanoseconds: u32,
}

/// Opaque endpoint-owned identity for detecting scan/open/commit races.
///
/// The engine compares identity bytes for equality but does not interpret them.
/// A local endpoint can derive them from stable stat fields; a remote endpoint
/// can issue a server-side token and validate it again when opening a file.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct EntryIdentity([u8; 32]);

impl EntryIdentity {
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl fmt::Debug for EntryIdentity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("EntryIdentity(<opaque>)")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EntryKind {
    File,
    Directory,
    Symlink,
}

/// Lean metadata required for ordered reconciliation.
///
/// Expensive preservation data such as xattrs, ACLs, sparse extents, and block
/// signatures deliberately do not live here. Those are requested on demand
/// after reconciliation/planning says they are needed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub path: RelativePath,
    pub kind: EntryKind,
    pub size: u64,
    pub modified: Timestamp,
    pub unix_mode: Option<u32>,
    pub symlink_target: Option<PathBuf>,
    pub identity: Option<EntryIdentity>,
    pub hardlink_group: Option<EntryIdentity>,
}

impl Entry {
    pub fn file(path: RelativePath, size: u64, modified: Timestamp) -> Self {
        Self {
            path,
            kind: EntryKind::File,
            size,
            modified,
            unix_mode: None,
            symlink_target: None,
            identity: None,
            hardlink_group: None,
        }
    }

    pub fn directory(path: RelativePath, modified: Timestamp) -> Self {
        Self {
            path,
            kind: EntryKind::Directory,
            size: 0,
            modified,
            unix_mode: None,
            symlink_target: None,
            identity: None,
            hardlink_group: None,
        }
    }

    pub fn symlink(path: RelativePath, target: PathBuf, modified: Timestamp) -> Self {
        Self {
            path,
            kind: EntryKind::Symlink,
            size: 0,
            modified,
            unix_mode: None,
            symlink_target: Some(target),
            identity: None,
            hardlink_group: None,
        }
    }

    pub const fn is_file(&self) -> bool {
        matches!(self.kind, EntryKind::File)
    }

    pub const fn is_directory(&self) -> bool {
        matches!(self.kind, EntryKind::Directory)
    }

    pub const fn is_symlink(&self) -> bool {
        matches!(self.kind, EntryKind::Symlink)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipReason {
    Unchanged,
    Filtered,
    ExistingOnly,
    MissingDestination,
    DestinationNewer,
}

/// Semantic result of reconciliation. No byte-transfer strategy appears here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncOp {
    Create {
        source: Entry,
    },
    Update {
        source: Entry,
        destination: Entry,
    },
    Replace {
        source: Entry,
        destination: Entry,
    },
    Metadata {
        source: Entry,
        destination: Entry,
    },
    Skip {
        path: RelativePath,
        reason: SkipReason,
    },
}

impl SyncOp {
    pub fn path(&self) -> &RelativePath {
        match self {
            Self::Create { source }
            | Self::Update { source, .. }
            | Self::Replace { source, .. }
            | Self::Metadata { source, .. } => &source.path,
            Self::Skip { path, .. } => path,
        }
    }
}

/// Converts platform path bytes without requiring UTF-8. This helper is kept at
/// the domain boundary because protocol and endpoint adapters both need the
/// exact same path semantics.
#[cfg(unix)]
pub fn os_str_bytes(value: &OsStr) -> &[u8] {
    use std::os::unix::ffi::OsStrExt;
    value.as_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_paths_reject_escape_components() {
        assert_eq!(RelativePath::new(""), Err(InvalidRelativePath::Empty));
        assert_eq!(
            RelativePath::new("../file"),
            Err(InvalidRelativePath::ParentTraversal)
        );
        assert_eq!(
            RelativePath::new("dir/../file"),
            Err(InvalidRelativePath::ParentTraversal)
        );
        assert_eq!(
            RelativePath::new("./file"),
            Err(InvalidRelativePath::CurrentDirectory)
        );
    }

    #[cfg(unix)]
    #[test]
    fn relative_paths_preserve_non_utf8_names() {
        use std::os::unix::ffi::OsStringExt;

        let raw = std::ffi::OsString::from_vec(vec![b'f', 0x80, b'o']);
        let path = RelativePath::new(PathBuf::from(raw)).unwrap();
        assert_eq!(os_str_bytes(path.as_path().as_os_str()), [b'f', 0x80, b'o']);
    }

    #[test]
    fn timestamp_validates_fraction() {
        assert_eq!(
            Timestamp::new(1, 1_000_000_000),
            Err(InvalidTimestamp {
                nanoseconds: 1_000_000_000
            })
        );
        assert_eq!(Timestamp::new(-1, 999_999_999).unwrap().seconds(), -1);
    }

    #[test]
    fn sync_operation_exposes_semantic_path() {
        let path = RelativePath::new("file").unwrap();
        let entry = Entry::file(path.clone(), 4, Timestamp::UNIX_EPOCH);
        let operation = SyncOp::Create { source: entry };
        assert_eq!(operation.path(), &path);
    }
}
