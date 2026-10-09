//! Original-descriptor publication authority and finalized observational proofs.
use super::{Result, RootedFs, RootedFsError};
use crate::endpoint::publication::PublicationAdmission;
use crate::engine::domain::{EntryIdentity, EntryKind, RelativePath};
use std::fs::File;
use std::sync::Arc;
#[cfg(unix)]
use {
    super::{identity_from_stat, stat_fd},
    std::os::fd::AsRawFd,
};

/// Original staging descriptor and its observed state after namespace publication.
/// A held descriptor alone must not adopt foreign edits to that same inode.
pub struct RootedPublishedFile {
    file: File,
    rooted: RootedFs,
    path: RelativePath,
    #[cfg(target_os = "macos")]
    admission: Option<Arc<PublicationAdmission>>,
    #[cfg(unix)]
    observed: libc::stat,
}

impl std::fmt::Debug for RootedPublishedFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RootedPublishedFile")
            .field("file", &self.file)
            .field("rooted", &self.rooted)
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

impl RootedPublishedFile {
    pub(super) fn from_committed(
        file: File,
        rooted: RootedFs,
        path: RelativePath,
        _admission: Option<Arc<PublicationAdmission>>,
    ) -> Result<Self> {
        #[cfg(unix)]
        {
            let observed = stat_fd(file.as_raw_fd())
                .map_err(|error| Self::finalization_error(&path, error))?;
            let published = Self {
                file,
                rooted,
                path,
                #[cfg(target_os = "macos")]
                admission: _admission,
                observed,
            };
            published
                .verify_binding()
                .map_err(|error| Self::finalization_error(&published.path, error))?;
            Ok(published)
        }
        #[cfg(not(unix))]
        {
            let _ = (file, rooted, _admission);
            Err(Self::finalization_error(
                &path,
                RootedFsError::UnsupportedPlatform,
            ))
        }
    }

    pub fn finalize_blocking(self, flags: Option<u32>) -> Result<PublishedFileProof> {
        let finalize = || {
            self.verify_binding()?;
            let identity = if let Some(flags) = flags {
                #[cfg(target_os = "macos")]
                {
                    // Namespace admission ended with cleanup. This distinct
                    // metadata mutation needs late admission of its own.
                    let _permits = self
                        .rooted
                        .admit_mutation_with_transaction_blocking(self.admission.as_deref())?;
                    self.verify_binding()?;
                    // SAFETY: the live descriptor is the original staged inode,
                    // checked against its recorded publication state, not a reopen.
                    if unsafe { libc::fchflags(self.file.as_raw_fd(), flags) } != 0 {
                        return Err(RootedFsError::Io(std::io::Error::last_os_error()));
                    }
                    let after = stat_fd(self.file.as_raw_fd())?;
                    let before = &self.observed;
                    // Advance only the requested flags and their native ctime
                    // bookkeeping. This is observational, not syscall isolation
                    // from a foreign ctime-only edit during our own mutation.
                    if after.st_flags != flags
                        || before.st_dev != after.st_dev
                        || before.st_ino != after.st_ino
                        || before.st_mode != after.st_mode
                        || before.st_uid != after.st_uid
                        || before.st_gid != after.st_gid
                        || before.st_rdev != after.st_rdev
                        || before.st_size != after.st_size
                        || before.st_nlink != after.st_nlink
                        || before.st_mtime != after.st_mtime
                        || before.st_mtime_nsec != after.st_mtime_nsec
                    {
                        return Err(RootedFsError::DestinationChanged(
                            self.path.as_path().to_path_buf(),
                        ));
                    }
                    self.verify_observation_binding(&after)?
                }
                #[cfg(not(target_os = "macos"))]
                {
                    let _ = flags;
                    return Err(RootedFsError::UnsupportedPlatform);
                }
            } else {
                self.verify_binding()?
            };
            Ok(identity)
        };
        let identity = finalize().map_err(|error| Self::finalization_error(&self.path, error))?;
        Ok(PublishedFileProof {
            path: self.path,
            identity,
        })
    }

    fn finalization_error(path: &RelativePath, error: RootedFsError) -> RootedFsError {
        RootedFsError::CommittedFinalizationFailed {
            path: path.as_path().to_path_buf(),
            reason: error.to_string(),
        }
    }

    fn verify_binding(&self) -> Result<EntryIdentity> {
        #[cfg(unix)]
        {
            self.verify_observation_binding(&self.observed)
        }
        #[cfg(not(unix))]
        {
            Err(RootedFsError::UnsupportedPlatform)
        }
    }

    #[cfg(unix)]
    fn verify_observation_binding(&self, observation: &libc::stat) -> Result<EntryIdentity> {
        let identity = identity_from_stat(observation)
            .ok_or_else(|| RootedFsError::NotRegularFile(self.path.as_path().to_path_buf()))?;
        if identity_from_stat(&stat_fd(self.file.as_raw_fd())?) != Some(identity)
            || self.rooted.path_identity_blocking(&self.path)? != Some((EntryKind::File, identity))
        {
            return Err(RootedFsError::DestinationChanged(
                self.path.as_path().to_path_buf(),
            ));
        }
        Ok(identity)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishedFileProof {
    pub path: RelativePath,
    pub identity: EntryIdentity,
}

/// Finalized publication of an entry, bound to the original staging authority.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishedEntryProof {
    pub path: RelativePath,
    pub kind: EntryKind,
    pub identity: EntryIdentity,
}

impl From<PublishedFileProof> for PublishedEntryProof {
    fn from(proof: PublishedFileProof) -> Self {
        Self {
            path: proof.path,
            kind: EntryKind::File,
            identity: proof.identity,
        }
    }
}

impl PublishedEntryProof {
    pub fn revalidate_blocking(&self, rooted: &RootedFs) -> Result<()> {
        if rooted.path_identity_blocking(&self.path)? != Some((self.kind, self.identity)) {
            return Err(RootedFsError::DestinationChanged(
                self.path.as_path().to_path_buf(),
            ));
        }
        Ok(())
    }
}

impl PublishedFileProof {
    /// Observational revalidation, not compare-and-swap. Exclusive access is
    /// required to exclude the residual interval before later source unlink.
    pub fn revalidate_blocking(&self, rooted: &RootedFs) -> Result<()> {
        if rooted.path_identity_blocking(&self.path)? != Some((EntryKind::File, self.identity)) {
            return Err(RootedFsError::DestinationChanged(
                self.path.as_path().to_path_buf(),
            ));
        }
        Ok(())
    }
}
