//! One backup-address policy shared by declaration and native execution.
use super::domain::{InvalidRelativePath, RelativePath};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackupAddress {
    Main(RelativePath),
    FixedRoot(RelativePath),
}

#[derive(Debug, Clone)]
pub struct BackupPolicy {
    directory: Option<RelativePath>,
    external: bool,
    suffix: String,
}

impl BackupPolicy {
    pub fn in_root(directory: Option<RelativePath>, suffix: String) -> Self {
        Self {
            directory,
            external: false,
            suffix,
        }
    }

    pub fn local(
        main: &Path,
        directory: Option<&Path>,
        suffix: String,
    ) -> Result<Self, InvalidRelativePath> {
        let (directory, external) = match directory {
            None => (None, false),
            Some(directory) => match directory.strip_prefix(main) {
                Ok(relative) if relative.as_os_str().is_empty() => (None, false),
                Ok(relative) => (Some(RelativePath::new(relative)?), false),
                Err(_) => (None, true),
            },
        };
        Ok(Self {
            directory,
            external,
            suffix,
        })
    }

    pub async fn capture_local(
        main: &Path,
        directory: Option<&Path>,
        suffix: String,
    ) -> Result<(Self, Option<crate::rooted_fs::backup::BackupRoot>), InvalidRelativePath> {
        let policy = Self::local(main, directory, suffix)?;
        let root = if policy.external {
            Some(
                crate::rooted_fs::backup::BackupRoot::capture(
                    directory.ok_or(InvalidRelativePath::Empty)?.to_path_buf(),
                )
                .await,
            )
        } else {
            None
        };
        Ok((policy, root))
    }

    pub fn destination(&self, path: &RelativePath) -> Result<BackupAddress, InvalidRelativePath> {
        let mut result = match &self.directory {
            Some(directory) => directory.as_path().join(path.as_path()),
            None => PathBuf::from(path.as_path()),
        };
        let mut name = path
            .as_path()
            .file_name()
            .ok_or(InvalidRelativePath::Empty)?
            .to_os_string();
        name.push(&self.suffix);
        result.set_file_name(name);
        let result = RelativePath::new(result)?;
        Ok(if self.external {
            BackupAddress::FixedRoot(result)
        } else {
            BackupAddress::Main(result)
        })
    }
}
