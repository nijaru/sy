//! Endpoint publication and verified-existing-destination receipts.
use crate::endpoint::io::VerificationStatus;
use crate::error::{Result, SyncError};
use sy::engine::domain::{Entry, EntryIdentity, RelativePath};

/// A completed transaction bound to independent source/destination addresses.
/// Regular files carry the original staging inode's finalized publication proof.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishedDestinationReceipt {
    source_path: RelativePath,
    destination_path: RelativePath,
    source_identity: Option<EntryIdentity>,
    verification: VerificationStatus,
    publication: sy::rooted_fs::PublishedEntryProof,
}

impl PublishedDestinationReceipt {
    pub(crate) fn for_file(
        source_path: RelativePath,
        destination_path: RelativePath,
        source_identity: Option<EntryIdentity>,
        verification: &VerificationStatus,
        publication: sy::rooted_fs::PublishedFileProof,
    ) -> Self {
        Self {
            source_path,
            destination_path,
            source_identity,
            verification: *verification,
            publication: publication.into(),
        }
    }

    pub(crate) fn for_symlink(
        source_path: RelativePath,
        destination_path: RelativePath,
        source_identity: Option<EntryIdentity>,
        publication: sy::rooted_fs::PublishedEntryProof,
    ) -> Self {
        Self {
            source_path,
            destination_path,
            source_identity,
            verification: VerificationStatus::NotRequested,
            publication,
        }
    }

    pub(crate) fn for_hardlink(
        source_path: RelativePath,
        destination_path: RelativePath,
        source_identity: Option<EntryIdentity>,
        publication: sy::rooted_fs::PublishedEntryProof,
    ) -> Self {
        Self::for_symlink(source_path, destination_path, source_identity, publication)
    }

    pub fn source_path(&self) -> &RelativePath {
        &self.source_path
    }
    pub fn destination_path(&self) -> &RelativePath {
        &self.destination_path
    }
    pub fn source_identity(&self) -> Option<EntryIdentity> {
        self.source_identity
    }
    pub fn required_verification_completed(&self) -> bool {
        !matches!(self.verification, VerificationStatus::Failed { .. })
    }

    pub(crate) fn publication(&self) -> Result<&sy::rooted_fs::PublishedEntryProof> {
        let proof = &self.publication;
        if proof.path != self.destination_path {
            return Err(SyncError::Config(
                "publication proof destination address mismatch".into(),
            ));
        }
        Ok(proof)
    }

    pub(crate) fn revalidate_destination_blocking(
        &self,
        rooted: &sy::rooted_fs::RootedFs,
    ) -> Result<()> {
        self.publication()?
            .revalidate_blocking(rooted)
            .map_err(|error| SyncError::Config(error.to_string()))
    }

    pub(crate) fn validate_source_removal(&self, source: &Entry) -> Result<()> {
        if self.source_path != source.path {
            return Err(SyncError::Config("receipt source address mismatch".into()));
        }
        if self.source_identity.is_none() || self.source_identity != source.identity {
            return Err(SyncError::SourceChanged {
                path: source.path.as_path().to_path_buf(),
            });
        }
        if !self.required_verification_completed() {
            return Err(SyncError::Config("destination verification failed".into()));
        }
        let proof = self.publication()?;
        if proof.kind != source.kind {
            return Err(SyncError::Config(
                "publication proof entry kind mismatch".into(),
            ));
        }
        Ok(())
    }
}

/// Strong content/preservation parity for an existing destination, not quick-check equality.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedExistingDestinationReceipt {
    source_path: RelativePath,
    destination_path: RelativePath,
    source_identity: EntryIdentity,
    destination_identity: EntryIdentity,
}

impl VerifiedExistingDestinationReceipt {
    pub(super) fn new(
        source_path: RelativePath,
        destination_path: RelativePath,
        source_identity: EntryIdentity,
        destination_identity: EntryIdentity,
    ) -> Self {
        Self {
            source_path,
            destination_path,
            source_identity,
            destination_identity,
        }
    }
    pub fn source_path(&self) -> &RelativePath {
        &self.source_path
    }
    pub fn destination_path(&self) -> &RelativePath {
        &self.destination_path
    }
    pub fn source_identity(&self) -> EntryIdentity {
        self.source_identity
    }
    pub fn destination_identity(&self) -> EntryIdentity {
        self.destination_identity
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sy::engine::domain::Timestamp;

    #[test]
    fn native_and_verified_publications_bind_both_addresses_and_source_identity() {
        let mut source = Entry::file(
            RelativePath::new("input").unwrap(),
            10,
            Timestamp::UNIX_EPOCH,
        );
        source.identity = Some(EntryIdentity::from_bytes([1; 32]));
        let proof = sy::rooted_fs::PublishedFileProof {
            path: RelativePath::new("renamed").unwrap(),
            identity: EntryIdentity::from_bytes([2; 32]),
        };
        for verification in [
            VerificationStatus::Verified,
            VerificationStatus::NotRequested,
        ] {
            let receipt = PublishedDestinationReceipt::for_file(
                source.path.clone(),
                proof.path.clone(),
                source.identity,
                &verification,
                proof.clone(),
            );
            assert!(receipt.validate_source_removal(&source).is_ok());
            assert_eq!(receipt.publication().unwrap(), &proof.clone().into());
            let mut changed = source.clone();
            changed.identity = Some(EntryIdentity::from_bytes([3; 32]));
            assert!(receipt.validate_source_removal(&changed).is_err());
            changed = source.clone();
            changed.path = proof.path.clone();
            assert!(receipt.validate_source_removal(&changed).is_err());
        }
    }
}
