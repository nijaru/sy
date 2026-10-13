//! One read-only remote source observation, retained through publication receipt.
//!
//! The held root is namespace authority even if relocated. These native checks
//! detect observable races; they do not provide snapshot isolation or atomic CAS.

use crate::endpoint::io::Preservation;
use crate::engine::domain::{EntryIdentity, EntryKind, RelativePath};
use crate::rooted_fs::{RootedFs, RootedFsError, RootedSourceFile};
use std::fs::File;
use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub(crate) enum SourceFileError {
    #[error(transparent)]
    Rooted(#[from] RootedFsError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("remote source observation changed since scan: {path:?}")]
    Changed { path: PathBuf },
    #[error("source observation has no data authority")]
    ObservationOnly,
}

pub(crate) type Result<T> = std::result::Result<T, SourceFileError>;

#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct SourceMetadataRequest {
    pub(crate) xattrs: bool,
    pub(crate) acls: bool,
    pub(crate) bsd_flags: bool,
}

impl SourceMetadataRequest {
    pub(crate) fn is_requested(self) -> bool {
        self.xattrs || self.acls || self.bsd_flags
    }
}

/// References the run's original catalogue, not a new stat-based authority.
/// Validation checks all original ancestors, names and effective identities.
pub(crate) struct SourceObservation {
    rooted: RootedFs,
    relative: RelativePath,
    identity: EntryIdentity,
    size: u64,
}

impl SourceObservation {
    fn validate_blocking(&self) -> Result<()> {
        self.validate_member_blocking(&self.relative, self.identity)
    }

    fn validate_member_blocking(
        &self,
        relative: &RelativePath,
        expected: EntryIdentity,
    ) -> Result<()> {
        if !cfg!(unix) {
            return Err(RootedFsError::UnsupportedPlatform.into());
        }
        if expected != self.identity {
            return Err(SourceFileError::Changed {
                path: relative.as_path().to_path_buf(),
            });
        }
        self.validate_name_blocking(&self.relative, self.identity)?;
        if relative != &self.relative {
            self.validate_name_blocking(relative, expected)?;
        }
        Ok(())
    }

    fn validate_name_blocking(
        &self,
        relative: &RelativePath,
        expected: EntryIdentity,
    ) -> Result<()> {
        self.rooted
            .validate_catalogued_source_blocking(relative)
            .map_err(|error| changed(error, relative))?;
        if self.rooted.path_identity_blocking(relative)? != Some((EntryKind::File, expected)) {
            return Err(SourceFileError::Changed {
                path: relative.as_path().to_path_buf(),
            });
        }
        Ok(())
    }
}

fn changed(error: RootedFsError, relative: &RelativePath) -> SourceFileError {
    match error {
        RootedFsError::DestinationChanged(_) | RootedFsError::SourceBindingChanged(_) => {
            SourceFileError::Changed {
                path: relative.as_path().to_path_buf(),
            }
        }
        error => error.into(),
    }
}

/// Group bookkeeping can be readless; bytes and requested native preservation
/// always require a genuine original reader. Neither variant authorizes removal.
pub(crate) enum GroupSource {
    Observation(SourceObservation),
    Data(SourceFile),
}

impl GroupSource {
    pub(crate) fn capture_blocking(
        rooted: RootedFs,
        relative: RelativePath,
        identity: EntryIdentity,
        size: u64,
        require_data: bool,
        request: SourceMetadataRequest,
    ) -> Result<Self> {
        if require_data || request.is_requested() {
            return SourceFile::capture_blocking(rooted, relative, identity, size).map(Self::Data);
        }
        let observation = SourceObservation {
            rooted,
            relative,
            identity,
            size,
        };
        observation.validate_blocking()?;
        Ok(Self::Observation(observation))
    }

    pub(crate) fn validate_blocking(&self) -> Result<()> {
        match self {
            Self::Observation(observation) => observation.validate_blocking(),
            Self::Data(file) => file.validate_blocking(),
        }
    }

    pub(crate) fn validate_member_blocking(
        &self,
        relative: &RelativePath,
        expected: EntryIdentity,
    ) -> Result<()> {
        match self {
            Self::Observation(observation) => {
                observation.validate_member_blocking(relative, expected)
            }
            Self::Data(file) => file.validate_member_blocking(relative, expected),
        }
    }

    pub(crate) fn clone_reader_blocking(&self) -> Result<File> {
        match self {
            Self::Data(file) => file.clone_reader_blocking(),
            Self::Observation(_) => Err(SourceFileError::ObservationOnly),
        }
    }

    pub(crate) fn read_metadata_blocking(
        &self,
        request: SourceMetadataRequest,
    ) -> Result<(Preservation, Option<u32>)> {
        match self {
            Self::Data(file) => file.read_metadata_blocking(request),
            Self::Observation(_) if request.is_requested() => Err(SourceFileError::ObservationOnly),
            Self::Observation(observation) => {
                observation.validate_blocking()?;
                Ok((Preservation::default(), None))
            }
        }
    }
}

/// Owns the original regular-file FD and opening parent. Acquisition remains
/// mandatory-read for every caller; observation-only work uses GroupSource.
pub(crate) struct SourceFile {
    observation: SourceObservation,
    member: RootedSourceFile,
}

impl SourceFile {
    pub(crate) fn capture_blocking(
        rooted: RootedFs,
        relative: RelativePath,
        identity: EntryIdentity,
        size: u64,
    ) -> Result<Self> {
        if !cfg!(unix) {
            return Err(RootedFsError::UnsupportedPlatform.into());
        }
        // RootedFs opens O_RDONLY, no-follow, through its held root directory.
        let member = rooted
            .capture_source_file_blocking(&relative, identity)
            .map_err(|error| changed(error, &relative))?;
        let source = Self {
            observation: SourceObservation {
                rooted,
                relative,
                identity,
                size,
            },
            member,
        };
        source.validate_blocking()?;
        Ok(source)
    }

    pub(crate) fn validate_blocking(&self) -> Result<()> {
        self.validate_member_blocking(&self.observation.relative, self.observation.identity)
    }

    /// Check a declared source name against its supplied scan identity AND the
    /// original held file. Same-inode group callers never adopt fresh metadata.
    pub(crate) fn validate_member_blocking(
        &self,
        relative: &RelativePath,
        expected: EntryIdentity,
    ) -> Result<()> {
        self.validate_owner_blocking()?;
        self.observation
            .validate_member_blocking(relative, expected)?;
        self.validate_owner_blocking()
    }

    fn validate_owner_blocking(&self) -> Result<()> {
        if !cfg!(unix) {
            return Err(RootedFsError::UnsupportedPlatform.into());
        }
        let metadata = self.member.file().metadata()?;
        let observation = &self.observation;
        if !metadata.is_file()
            || metadata.len() != observation.size
            || crate::endpoint::local_identity::metadata_identity(&metadata, EntryKind::File)
                != Some(observation.identity)
        {
            return Err(SourceFileError::Changed {
                path: observation.relative.as_path().to_path_buf(),
            });
        }
        observation
            .rooted
            .validate_source_file_parent_blocking(&observation.relative, &self.member)
            .map_err(|error| changed(error, &observation.relative))
    }

    /// Only the byte producer may read/seek this duplicate. It shares the
    /// ORIGINAL open-file description; the carrier only stats/reads metadata.
    pub(crate) fn clone_reader_blocking(&self) -> Result<File> {
        self.validate_blocking()?;
        Ok(self.member.file().try_clone()?)
    }

    /// Bounded, demand-driven native metadata from the original content FD.
    pub(crate) fn read_metadata_blocking(
        &self,
        request: SourceMetadataRequest,
    ) -> Result<(Preservation, Option<u32>)> {
        self.validate_blocking()?;
        let xattrs = if request.xattrs {
            let value = self
                .observation
                .rooted
                .read_open_file_xattrs_blocking(self.member.file(), &self.observation.relative)?;
            self.validate_blocking()?;
            Some(value)
        } else {
            None
        };
        let acl = if request.acls {
            let value = self
                .observation
                .rooted
                .read_open_file_acl_blocking(self.member.file(), &self.observation.relative)?
                .unwrap_or_default();
            self.validate_blocking()?;
            Some(value)
        } else {
            None
        };
        let bsd_flags = if request.bsd_flags {
            let value = self
                .observation
                .rooted
                .read_open_file_bsd_flags_blocking(self.member.file())?;
            self.validate_blocking()?;
            Some(value)
        } else {
            None
        };
        Ok((Preservation { xattrs, acl }, bsd_flags))
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::io::{Read, Seek, Write};
    use std::os::unix::fs::MetadataExt;
    use std::path::Path;

    fn relative(path: &str) -> RelativePath {
        RelativePath::new(PathBuf::from(path)).unwrap()
    }

    fn capture(root: &Path, name: &str) -> SourceFile {
        let rooted = RootedFs::open_blocking_for_worker(root.to_path_buf()).unwrap();
        crate::rooted_fs::test_source::scan_blocking(&rooted);
        let path = relative(name);
        let (_, identity) = rooted.path_identity_blocking(&path).unwrap().unwrap();
        let size = std::fs::metadata(root.join(name)).unwrap().len();
        SourceFile::capture_blocking(rooted, path, identity, size).unwrap()
    }

    #[test]
    fn readless_group_observation_has_no_reader_or_optional_metadata_authority() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("file"), b"original").unwrap();
        std::fs::set_permissions(
            root.path().join("file"),
            std::fs::Permissions::from_mode(0o000),
        )
        .unwrap();
        let rooted = RootedFs::open_blocking_for_worker(root.path().to_path_buf()).unwrap();
        let path = relative("file");
        let (_, identity) = rooted.path_identity_blocking(&path).unwrap().unwrap();
        assert!(matches!(
            GroupSource::capture_blocking(
                rooted.clone(),
                path.clone(),
                identity,
                8,
                false,
                SourceMetadataRequest::default()
            ),
            Err(SourceFileError::Rooted(RootedFsError::SourceUnobserved(_)))
        ));
        crate::rooted_fs::test_source::scan_blocking(&rooted);
        let source = GroupSource::capture_blocking(
            rooted.clone(),
            path.clone(),
            identity,
            8,
            false,
            SourceMetadataRequest::default(),
        )
        .unwrap();
        source.validate_blocking().unwrap();
        assert!(matches!(
            source.clone_reader_blocking(),
            Err(SourceFileError::ObservationOnly)
        ));
        assert!(matches!(
            source.read_metadata_blocking(SourceMetadataRequest {
                xattrs: true,
                ..Default::default()
            }),
            Err(SourceFileError::ObservationOnly)
        ));
        let (metadata, flags) = source
            .read_metadata_blocking(SourceMetadataRequest::default())
            .unwrap();
        assert_eq!(metadata.xattrs, None);
        assert_eq!(metadata.acl, None);
        assert_eq!(flags, None);
        // SourceFile's existing callers never acquire a read waiver.
        assert!(SourceFile::capture_blocking(rooted.clone(), path.clone(), identity, 8).is_err());
        for (required, request) in [
            (true, SourceMetadataRequest::default()),
            (
                false,
                SourceMetadataRequest {
                    xattrs: true,
                    ..Default::default()
                },
            ),
            (
                false,
                SourceMetadataRequest {
                    acls: true,
                    ..Default::default()
                },
            ),
            (
                false,
                SourceMetadataRequest {
                    bsd_flags: true,
                    ..Default::default()
                },
            ),
        ] {
            assert!(GroupSource::capture_blocking(
                rooted.clone(),
                path.clone(),
                identity,
                8,
                required,
                request
            )
            .is_err());
        }
    }

    #[test]
    fn readless_group_rejects_foreign_parent_with_unchanged_original_inode() {
        use std::os::unix::fs::PermissionsExt;
        for symlink_parent in [false, true] {
            let root = tempfile::tempdir().unwrap();
            for dir in ["selected", "alternate"] {
                std::fs::create_dir(root.path().join(dir)).unwrap();
            }
            std::fs::write(root.path().join("selected/file"), b"original").unwrap();
            std::fs::hard_link(
                root.path().join("selected/file"),
                root.path().join("alternate/file"),
            )
            .unwrap();
            std::fs::set_permissions(
                root.path().join("selected/file"),
                std::fs::Permissions::from_mode(0o000),
            )
            .unwrap();
            let rooted = RootedFs::open_blocking_for_worker(root.path().to_path_buf()).unwrap();
            crate::rooted_fs::test_source::scan_blocking(&rooted);
            let path = relative("selected/file");
            let (_, identity) = rooted.path_identity_blocking(&path).unwrap().unwrap();
            let source = GroupSource::capture_blocking(
                rooted,
                path,
                identity,
                8,
                false,
                SourceMetadataRequest::default(),
            )
            .unwrap();
            std::fs::rename(root.path().join("selected"), root.path().join("held")).unwrap();
            if symlink_parent {
                std::os::unix::fs::symlink("held", root.path().join("selected")).unwrap();
            } else {
                std::fs::rename(root.path().join("alternate"), root.path().join("selected"))
                    .unwrap();
            }
            assert_eq!(
                crate::endpoint::local_identity::metadata_identity(
                    &std::fs::metadata(root.path().join("selected/file")).unwrap(),
                    EntryKind::File
                ),
                Some(identity)
            );
            assert!(source.validate_blocking().is_err());
        }
    }

    #[test]
    fn original_read_only_description_survives_producer_clone_and_root_relocation() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("root");
        let moved = temp.path().join("moved");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("file"), b"original").unwrap();
        let source = capture(&root, "file");
        let mut reader = source.clone_reader_blocking().unwrap();
        let error = reader.write_all(b"must not write").unwrap_err();
        assert_eq!(error.raw_os_error(), Some(libc::EBADF));
        let mut prefix = [0; 3];
        reader.read_exact(&mut prefix).unwrap();
        assert_eq!(&prefix, b"ori");
        // dup, not reopen: the offset advances on the original description.
        assert_eq!(source.member.file().stream_position().unwrap(), 3);
        drop(reader);
        std::fs::rename(&root, &moved).unwrap();
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("file"), b"foreign!").unwrap();
        source.validate_blocking().unwrap();
        let mut rest = Vec::new();
        source
            .clone_reader_blocking()
            .unwrap()
            .read_to_end(&mut rest)
            .unwrap();
        assert_eq!(rest, b"ginal");
        assert_eq!(std::fs::read(moved.join("file")).unwrap(), b"original");
    }

    #[test]
    fn rejects_wrong_held_fd_even_when_original_name_still_matches() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("file"), b"same bytes").unwrap();
        std::fs::write(root.path().join("other"), b"same bytes").unwrap();
        let mut source = capture(root.path(), "file");
        source.member = capture(root.path(), "other").member;
        assert!(matches!(
            source.validate_blocking(),
            Err(SourceFileError::Changed { .. })
        ));
    }

    #[test]
    fn foreign_parent_with_the_original_inode_cannot_replace_the_opening_binding() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("selected")).unwrap();
        std::fs::create_dir(root.path().join("alternate")).unwrap();
        std::fs::write(root.path().join("selected/file"), b"original").unwrap();
        std::fs::hard_link(
            root.path().join("selected/file"),
            root.path().join("alternate/file"),
        )
        .unwrap();
        let source = capture(root.path(), "selected/file");
        let original = source.observation.identity;
        std::fs::rename(root.path().join("selected"), root.path().join("held")).unwrap();
        std::fs::rename(root.path().join("alternate"), root.path().join("selected")).unwrap();
        assert_eq!(
            source
                .observation
                .rooted
                .path_identity_blocking(&source.observation.relative)
                .unwrap(),
            Some((EntryKind::File, original)),
            "the named inode observation alone still passes"
        );
        assert_eq!(
            crate::endpoint::local_identity::metadata_identity(
                &source.member.file().metadata().unwrap(),
                EntryKind::File,
            ),
            Some(original)
        );
        assert!(matches!(
            source.validate_blocking(),
            Err(SourceFileError::Changed { .. })
        ));
        assert!(source.clone_reader_blocking().is_err());
        assert_eq!(
            std::fs::read(root.path().join("held/file")).unwrap(),
            b"original"
        );
    }

    #[test]
    fn exact_original_and_member_names_are_checked_without_adopting_fresh_stats() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("dir")).unwrap();
        std::fs::write(root.path().join("file"), b"original").unwrap();
        std::fs::hard_link(root.path().join("file"), root.path().join("dir/member")).unwrap();
        std::fs::write(root.path().join("other"), b"original").unwrap();
        let source = capture(root.path(), "file");
        source
            .validate_member_blocking(&relative("dir/member"), source.observation.identity)
            .unwrap();
        let (_, other) = source
            .observation
            .rooted
            .path_identity_blocking(&relative("other"))
            .unwrap()
            .unwrap();
        assert!(matches!(
            source.validate_member_blocking(&relative("other"), other),
            Err(SourceFileError::Changed { .. })
        ));
        // Swap an ancestor, not a leaf: the original inode observation remains
        // EXACTLY unchanged, so FD-only validation cannot detect this race.
        std::fs::rename(root.path().join("dir"), root.path().join("old-dir")).unwrap();
        std::fs::create_dir(root.path().join("dir")).unwrap();
        std::fs::write(root.path().join("dir/member"), b"original").unwrap();
        source.validate_blocking().unwrap();
        assert!(matches!(
            source.validate_member_blocking(&relative("dir/member"), source.observation.identity),
            Err(SourceFileError::Changed { .. })
        ));

        let nested = capture(root.path(), "old-dir/member");
        std::fs::rename(
            root.path().join("old-dir"),
            root.path().join("relocated-dir"),
        )
        .unwrap();
        std::fs::create_dir(root.path().join("old-dir")).unwrap();
        std::fs::write(root.path().join("old-dir/member"), b"original").unwrap();
        assert_eq!(
            crate::endpoint::local_identity::metadata_identity(
                &nested.member.file().metadata().unwrap(),
                EntryKind::File,
            ),
            Some(nested.observation.identity)
        );
        assert!(matches!(
            nested.validate_blocking(),
            Err(SourceFileError::Changed { .. })
        ));
        assert!(matches!(
            nested.read_metadata_blocking(SourceMetadataRequest::default()),
            Err(SourceFileError::Rooted(
                RootedFsError::SourceCatalogueTerminal
            ))
        ));
    }

    #[test]
    fn requested_native_metadata_is_read_only_and_demand_driven() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("file");
        std::fs::write(&path, b"original").unwrap();
        xattr::set(&path, "user.sy-source-lease", b"value").unwrap();
        let source = capture(root.path(), "file");
        let before = source.member.file().metadata().unwrap();
        let (empty, flags) = source
            .read_metadata_blocking(SourceMetadataRequest::default())
            .unwrap();
        assert!(empty.is_empty());
        assert_eq!(flags, None);
        let (metadata, flags) = source
            .read_metadata_blocking(SourceMetadataRequest {
                xattrs: true,
                acls: cfg!(feature = "acl"),
                bsd_flags: cfg!(target_os = "macos"),
            })
            .unwrap();
        assert!(metadata
            .xattrs
            .unwrap()
            .iter()
            .any(|(name, value)| name == "user.sy-source-lease" && value == b"value"));
        assert_eq!(metadata.acl.is_some(), cfg!(feature = "acl"));
        #[cfg(target_os = "macos")]
        {
            use std::os::macos::fs::MetadataExt;
            assert_eq!(flags, Some(before.st_flags()));
        }
        #[cfg(not(target_os = "macos"))]
        {
            assert_eq!(flags, None);
            assert!(matches!(
                source.read_metadata_blocking(SourceMetadataRequest {
                    bsd_flags: true,
                    ..Default::default()
                }),
                Err(SourceFileError::Rooted(RootedFsError::UnsupportedPlatform))
            ));
        }
        source.validate_blocking().unwrap();
        let after = source.member.file().metadata().unwrap();
        assert_eq!(
            (
                before.mode(),
                before.mtime(),
                before.mtime_nsec(),
                before.ctime(),
                before.ctime_nsec()
            ),
            (
                after.mode(),
                after.mtime(),
                after.mtime_nsec(),
                after.ctime(),
                after.ctime_nsec()
            )
        );
        assert_eq!(std::fs::read(path).unwrap(), b"original");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn native_flags_getter_does_not_reopen_replaced_source_name() {
        use std::os::fd::AsRawFd;
        use std::os::macos::fs::MetadataExt;
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("file");
        std::fs::write(&path, b"original").unwrap();
        let original = File::open(&path).unwrap();
        // SAFETY: setup owns a live regular-file FD; UF_HIDDEN is reversible
        // and does not block cleanup. No transfer code mutates flags.
        assert_eq!(
            unsafe { libc::fchflags(original.as_raw_fd(), libc::UF_HIDDEN) },
            0
        );
        let source = capture(root.path(), "file");
        std::fs::rename(&path, root.path().join("old")).unwrap();
        std::fs::write(&path, b"foreign!").unwrap();
        assert_ne!(
            original.metadata().unwrap().st_flags(),
            std::fs::metadata(&path).unwrap().st_flags()
        );
        assert_eq!(
            source
                .observation
                .rooted
                .read_open_file_bsd_flags_blocking(source.member.file())
                .unwrap(),
            original.metadata().unwrap().st_flags()
        );
        assert!(source.validate_blocking().is_err());
    }
}

impl From<SourceFileError> for crate::remote::transfer::RemoteTransferError {
    fn from(error: SourceFileError) -> Self {
        // Keep the private typed cause without expanding the shared wire/transfer
        // error vocabulary owned by the transfer layer.
        Self::Io(std::io::Error::other(error))
    }
}
