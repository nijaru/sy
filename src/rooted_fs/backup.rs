//! Original authority for the one operator-selected external backup root.
use super::operand::{BoundDestination, DestinationRoot, SourceShape};
use super::*;
use std::sync::Mutex;

#[derive(Clone)]
pub struct BackupRoot {
    state: Arc<Mutex<BackupState>>,
}
#[cfg(all(test, unix))]
mod tests;

struct BackupState {
    original: std::result::Result<DestinationRoot, Arc<RootedFsError>>,
    prepared: bool,
}
impl BackupRoot {
    /// Capture before the main scans. An error is retained, not retried: an
    /// effect-free plan does not need unrelated backup-parent read authority.
    pub async fn capture(path: PathBuf) -> Self {
        let original = BoundDestination::open(path, SourceShape::Directory { basename: None })
            .await
            .map(|bound| bound.root)
            .map_err(Arc::new);
        Self {
            state: Arc::new(Mutex::new(BackupState {
                original,
                prepared: false,
            })),
        }
    }

    fn original(&self) -> Result<DestinationRoot> {
        let state = self
            .state
            .lock()
            .map_err(|_| RootedFsError::NamespaceCatalogueTerminal)?;
        state
            .original
            .as_ref()
            .cloned()
            .map_err(|error| RootedFsError::BackupRootCapture(Arc::clone(error)))
    }

    pub(super) fn original_path(&self) -> Result<PathBuf> {
        Ok(self.original()?.path().to_path_buf())
    }

    /// Called only once backup candidates exist, before collision proof/seal.
    pub async fn prepare(
        &self,
    ) -> Result<(DestinationNamespace, crate::engine::reconcile::EntryStream)> {
        {
            let mut state = self
                .state
                .lock()
                .map_err(|_| RootedFsError::NamespaceCatalogueTerminal)?;
            if state.prepared {
                return Err(RootedFsError::NamespaceCatalogueTerminal);
            }
            state.prepared = true;
        }
        let (root, qualification) = self.original()?.with_namespace_qualification().await?;
        let entries = match &root {
            DestinationRoot::Present(rooted) => {
                rooted.entry_stream(crate::engine::scan::ScanRequest::default())
            }
            DestinationRoot::Pending(_) => {
                crate::engine::reconcile::EntryStream::new(futures::stream::empty())
            }
        };
        self.state
            .lock()
            .map_err(|_| RootedFsError::NamespaceCatalogueTerminal)?
            .original = Ok(root);
        Ok((qualification, entries))
    }

    #[cfg(unix)]
    pub(super) fn copy_blocking(
        &self,
        main: &RootedFs,
        source: RootedCopySource<'_>,
        relative: &RelativePath,
    ) -> Result<()> {
        main.verify_root_path_blocking()?;
        // Serialize only original pending acquisition, not whole-file copies.
        let rooted = {
            let mut state = self
                .state
                .lock()
                .map_err(|_| RootedFsError::NamespaceCatalogueTerminal)?;
            if !state.prepared {
                return Err(RootedFsError::NamespaceUnqualified(
                    relative.as_path().to_path_buf(),
                ));
            }
            let mut original = state
                .original
                .as_ref()
                .cloned()
                .map_err(|error| RootedFsError::BackupRootCapture(Arc::clone(error)))?;
            original.inherit_backup_ownership(main);
            let rooted = original.acquire_backup_blocking(true, relative)?;
            rooted.require_namespace_sealed()?;
            state.original = Ok(DestinationRoot::Present(rooted.clone()));
            rooted
        };
        rooted.verify_root_path_blocking()?;
        source.copy_to(&rooted, relative.as_path())?;
        for authority in [main, &rooted] {
            authority.verify_root_path_blocking().map_err(|error| {
                RootedFsError::CommittedFinalizationFailed {
                    path: relative.as_path().to_path_buf(),
                    reason: error.to_string(),
                }
            })?;
        }
        Ok(())
    }
}
