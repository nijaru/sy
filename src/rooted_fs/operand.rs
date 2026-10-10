//! Initial operator-selected operand binding. No transfer reopens these paths.
use super::*;
use crate::engine::domain::RelativePath;

#[derive(Debug, Clone)]
pub enum SourceShape {
    Directory { basename: Option<RelativePath> },
    Leaf { name: RelativePath },
}

#[derive(Debug, Clone)]
pub struct BoundSource {
    pub rooted: RootedFs,
    pub shape: SourceShape,
}

#[derive(Debug, Clone)]
pub enum DestinationRoot {
    Present(RootedFs),
    Pending(PendingRoot),
}

#[derive(Debug, Clone)]
pub struct BoundDestination {
    pub root: DestinationRoot,
    pub name: Option<RelativePath>,
}

/// Permanent parents are not private staging: we do not claim creator-owned
/// cleanup or roll them back. The ancestor descriptor is the sole authority
/// for initial acquisition; once acquired, the root is never adopted again.
#[derive(Debug, Clone)]
pub struct PendingRoot {
    ancestor: RootedFs,
    path: PathBuf,
    missing: Vec<OsString>,
}

impl PendingRoot {
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn bind_mutations(&mut self, admission: Arc<PublicationAdmission>, read_only: bool) {
        self.ancestor.bind_session_mutations(admission, read_only);
    }
}

impl DestinationRoot {
    pub(crate) fn bind_mutations(&mut self, admission: Arc<PublicationAdmission>, read_only: bool) {
        match self {
            Self::Present(rooted) => rooted.bind_session_mutations(admission, read_only),
            Self::Pending(pending) => pending.bind_mutations(admission, read_only),
        }
    }

    pub fn path(&self) -> &Path {
        match self {
            Self::Present(rooted) => rooted.root_path(),
            Self::Pending(pending) => &pending.path,
        }
    }

    pub async fn acquire(self) -> Result<RootedFs> {
        self.acquire_with_continuity(false).await
    }

    pub(crate) async fn acquire_local(self) -> Result<RootedFs> {
        self.acquire_with_continuity(true).await
    }

    async fn acquire_with_continuity(self, local: bool) -> Result<RootedFs> {
        match self {
            Self::Present(rooted) => Ok(rooted),
            Self::Pending(pending) => {
                tokio::task::spawn_blocking(move || pending.acquire_blocking(local))
                    .await
                    .map_err(|error| RootedFsError::Worker(error.to_string()))?
            }
        }
    }
}

impl BoundSource {
    pub async fn open(path: PathBuf, contents: bool) -> Result<Self> {
        Self::open_with_directory_links(path, contents, false).await
    }

    /// Explicit local --copy-links also applies to a directory root operand.
    /// This does not authorize following descendant paths on a remote source.
    pub async fn open_following(path: PathBuf, contents: bool) -> Result<Self> {
        Self::open_with_directory_links(path, contents, true).await
    }

    async fn open_with_directory_links(
        path: PathBuf,
        contents: bool,
        follow: bool,
    ) -> Result<Self> {
        tokio::task::spawn_blocking(move || source_blocking(path, contents, follow))
            .await
            .map_err(|error| RootedFsError::Worker(error.to_string()))?
    }
}

impl BoundDestination {
    pub async fn open(path: PathBuf, shape: SourceShape) -> Result<Self> {
        tokio::task::spawn_blocking(move || destination_blocking(path, shape))
            .await
            .map_err(|error| RootedFsError::Worker(error.to_string()))?
    }
}

fn parent(path: &Path) -> &Path {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."))
}

fn name(path: &Path) -> Result<RelativePath> {
    RelativePath::new(PathBuf::from(
        path.file_name().ok_or(RootedFsError::InvalidRelativePath)?,
    ))
    .map_err(|_| RootedFsError::InvalidRelativePath)
}

#[cfg(unix)]
fn requires_directory(path: &Path) -> bool {
    // Preserve POSIX directory syntax before extracting a normalized leaf.
    let bytes = path.as_os_str().as_bytes();
    bytes.ends_with(b"/") || bytes.ends_with(b"/.")
}

#[cfg(unix)]
fn source_blocking(path: PathBuf, contents: bool, follow: bool) -> Result<BoundSource> {
    let requires_directory = requires_directory(&path);
    if follow || requires_directory {
        let (rooted, shape) = match classify_destination(&path)? {
            DestinationClassification::Directory(rooted) => {
                let basename = if contents {
                    None
                } else {
                    path.file_name().map(|_| name(&path)).transpose()?
                };
                (rooted, SourceShape::Directory { basename })
            }
            DestinationClassification::Leaf {
                parent,
                present: true,
            } if !requires_directory => (parent, SourceShape::Leaf { name: name(&path)? }),
            DestinationClassification::Leaf { present: true, .. } => {
                return Err(std::io::Error::from_raw_os_error(libc::ENOTDIR).into());
            }
            _ => return Err(std::io::Error::from(std::io::ErrorKind::NotFound).into()),
        };
        return Ok(BoundSource { rooted, shape });
    }
    let Some(leaf) = path.file_name() else {
        return Ok(BoundSource {
            rooted: RootedFs::open_blocking(path)?,
            shape: SourceShape::Directory { basename: None },
        });
    };
    let parent_root = RootedFs::open_blocking(parent(&path).to_path_buf())?;
    let observed = stat_at_optional(parent_root.root_fd.as_raw_fd(), leaf)?
        .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::NotFound))?;
    if observed.st_mode & libc::S_IFMT == libc::S_IFDIR {
        let fd = open_dir_at(parent_root.root_fd.as_raw_fd(), leaf)?;
        let opened = stat_fd(fd.as_raw_fd())?;
        let after = stat_at_optional(parent_root.root_fd.as_raw_fd(), leaf)?;
        if opened.st_dev != observed.st_dev
            || opened.st_ino != observed.st_ino
            || after.is_none_or(|after| {
                after.st_dev != opened.st_dev
                    || after.st_ino != opened.st_ino
                    || after.st_mode & libc::S_IFMT != libc::S_IFDIR
            })
        {
            return Err(RootedFsError::CopySourceChanged(path));
        }
        let basename = if contents { None } else { Some(name(&path)?) };
        Ok(BoundSource {
            rooted: RootedFs::from_operand_fd(path, fd),
            shape: SourceShape::Directory { basename },
        })
    } else {
        Ok(BoundSource {
            rooted: parent_root,
            shape: SourceShape::Leaf { name: name(&path)? },
        })
    }
}

#[cfg(unix)]
fn destination_blocking(path: PathBuf, shape: SourceShape) -> Result<BoundDestination> {
    match shape {
        SourceShape::Directory { basename } => {
            let path = match basename {
                Some(name) => path.join(name.as_path()),
                None => path,
            };
            Ok(BoundDestination {
                root: destination_root_blocking(path)?,
                name: None,
            })
        }
        SourceShape::Leaf { name: source_name } => {
            // Following the operator-selected destination directory symlink is
            // intentional, once. A dangling/cyclic symlink remains an opaque
            // replaceable leaf. No descendants are followed after root binding.
            match classify_destination(&path)? {
                DestinationClassification::Directory(rooted) => Ok(BoundDestination {
                    root: DestinationRoot::Present(rooted),
                    name: Some(source_name),
                }),
                DestinationClassification::Leaf { parent, .. } => Ok(BoundDestination {
                    name: Some(name(&path)?),
                    root: DestinationRoot::Present(parent),
                }),
                DestinationClassification::MissingParent => Ok(BoundDestination {
                    name: Some(name(&path)?),
                    root: destination_root_blocking(parent(&path).to_path_buf())?,
                }),
            }
        }
    }
}

#[cfg(unix)]
enum DestinationClassification {
    Directory(RootedFs),
    Leaf { parent: RootedFs, present: bool },
    MissingParent,
}

#[cfg(unix)]
fn classify_destination(path: &Path) -> Result<DestinationClassification> {
    let Some(leaf) = path.file_name() else {
        return Ok(DestinationClassification::Directory(
            RootedFs::open_blocking(path.to_path_buf())?,
        ));
    };
    let parent = match RootedFs::open_blocking(parent(path).to_path_buf()) {
        Ok(parent) => parent,
        Err(RootedFsError::Io(error)) if error.raw_os_error() == Some(libc::ENOENT) => {
            return Ok(DestinationClassification::MissingParent)
        }
        Err(error) => return Err(error),
    };
    let observed = stat_at_optional(parent.root_fd.as_raw_fd(), leaf)?;
    let Some(observed) = observed else {
        return Ok(DestinationClassification::Leaf {
            parent,
            present: false,
        });
    };
    let kind = observed.st_mode & libc::S_IFMT;
    let directory_syntax = requires_directory(path);
    if directory_syntax && !matches!(kind, libc::S_IFDIR | libc::S_IFLNK) {
        return Err(std::io::Error::from_raw_os_error(libc::ENOTDIR).into());
    }
    let child = match kind {
        libc::S_IFDIR => open_dir_at(parent.root_fd.as_raw_fd(), leaf)?,
        libc::S_IFLNK => {
            let leaf_c = component_cstring(leaf)?;
            // SAFETY: held parent and live single-component pathname. Following
            // this operator-selected directory symlink is intentional once.
            let fd = unsafe {
                libc::openat(
                    parent.root_fd.as_raw_fd(),
                    leaf_c.as_ptr(),
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
                )
            };
            if fd < 0 {
                let error = std::io::Error::last_os_error();
                if directory_syntax && error.raw_os_error() == Some(libc::ENOTDIR) {
                    return Err(error.into());
                }
                if matches!(
                    error.raw_os_error(),
                    Some(libc::ENOENT | libc::ENOTDIR | libc::ELOOP)
                ) {
                    return Ok(DestinationClassification::Leaf {
                        parent,
                        present: true,
                    });
                }
                return Err(error.into());
            }
            // SAFETY: successful openat returned a fresh owned descriptor.
            unsafe { OwnedFd::from_raw_fd(fd) }
        }
        _ => {
            return Ok(DestinationClassification::Leaf {
                parent,
                present: true,
            })
        }
    };
    let opened = stat_fd(child.as_raw_fd())?;
    let after = stat_at_optional(parent.root_fd.as_raw_fd(), leaf)?;
    if opened.st_mode & libc::S_IFMT != libc::S_IFDIR
        || (kind == libc::S_IFDIR
            && (observed.st_dev != opened.st_dev || observed.st_ino != opened.st_ino))
        || after.is_none_or(|after| identity_from_stat(&after) != identity_from_stat(&observed))
    {
        return Err(RootedFsError::DestinationChanged(path.to_path_buf()));
    }
    Ok(DestinationClassification::Directory(
        RootedFs::from_operand_fd(path.to_path_buf(), child),
    ))
}

#[cfg(unix)]
fn destination_root_blocking(path: PathBuf) -> Result<DestinationRoot> {
    let mut candidate = path.clone();
    let mut missing = Vec::new();
    loop {
        let classification = classify_destination(&candidate)?;
        match classification {
            DestinationClassification::Directory(rooted) => {
                if missing.is_empty() {
                    return Ok(DestinationRoot::Present(rooted));
                }
                missing.reverse();
                return Ok(DestinationRoot::Pending(PendingRoot {
                    ancestor: rooted,
                    path,
                    missing,
                }));
            }
            DestinationClassification::Leaf { present: true, .. } => {
                return Err(std::io::Error::from_raw_os_error(libc::ENOTDIR).into())
            }
            DestinationClassification::Leaf {
                parent: ancestor,
                present: false,
            } => {
                missing.push(
                    candidate
                        .file_name()
                        .ok_or(RootedFsError::InvalidRelativePath)?
                        .to_owned(),
                );
                missing.reverse();
                return Ok(DestinationRoot::Pending(PendingRoot {
                    ancestor,
                    path,
                    missing,
                }));
            }
            DestinationClassification::MissingParent => {
                let leaf = candidate
                    .file_name()
                    .ok_or(RootedFsError::InvalidRelativePath)?;
                if !matches!(
                    candidate.components().next_back(),
                    Some(Component::Normal(_))
                ) {
                    return Err(RootedFsError::InvalidRelativePath);
                }
                missing.push(leaf.to_owned());
                candidate = parent(&candidate).to_path_buf();
            }
        }
    }
}

#[cfg(unix)]
impl PendingRoot {
    fn acquire_blocking(self, local: bool) -> Result<RootedFs> {
        self.ancestor.require_writable()?;
        let mut fd = self.ancestor.root_fd.try_clone()?;
        let mut prefix = PathBuf::new();
        for name in &self.missing {
            if local {
                self.ancestor.verify_root_path_blocking()?;
            }
            prefix.push(name);
            self.ancestor.verify_parent_binding_blocking(&prefix, &fd)?;
            if stat_at_optional(fd.as_raw_fd(), name)?.is_some() {
                return Err(RootedFsError::DestinationChanged(self.path));
            }
            let name_c = component_cstring(name)?;
            let _permit = self.ancestor.admit_mutation_blocking()?;
            // SAFETY: fd is held, name_c is one live NUL-terminated component.
            if unsafe { libc::mkdirat(fd.as_raw_fd(), name_c.as_ptr(), 0o777) } != 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            let observed = stat_at_optional(fd.as_raw_fd(), name)?
                .ok_or_else(|| RootedFsError::DestinationChanged(self.path.clone()))?;
            let child = open_dir_at(fd.as_raw_fd(), name)?;
            let opened = stat_fd(child.as_raw_fd())?;
            if observed.st_dev != opened.st_dev
                || observed.st_ino != opened.st_ino
                || observed.st_mode & libc::S_IFMT != libc::S_IFDIR
            {
                return Err(RootedFsError::DestinationChanged(self.path));
            }
            fd = child;
        }
        if local {
            self.ancestor.verify_root_path_blocking()?;
        }
        // Observational initial acquisition, not atomic creator ownership.
        let (parent, leaf) = self.ancestor.open_parent_blocking(&prefix)?;
        let current = open_dir_at(parent.as_raw_fd(), &leaf)?;
        let observed = stat_fd(current.as_raw_fd())?;
        let held = stat_fd(fd.as_raw_fd())?;
        if observed.st_dev != held.st_dev || observed.st_ino != held.st_ino {
            return Err(RootedFsError::DestinationChanged(self.path));
        }
        let mut rooted = RootedFs::from_operand_fd(self.path, fd);
        rooted.mutation_admission = self.ancestor.mutation_admission;
        Ok(rooted)
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::io::Read;

    #[tokio::test]
    async fn classified_directory_authorities_survive_relocation_but_local_paths_fail() {
        let fixture = tempfile::tempdir().unwrap();
        let path = fixture.path().join("selected");
        let held = fixture.path().join("held");
        let foreign = fixture.path().join("foreign");
        std::fs::create_dir(&path).unwrap();
        std::fs::create_dir(&foreign).unwrap();
        std::fs::write(path.join("file"), b"original").unwrap();
        std::fs::write(foreign.join("file"), b"foreign").unwrap();
        let source = BoundSource::open(path.clone(), false).await.unwrap();
        let destination =
            BoundDestination::open(path.clone(), SourceShape::Directory { basename: None })
                .await
                .unwrap();
        std::fs::rename(&path, &held).unwrap();
        std::os::unix::fs::symlink(&foreign, &path).unwrap();
        let destination = destination.root.acquire().await.unwrap();
        let relative = RelativePath::new("file").unwrap();
        for rooted in [&source.rooted, &destination] {
            let mut bytes = Vec::new();
            rooted
                .open_regular_blocking(&relative)
                .unwrap()
                .read_to_end(&mut bytes)
                .unwrap();
            assert_eq!(bytes, b"original");
            assert!(matches!(
                rooted.verify_root_path_blocking(),
                Err(RootedFsError::RootChanged(_))
            ));
        }
        let local_source = crate::endpoint::source_root::SourceRoot::from_rooted(source.rooted);
        assert!(matches!(
            local_source.validate().await,
            Err(RootedFsError::RootChanged(_))
        ));
        assert_eq!(std::fs::read(foreign.join("file")).unwrap(), b"foreign");
    }

    #[tokio::test]
    async fn pending_acquisition_refuses_foreign_names_and_preserves_ancestor_authority() {
        let fixture = tempfile::tempdir().unwrap();
        let ancestor = fixture.path().join("ancestor");
        let moved = fixture.path().join("held");
        std::fs::create_dir(&ancestor).unwrap();
        let binding = BoundDestination::open(
            ancestor.join("missing/nested"),
            SourceShape::Directory { basename: None },
        )
        .await
        .unwrap();
        std::fs::create_dir(ancestor.join("missing")).unwrap();
        std::fs::write(ancestor.join("missing/foreign"), b"keep").unwrap();
        assert!(matches!(
            binding.root.acquire().await,
            Err(RootedFsError::DestinationChanged(_))
        ));
        assert_eq!(
            std::fs::read(ancestor.join("missing/foreign")).unwrap(),
            b"keep"
        );
        assert!(!ancestor.join("missing/nested").exists());
        let binding = BoundDestination::open(
            ancestor.join("new/nested"),
            SourceShape::Directory { basename: None },
        )
        .await
        .unwrap();
        std::fs::rename(&ancestor, &moved).unwrap();
        std::fs::create_dir(&ancestor).unwrap();
        assert!(matches!(
            binding.root.clone().acquire_local().await,
            Err(RootedFsError::RootChanged(_))
        ));
        assert!(!ancestor.join("new").exists());
        assert!(!moved.join("new").exists());
        let root = binding.root.acquire().await.unwrap();
        assert!(moved.join("new/nested").is_dir());
        assert!(!ancestor.join("new").exists());
        assert!(matches!(
            root.verify_root_path_blocking(),
            Err(RootedFsError::RootChanged(_))
        ));
        // Once captured, later replacement cannot be adopted on a retry.
        std::fs::create_dir_all(ancestor.join("new/nested")).unwrap();
        assert!(matches!(
            root.verify_root_path_blocking(),
            Err(RootedFsError::RootChanged(_))
        ));
    }
}

#[cfg(not(unix))]
fn source_blocking(_path: PathBuf, _contents: bool, _follow: bool) -> Result<BoundSource> {
    Err(RootedFsError::UnsupportedPlatform)
}
#[cfg(not(unix))]
fn destination_blocking(_path: PathBuf, _shape: SourceShape) -> Result<BoundDestination> {
    Err(RootedFsError::UnsupportedPlatform)
}
#[cfg(not(unix))]
impl PendingRoot {
    fn acquire_blocking(self, _local: bool) -> Result<RootedFs> {
        Err(RootedFsError::UnsupportedPlatform)
    }
}
