//! Run-owned local source authority, retained from scan through final removal.
use super::local::LocalEndpoint;
use crate::engine::domain::RelativePath;
use crate::engine::reconcile::EntryStream;
use crate::engine::scan::ScanRequest;
use crate::rooted_fs::{self, RootedFs};
use std::path::{Path, PathBuf};

/// Clones share the original root descriptor, never re-resolve its pathname.
#[derive(Debug, Clone)]
pub struct SourceRoot {
    rooted: RootedFs,
}

/// Cancellation/panic cutoff is nonblocking even while native SOURCE replay
/// holds the catalogue lock. It never substitutes for an owned work join.
pub(crate) struct SourceRunAttempt {
    cancelled: std::sync::Arc<std::sync::atomic::AtomicBool>,
    finished: bool,
}
impl SourceRunAttempt {
    pub(crate) fn finish(mut self) {
        self.finished = true;
    }
}
impl Drop for SourceRunAttempt {
    fn drop(&mut self) {
        if !self.finished {
            self.cancelled
                .store(true, std::sync::atomic::Ordering::Release);
        }
    }
}

impl SourceRoot {
    pub(crate) fn attempt(&self) -> rooted_fs::Result<SourceRunAttempt> {
        Ok(SourceRunAttempt {
            cancelled: self.rooted.source_run_cancellation(false)?,
            finished: false,
        })
    }

    /// Arm only a cancellation cutoff for a retained failed owner. This does
    /// not make its DATA observations usable or authorize retry by itself.
    pub(crate) fn retry_attempt(&self) -> rooted_fs::Result<SourceRunAttempt> {
        Ok(SourceRunAttempt {
            cancelled: self.rooted.source_run_cancellation(true)?,
            finished: false,
        })
    }

    /// Retain the authority already captured during operand classification.
    pub fn from_rooted(rooted: RootedFs) -> rooted_fs::Result<Self> {
        if !rooted.is_local_source_authority() {
            return Err(rooted_fs::RootedFsError::SourceOperatorUncaptured);
        }
        Ok(Self { rooted })
    }

    pub async fn open(path: PathBuf) -> rooted_fs::Result<Self> {
        tokio::task::spawn_blocking(move || Self::open_blocking(path))
            .await
            .map_err(|error| rooted_fs::RootedFsError::Worker(error.to_string()))?
    }

    /// Blocking root acquisition for native workers and synchronous callers.
    pub fn open_blocking(path: PathBuf) -> rooted_fs::Result<Self> {
        Self::from_rooted(RootedFs::open_local_source_blocking(path)?)
    }

    /// Start a new observation run through the original acquired operator
    /// authority. An unscanned classification is consumed unchanged; completed
    /// runs get fresh child observations, never fresh operator provenance.
    pub async fn begin_run(&self) -> rooted_fs::Result<Self> {
        let root = self.clone();
        tokio::task::spawn_blocking(move || {
            Self::from_rooted(root.rooted.next_source_run_blocking()?)
        })
        .await
        .map_err(|error| rooted_fs::RootedFsError::Worker(error.to_string()))?
    }

    /// Explicit new observation attempt after a classified source race. This
    /// never revives the failed owner or reacquires the operator operand.
    pub async fn retry_after_source_change(&self) -> rooted_fs::Result<Self> {
        let root = self.clone();
        tokio::task::spawn_blocking(move || {
            Self::from_rooted(root.rooted.retry_source_run_blocking()?)
        })
        .await
        .map_err(|error| rooted_fs::RootedFsError::Worker(error.to_string()))?
    }

    pub(crate) fn same_run(&self, other: &Self) -> bool {
        self.rooted.same_source_run(&other.rooted)
    }

    pub(crate) async fn retire_failed_run(&self, source_race: bool) -> rooted_fs::Result<()> {
        let root = self.clone();
        tokio::task::spawn_blocking(move || {
            root.rooted.retire_failed_source_run_blocking(source_race)
        })
        .await
        .map_err(|error| rooted_fs::RootedFsError::Worker(error.to_string()))?
    }

    pub fn path(&self) -> &Path {
        self.rooted.root_path()
    }

    pub fn rooted(&self) -> RootedFs {
        self.rooted.clone()
    }

    /// A narrow clone for deferred unlink. Reads retain the original authority
    /// and remain valid after cutoff; no destination lineage is imported.
    pub(crate) fn for_removal(
        &self,
        admission: std::sync::Arc<super::publication::PublicationAdmission>,
    ) -> Self {
        let mut rooted = self.rooted();
        rooted.bind_session_mutations(admission, false);
        Self { rooted }
    }

    pub(crate) fn with_run_admission(
        mut self,
        admission: std::sync::Arc<super::publication::PublicationAdmission>,
    ) -> Self {
        self.rooted.bind_run_mutations(admission);
        self
    }

    pub fn validate_blocking(&self) -> rooted_fs::Result<()> {
        self.rooted.verify_root_path_blocking()
    }

    pub async fn validate(&self) -> rooted_fs::Result<()> {
        let root = self.clone();
        tokio::task::spawn_blocking(move || root.validate_blocking())
            .await
            .map_err(|error| rooted_fs::RootedFsError::Worker(error.to_string()))?
    }

    pub fn endpoint(&self) -> LocalEndpoint {
        LocalEndpoint::new(self.path().to_path_buf()).with_rooted_authority(self.rooted())
    }

    pub fn entries(&self, request: ScanRequest) -> EntryStream {
        super::local_entry_scan::source_entry_stream(self.clone(), request)
    }

    pub fn selected_entries(
        &self,
        path: RelativePath,
        request: ScanRequest,
        missing_allowed: bool,
    ) -> EntryStream {
        super::local_entry_scan::selected_source_stream(
            self.clone(),
            path,
            request,
            missing_allowed,
        )
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::endpoint::{existing, Endpoint};
    use crate::rooted_fs::RootedFsError;
    use futures::StreamExt;

    #[tokio::test]
    async fn unchanged_source_root_under_foreign_operator_ancestor_is_not_original_binding() {
        use std::os::unix::fs::MetadataExt;
        let fixture = tempfile::tempdir().unwrap();
        let ancestor = fixture.path().join("ancestor");
        let parent = ancestor.join("parent");
        let path = parent.join("source");
        std::fs::create_dir_all(&path).unwrap();
        std::fs::write(path.join("file"), b"original SOURCE").unwrap();
        let root = SourceRoot::open(path.clone()).await.unwrap();
        let entries: Vec<_> = root
            .entries(Default::default())
            .map(|entry| entry.unwrap())
            .collect()
            .await;
        assert_eq!(entries.len(), 1);
        let original = std::fs::metadata(&path).unwrap();
        let original_parent = std::fs::metadata(&parent).unwrap();
        let parked = fixture.path().join("original-ancestor");
        std::fs::rename(&ancestor, &parked).unwrap();
        std::fs::create_dir(&ancestor).unwrap();
        std::fs::rename(parked.join("parent"), &parent).unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().ino(), original.ino());
        assert_eq!(
            std::fs::metadata(&parent).unwrap().ino(),
            original_parent.ino()
        );
        assert!(matches!(root.validate().await,
            Err(RootedFsError::RootChanged(changed)) if changed == path));
        assert!(root
            .endpoint()
            .open_native_file(Path::new("file"))
            .await
            .is_err());
        assert_eq!(
            std::fs::read(path.join("file")).unwrap(),
            b"original SOURCE"
        );
    }

    #[tokio::test]
    async fn persistent_root_replacement_preserves_original_authority_despite_identical_leaves() {
        let parent = tempfile::tempdir().unwrap();
        let original = parent.path().join("source");
        let replacement = parent.path().join("replacement");
        let held = parent.path().join("held-source");
        std::fs::create_dir(&original).unwrap();
        std::fs::create_dir(&replacement).unwrap();
        std::fs::write(original.join("file"), b"same").unwrap();
        std::fs::hard_link(original.join("file"), replacement.join("file")).unwrap();
        let root = SourceRoot::open(original.clone()).await.unwrap();
        let endpoint = root.endpoint();
        let entries: Vec<_> = root
            .entries(Default::default())
            .map(|entry| entry.unwrap())
            .collect()
            .await;
        assert_eq!(endpoint.metadata(Path::new("file")).await.unwrap().size, 4);
        let source = entries[0].clone();
        std::fs::rename(&original, &held).unwrap();
        std::fs::rename(&replacement, &original).unwrap();
        for path in [held.join("file"), original.join("file")] {
            assert_eq!(
                crate::endpoint::local_identity::metadata_identity(
                    &std::fs::metadata(path).unwrap(),
                    source.kind,
                ),
                source.identity
            );
        }
        assert!(
            matches!(root.validate().await, Err(RootedFsError::RootChanged(path)) if path == original)
        );
        for error in [
            endpoint
                .open_native_file(source.path.as_path())
                .await
                .unwrap_err(),
            endpoint.metadata(source.path.as_path()).await.unwrap_err(),
            endpoint
                .read_xattrs(source.path.as_path())
                .await
                .unwrap_err(),
        ] {
            let crate::error::SyncError::Io(error) = error else {
                panic!("source root change lost its typed cause: {error:?}");
            };
            assert!(
                matches!(error.get_ref().and_then(|cause| cause.downcast_ref::<RootedFsError>()),
                Some(RootedFsError::RootChanged(path)) if path == &original)
            );
        }
        assert!(matches!(
            existing::fingerprint(root.rooted(), source.clone(), Default::default()).await,
            Err(existing::ExistingDestinationError::Rooted(
                RootedFsError::RootChanged(_)
            ))
        ));
        assert!(matches!(
            existing::remove_observed_source_blocking(&root.rooted(), &source),
            Err(existing::ExistingDestinationError::Rooted(
                RootedFsError::RootChanged(_)
            ))
        ));
        // Both source producers reject the replacement before emitting a leaf,
        // rather than adopting its root and relying on indistinguishable IDs.
        for mut stream in [
            root.entries(Default::default()),
            root.selected_entries(source.path.clone(), Default::default(), false),
        ] {
            let error = stream.next().await.unwrap().unwrap_err();
            assert!(matches!(
                error.downcast_ref::<RootedFsError>(),
                Some(RootedFsError::RootChanged(_))
            ));
            assert!(stream.next().await.is_none());
            stream.close().await.unwrap();
        }
        assert!(held.join("file").exists());
        assert!(original.join("file").exists());
    }
}
