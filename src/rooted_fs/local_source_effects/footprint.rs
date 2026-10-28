//! Native destination facts for SOURCE exclusion, never effect permission.
//! Capturing these facts performs no SOURCE validation and grants no effect.
use super::*;

pub(super) struct NativeFootprint {
    pub(super) parent: native_object::NativeObjectKey,
    pub(super) candidates: Vec<native_object::NativeObjectKey>,
    pub(super) additional: SourceExclusion,
    pub(super) empty_directory: Option<empty_directory::EmptyDirectory>,
}

impl SourceEffect<'_> {
    pub(super) fn capture_footprint(&self) -> Result<NativeFootprint> {
        let unknown = || RootedFsError::SourceEffectUnqualified(self.path.to_path_buf());
        let parent = object_key(self.parent, None)?.ok_or_else(unknown)?;
        // The containing namespace must not be any original TREE data DIR.
        // Selected-leaf parent anchors remain neutral. Object absence is not
        // ancestry closure for recursive retirement or directory policy setters.
        let mut candidates = vec![parent];
        let mut empty_directory = None;
        let mut additional = SourceExclusion::Objects;
        match self.kind {
            SourceEffectKind::LeafPublication | SourceEffectKind::NamespaceRetirement => {
                if let Some(stat) = stat_at_optional(self.parent, self.name)? {
                    let old = match stat.st_mode & libc::S_IFMT {
                        libc::S_IFREG | libc::S_IFLNK => {
                            object_key(self.parent, Some(self.name))?.ok_or_else(unknown)?
                        }
                        libc::S_IFDIR => {
                            let directory = empty_directory::EmptyDirectory::capture(self, stat)?;
                            let object = directory.object;
                            empty_directory = Some(directory);
                            object
                        }
                        _ => return Err(unknown()),
                    };
                    if stat.st_mode & libc::S_IFMT == libc::S_IFLNK {
                        additional = SourceExclusion::OperatorLinks(old);
                    }
                    candidates.push(old);
                } else if matches!(self.kind, SourceEffectKind::NamespaceRetirement) {
                    return Err(unknown());
                }
            }
            SourceEffectKind::PrivateLeafMetadata => {
                let fd = self.object.ok_or_else(unknown)?;
                let kind = stat_fd(fd)?.st_mode & libc::S_IFMT;
                if !matches!(kind, libc::S_IFREG | libc::S_IFLNK) {
                    return Err(unknown());
                }
                let object = object_key(fd, None)?.ok_or_else(unknown)?;
                if kind == libc::S_IFLNK {
                    additional = SourceExclusion::OperatorLinks(object);
                }
                candidates.push(object);
            }
            SourceEffectKind::SymlinkMetadata => {
                let opened = if self.object.is_none() {
                    Some(open_symlink_at(self.parent, self.name)?)
                } else {
                    None
                };
                let fd = self
                    .object
                    .or_else(|| opened.as_ref().map(AsRawFd::as_raw_fd))
                    .ok_or_else(unknown)?;
                let held = stat_fd(fd)?;
                let named = stat_at_optional(self.parent, self.name)?.ok_or_else(unknown)?;
                let object = object_key(fd, None)?.ok_or_else(unknown)?;
                if held.st_mode & libc::S_IFMT != libc::S_IFLNK
                    || !staging::same_observation(&held, &named)
                    || object_key(self.parent, Some(self.name))? != Some(object)
                {
                    return Err(unknown());
                }
                additional = SourceExclusion::OperatorLinks(object);
                candidates.push(object);
            }
            SourceEffectKind::PrivateDirectoryMetadata => {
                let fd = self.object.ok_or_else(unknown)?;
                let held = stat_fd(fd)?;
                let named = stat_at_optional(self.parent, self.name)?.ok_or_else(unknown)?;
                if held.st_mode & libc::S_IFMT != libc::S_IFDIR
                    || !staging::same_inode(&held, &named)
                    || crate::fs_util::directory_stream::DirectoryStream::open(fd)?
                        .next()
                        .transpose()?
                        .is_some()
                {
                    return Err(unknown());
                }
                let object = object_key(fd, None)?.ok_or_else(unknown)?;
                if object_key(self.parent, Some(self.name))? != Some(object) {
                    return Err(unknown());
                }
                candidates.push(object);
            }
            SourceEffectKind::DirectoryNamespaceChange => {
                if stat_at_optional(self.parent, self.name)?.is_some() {
                    return Err(unknown());
                }
            }
            SourceEffectKind::InodeMetadata => {
                let named = stat_at_optional(self.parent, self.name)?.ok_or_else(unknown)?;
                // Regular objects cannot contain SOURCE ancestors. Directories
                // still require operator-binding provenance, not census absence.
                if named.st_mode & libc::S_IFMT != libc::S_IFREG {
                    return Err(unknown());
                }
                let named_key = object_key(self.parent, Some(self.name))?.ok_or_else(unknown)?;
                if let Some(fd) = self.object {
                    if stat_fd(fd)?.st_mode & libc::S_IFMT != libc::S_IFREG
                        || object_key(fd, None)? != Some(named_key)
                    {
                        return Err(unknown());
                    }
                }
                candidates.push(named_key);
            }
            SourceEffectKind::DirectoryPolicy => {
                let named = stat_at_optional(self.parent, self.name)?.ok_or_else(unknown)?;
                let opened = if self.object.is_none() {
                    Some(source_observations::operator::directory_at(
                        self.parent,
                        self.name,
                    )?)
                } else {
                    None
                };
                let fd = self
                    .object
                    .or_else(|| opened.as_ref().map(AsRawFd::as_raw_fd))
                    .ok_or_else(unknown)?;
                let held = stat_fd(fd)?;
                let object = object_key(fd, None)?.ok_or_else(unknown)?;
                if held.st_mode & libc::S_IFMT != libc::S_IFDIR
                    || !staging::same_inode(&held, &named)
                    || object_key(self.parent, Some(self.name))? != Some(object)
                {
                    return Err(unknown());
                }
                additional = SourceExclusion::DirectoryAncestors(object);
                candidates.push(object);
            }
            SourceEffectKind::DirectoryTimestamp => {
                let named = stat_at_optional(self.parent, self.name)?.ok_or_else(unknown)?;
                if named.st_mode & libc::S_IFMT != libc::S_IFDIR {
                    return Err(unknown());
                }
                let named_key = object_key(self.parent, Some(self.name))?.ok_or_else(unknown)?;
                if let Some(fd) = self.object {
                    let held = stat_fd(fd)?;
                    if held.st_mode & libc::S_IFMT != libc::S_IFDIR
                        || !staging::same_inode(&held, &named)
                        || object_key(fd, None)? != Some(named_key)
                    {
                        return Err(unknown());
                    }
                }
                // futimens affects this inode's timestamps only. Object absence
                // suffices for that footprint, not for an ancestor's permissions,
                // inherited policy, namespace changes or recursive retirement.
                candidates.push(named_key);
            }
            SourceEffectKind::LinkCountChange => {
                // The caller owns the original creator/name/seal and exact own
                // nlink transitions. A declaration without that genuine object
                // cannot qualify a future link or unlink. Regular inodes cannot
                // contain SOURCE ancestors; directory ancestry stays unknown.
                let fd = self.object.ok_or_else(unknown)?;
                let held = stat_fd(fd)?;
                let named = stat_at_optional(self.parent, self.name)?.ok_or_else(unknown)?;
                if held.st_mode & libc::S_IFMT != libc::S_IFREG
                    || !staging::same_observation(&held, &named)
                {
                    return Err(unknown());
                }
                let object = object_key(fd, None)?.ok_or_else(unknown)?;
                if object_key(self.parent, Some(self.name))? != Some(object) {
                    return Err(unknown());
                }
                candidates.push(object);
            }
            SourceEffectKind::Unknown => return Err(unknown()),
        }
        Ok(NativeFootprint {
            parent,
            candidates,
            additional,
            empty_directory,
        })
    }
}
