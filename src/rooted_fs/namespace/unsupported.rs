use super::{Result, RootedFs, RootedFsError};
use crate::engine::namespace::{
    NamespaceParent, NamespacePreflightError, NamespaceQualification, NamespaceSemantics,
    Result as NamespaceResult,
};
use futures::future::BoxFuture;

/// No native qualification can be constructed on an unsupported platform.
#[derive(Clone, Debug)]
pub enum DestinationNamespace {}

impl RootedFs {
    pub async fn with_namespace_qualification(self) -> Result<(Self, DestinationNamespace)> {
        Err(RootedFsError::UnsupportedPlatform)
    }
}

impl NamespaceQualification for DestinationNamespace {
    fn qualify(
        &self,
        _parent: NamespaceParent,
    ) -> BoxFuture<'_, NamespaceResult<NamespaceSemantics>> {
        Box::pin(async { Err(unsupported()) })
    }

    fn validate_backup_address(
        &self,
        _path: crate::engine::domain::RelativePath,
    ) -> BoxFuture<'_, NamespaceResult<()>> {
        Box::pin(async { Err(unsupported()) })
    }

    fn seal(&self) -> BoxFuture<'_, NamespaceResult<()>> {
        Box::pin(async { Err(unsupported()) })
    }
}

fn unsupported() -> NamespacePreflightError {
    NamespacePreflightError::Io(std::io::Error::other(RootedFsError::UnsupportedPlatform))
}
