//! Source policy separates query-derived directory ancestry from trusted configuration pointers.
#[cfg(unix)]
use super::component_cstring;
use super::{Result, RootedFs, RootedFsError};
use std::fs::File;
#[cfg(unix)]
use std::mem::MaybeUninit;
#[cfg(unix)]
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::Path;

impl RootedFs {
    /// Local source configuration follows user-selected repository/config
    /// symlinks, unlike peer-controlled data paths. Anchor resolution to the
    /// held root so replacing its pathname cannot select another rule file.
    // Explicit configuration pointers may use '..' or symlinks outside this
    // root. This readonly local API is not remote path-confinement authority.
    #[cfg(unix)]
    pub(crate) fn open_source_configuration_blocking(&self, relative: &Path) -> Result<File> {
        if relative.is_absolute() {
            return Err(RootedFsError::InvalidRelativePath);
        }
        self.verify_source_root_path_blocking()?;
        let file = open_configuration_at(self.root_fd.as_raw_fd(), relative)?;
        self.verify_source_root_path_blocking()?;
        Ok(file)
    }

    #[cfg(not(unix))]
    pub(crate) fn open_source_configuration_blocking(&self, _relative: &Path) -> Result<File> {
        Err(RootedFsError::UnsupportedPlatform)
    }

    #[cfg(unix)]
    pub(crate) fn source_configuration_is_file_blocking(
        &self,
        relative: &Path,
    ) -> Result<Option<bool>> {
        if relative.is_absolute() {
            return Err(RootedFsError::InvalidRelativePath);
        }
        self.verify_source_root_path_blocking()?;
        let is_file = configuration_is_file_at(self.root_fd.as_raw_fd(), relative)?;
        self.verify_source_root_path_blocking()?;
        Ok(is_file)
    }

    #[cfg(not(unix))]
    pub(crate) fn source_configuration_is_file_blocking(
        &self,
        _relative: &Path,
    ) -> Result<Option<bool>> {
        Err(RootedFsError::UnsupportedPlatform)
    }

    #[cfg(unix)]
    fn source_policy_directory_blocking(&self, directory: &Path) -> Result<Option<OwnedFd>> {
        self.verify_source_root_path_blocking()?;
        // Resolve only the directory prefix through the existing no-follow
        // walker. It retains a genuine directory FD and never opens this leaf.
        match self.open_parent_blocking(&directory.join(".ignore")) {
            Ok((parent, _)) => Ok(Some(parent)),
            Err(RootedFsError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                Ok(None)
            }
            Err(error) => Err(error),
        }
    }

    #[cfg(unix)]
    pub(crate) fn open_source_policy_configuration_blocking(
        &self,
        directory: &Path,
        configuration: &Path,
    ) -> Result<Option<File>> {
        let Some(parent) = self.source_policy_directory_blocking(directory)? else {
            return Ok(None);
        };
        let file = match open_configuration_at(parent.as_raw_fd(), configuration) {
            Ok(file) => Some(file),
            Err(RootedFsError::Io(error))
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound
                        | std::io::ErrorKind::NotADirectory
                        | std::io::ErrorKind::PermissionDenied
                ) =>
            {
                None
            }
            Err(error) => return Err(error),
        };
        self.verify_source_root_path_blocking()?;
        Ok(file)
    }

    #[cfg(unix)]
    pub(crate) fn source_policy_configuration_is_file_blocking(
        &self,
        directory: &Path,
        configuration: &Path,
    ) -> Result<Option<bool>> {
        let Some(parent) = self.source_policy_directory_blocking(directory)? else {
            return Ok(None);
        };
        let result = configuration_is_file_at(parent.as_raw_fd(), configuration)?;
        self.verify_source_root_path_blocking()?;
        Ok(result)
    }

    #[cfg(not(unix))]
    pub(crate) fn open_source_policy_configuration_blocking(
        &self,
        _directory: &Path,
        _configuration: &Path,
    ) -> Result<Option<File>> {
        Err(RootedFsError::UnsupportedPlatform)
    }

    #[cfg(not(unix))]
    pub(crate) fn source_policy_configuration_is_file_blocking(
        &self,
        _directory: &Path,
        _configuration: &Path,
    ) -> Result<Option<bool>> {
        Err(RootedFsError::UnsupportedPlatform)
    }
}

#[cfg(unix)]
fn open_configuration_at(parent: RawFd, relative: &Path) -> Result<File> {
    if relative.is_absolute() {
        return Err(RootedFsError::InvalidRelativePath);
    }
    let path = component_cstring(relative.as_os_str())?;
    // SAFETY: parent is held and path is NUL terminated. Only trusted policy
    // pointers may follow links here; peer-derived directory ancestry is
    // resolved separately with no-follow semantics. O_NONBLOCK prevents
    // configuration FIFOs from indefinitely blocking a native worker.
    let fd = unsafe {
        libc::openat(
            parent,
            path.as_ptr(),
            libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NONBLOCK,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    // SAFETY: successful openat returned one owned descriptor.
    Ok(unsafe { File::from_raw_fd(fd) })
}

#[cfg(unix)]
fn configuration_is_file_at(parent: RawFd, relative: &Path) -> Result<Option<bool>> {
    if relative.is_absolute() {
        return Err(RootedFsError::InvalidRelativePath);
    }
    let path = component_cstring(relative.as_os_str())?;
    let mut stat = MaybeUninit::<libc::stat>::uninit();
    // SAFETY: parent/path remain live and stat is writable for the native type.
    // Following trusted configuration pointers is not peer data-path authority.
    if unsafe { libc::fstatat(parent, path.as_ptr(), stat.as_mut_ptr(), 0) } < 0 {
        let error = std::io::Error::last_os_error();
        return if matches!(
            error.kind(),
            std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
        ) {
            Ok(None)
        } else {
            Err(error.into())
        };
    }
    // SAFETY: successful fstatat initialized the complete stat.
    let stat = unsafe { stat.assume_init() };
    Ok(Some(stat.st_mode & libc::S_IFMT == libc::S_IFREG))
}
