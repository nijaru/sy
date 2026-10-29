//! Empty OLD directories have no descendant effects. This is not recursive
//! retirement authority: callers exchange only OLD and clean it with rmdir.
use super::*;
use crate::fs_util::directory_stream::DirectoryStream;
use native_object::NativeObjectKey;
#[cfg(all(test, any(target_os = "macos", target_os = "linux")))]
#[path = "empty_directory_tests.rs"]
mod tests;

pub(super) struct EmptyDirectory {
    directory: OwnedFd,
    before: libc::stat,
    pub(super) object: NativeObjectKey,
}

impl EmptyDirectory {
    pub(super) fn capture(effect: &SourceEffect<'_>, before: libc::stat) -> Result<Self> {
        let directory = open_dir_at(effect.parent, effect.name)?;
        let object = object_key(directory.as_raw_fd(), None)?
            .ok_or_else(|| RootedFsError::SourceEffectUnqualified(effect.path.to_path_buf()))?;
        let empty = Self {
            directory,
            before,
            object,
        };
        empty.verify(effect)?;
        Ok(empty)
    }

    pub(super) fn verify(&self, effect: &SourceEffect<'_>) -> Result<()> {
        self.verify_binding(effect)?;
        if DirectoryStream::open(self.directory.as_raw_fd())?
            .next()
            .transpose()?
            .is_some()
        {
            return Err(RootedFsError::SourceEffectUnqualified(
                effect.path.to_path_buf(),
            ));
        }
        // Directory reads and SOURCE census can block. Do not accept a changed
        // empty-again directory or a foreign name after either operation.
        self.verify_binding(effect)
    }

    fn verify_binding(&self, effect: &SourceEffect<'_>) -> Result<()> {
        if !staging::same_observation(&self.before, &stat_fd(self.directory.as_raw_fd())?)
            || !stat_at_optional(effect.parent, effect.name)?
                .is_some_and(|named| staging::same_observation(&self.before, &named))
            || object_key(effect.parent, Some(effect.name))? != Some(self.object)
        {
            return Err(RootedFsError::DestinationChanged(effect.path.to_path_buf()));
        }
        Ok(())
    }
}
