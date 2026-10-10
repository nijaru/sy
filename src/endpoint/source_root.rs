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

impl SourceRoot {
    /// Retain the authority already captured during operand classification.
    pub fn from_rooted(rooted: RootedFs) -> Self {
        Self {
            rooted: rooted.into_local_source_authority(),
        }
    }

    pub async fn open(path: PathBuf) -> rooted_fs::Result<Self> {
        Ok(Self {
            rooted: RootedFs::open(path).await?.into_local_source_authority(),
        })
    }

    /// Blocking root acquisition for native workers and synchronous callers.
    pub fn open_blocking(path: PathBuf) -> rooted_fs::Result<Self> {
        Ok(Self {
            rooted: RootedFs::open_blocking_for_worker(path)?.into_local_source_authority(),
        })
    }

    pub fn path(&self) -> &Path {
        self.rooted.root_path()
    }

    pub fn rooted(&self) -> RootedFs {
        self.rooted.clone()
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
        assert_eq!(endpoint.metadata(Path::new("file")).await.unwrap().size, 4);
        let entries: Vec<_> = root
            .entries(Default::default())
            .map(|entry| entry.unwrap())
            .collect()
            .await;
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
            existing::remove_observed_source(root.rooted(), source.clone()).await,
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
