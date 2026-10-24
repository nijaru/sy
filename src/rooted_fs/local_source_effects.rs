//! LOCAL exclusion admission through original SOURCE and held native effects.
use super::*;
use source_observations::inventory::SourceInventory;
pub(super) mod ancestry;
pub(super) mod native_object;
use native_object::object_key;
#[cfg(all(test, target_os = "macos"))]
mod symlink_tests;
#[cfg(test)]
#[path = "local_source_effects_tests.rs"]
mod tests;

/// Unknown declarations remain explicit, including directory retirement.
/// A leaf publication includes OLD inode retirement; group link deltas require
/// the actual named regular FD, never a declaration-only grant.
#[derive(Clone, Copy)]
pub enum SourceEffectKind {
    LeafPublication,
    PrivateLeafMetadata,
    PrivateDirectoryMetadata,
    DirectoryNamespaceChange,
    InodeMetadata,
    SymlinkMetadata,
    /// Directory access/inheritance policy needs physical ancestor exclusion.
    DirectoryPolicy,
    /// Only target directory mtime/ctime, never access or inheritance policy.
    DirectoryTimestamp,
    LinkCountChange,
    NamespaceRetirement,
    Unknown,
}

pub(super) struct SourceEffect<'a> {
    pub(super) kind: SourceEffectKind,
    pub(super) parent: RawFd,
    pub(super) name: &'a OsStr,
    pub(super) object: Option<RawFd>,
    pub(super) path: &'a Path,
}

/// Issued only by successful native admission before private creation. The
/// namespace owner pins the admitted parent and the actual creator directory.
/// This is not authority for OLD retirement or arbitrary same-inode aliases.
#[derive(Clone, Copy)]
pub(super) enum PrivateCreationAdmission {
    Unguarded,
    Qualified(PrivateCreationReceipt),
}

#[derive(Clone, Copy)]
pub(super) struct PrivateCreationReceipt {
    parent: native_object::NativeObjectKey,
}

#[derive(Clone)]
pub struct LocalSourceEffects {
    inventory: Arc<SourceInventory>,
}

impl LocalSourceEffects {
    /// Seal only after the controller has joined both successful scan producers.
    pub async fn seal(source: &crate::endpoint::source_root::SourceRoot) -> Result<Self> {
        let rooted = source.rooted();
        tokio::task::spawn_blocking(move || {
            let effects = Self {
                inventory: Arc::new(rooted.seal_source_inventory_blocking()?),
            };
            effects.validate_originals_blocking()?;
            Ok(effects)
        })
        .await
        .map_err(|error| RootedFsError::Worker(error.to_string()))?
    }

    pub(super) fn validate_originals_blocking(&self) -> Result<()> {
        self.inventory
            .validate_originals_blocking()
            .map_err(|error| {
                // This SOURCE check runs inside destination staging/publication.
                // Preserve its typed cause across destination-role error mapping.
                match error {
                    RootedFsError::RootChanged(_) => {
                        RootedFsError::Io(std::io::Error::other(error))
                    }
                    error => error,
                }
            })
    }

    pub async fn validate_effect(
        &self,
        destination: &operand::DestinationRoot,
        path: RelativePath,
        kind: SourceEffectKind,
    ) -> Result<()> {
        let effects = self.clone();
        let destination = destination.clone();
        tokio::task::spawn_blocking(move || match destination {
            operand::DestinationRoot::Present(rooted) => {
                rooted.verify_root_path_blocking()?;
                effects.validate_declared_effect_blocking(&rooted, path.as_path(), kind)
            }
            operand::DestinationRoot::Pending(pending) => {
                pending.declare_source_effect_blocking(&effects, path.as_path(), kind)
            }
        })
        .await
        .map_err(|error| RootedFsError::Worker(error.to_string()))?
    }

    pub(super) fn validate_declared_effect_blocking(
        &self,
        rooted: &RootedFs,
        path: &Path,
        kind: SourceEffectKind,
    ) -> Result<()> {
        let parent_path = path.parent().ok_or(RootedFsError::InvalidRelativePath)?;
        let mut directory = rooted.root_fd.try_clone()?;
        let mut prefix = PathBuf::new();
        for component in parent_path.components() {
            let Component::Normal(name) = component else {
                return Err(RootedFsError::InvalidRelativePath);
            };
            prefix.push(name);
            let absent = rooted.namespace_predicts_absent(&prefix, directory.as_raw_fd())?;
            match open_dir_at(directory.as_raw_fd(), name) {
                Ok(_) if absent => return Err(RootedFsError::DestinationChanged(prefix)),
                Ok(fd) => directory = fd,
                Err(RootedFsError::Io(error))
                    if absent
                        && error.raw_os_error() == Some(libc::ENOENT)
                        && matches!(
                            kind,
                            SourceEffectKind::LeafPublication
                                | SourceEffectKind::DirectoryNamespaceChange
                        ) =>
                {
                    // Only new-only descendants can exist below this original
                    // absent slot. Admit its actual namespace effect, not a
                    // projected identity or name profile for future parents.
                    self.admit_native_effect_blocking(SourceEffect {
                        kind: SourceEffectKind::DirectoryNamespaceChange,
                        parent: directory.as_raw_fd(),
                        name,
                        object: None,
                        path: &prefix,
                    })?;
                    // Admission above qualifies this actual APFS volume. Plain
                    // mkdir allocates new directory objects, not directory
                    // aliases. Every materialized parent still needs its own
                    // namespace binding and native SOURCE admission at use.
                    if !ancestry::unique_directory_parents(directory.as_raw_fd())? {
                        return self.refuse_native_effect_blocking(path);
                    }
                    return Ok(());
                }
                Err(error) => return Err(error),
            }
            rooted.verify_namespace_directory(&prefix, directory.as_raw_fd())?;
        }
        rooted.verify_namespace_directory(parent_path, directory.as_raw_fd())?;
        self.admit_native_effect_blocking(SourceEffect {
            kind,
            parent: directory.as_raw_fd(),
            name: path.file_name().ok_or(RootedFsError::InvalidRelativePath)?,
            object: None,
            path,
        })
    }

    pub(super) fn admit_native_effect_blocking(&self, effect: SourceEffect<'_>) -> Result<()> {
        self.qualify_native_effect_blocking(effect).map(|_| ())
    }

    fn qualify_native_effect_blocking(
        &self,
        effect: SourceEffect<'_>,
    ) -> Result<native_object::NativeObjectKey> {
        self.validate_originals_blocking()?;
        let unknown = || RootedFsError::SourceEffectUnqualified(effect.path.to_path_buf());
        let parent = object_key(effect.parent, None)?.ok_or_else(unknown)?;
        // The containing namespace must not be any original TREE data DIR.
        // Selected-leaf parent anchors remain neutral. Object absence is not
        // ancestry closure for recursive retirement or directory policy setters.
        let mut candidates = vec![parent];
        match effect.kind {
            SourceEffectKind::LeafPublication => {
                if let Some(stat) = stat_at_optional(effect.parent, effect.name)? {
                    if !matches!(stat.st_mode & libc::S_IFMT, libc::S_IFREG | libc::S_IFLNK) {
                        return Err(unknown());
                    }
                    let old = object_key(effect.parent, Some(effect.name))?.ok_or_else(unknown)?;
                    if stat.st_mode & libc::S_IFMT == libc::S_IFLNK
                        && !self.inventory.excludes_operator_links_blocking(old)?
                    {
                        return Err(unknown());
                    }
                    candidates.push(old);
                }
            }
            SourceEffectKind::PrivateLeafMetadata => {
                let fd = effect.object.ok_or_else(unknown)?;
                let kind = stat_fd(fd)?.st_mode & libc::S_IFMT;
                if !matches!(kind, libc::S_IFREG | libc::S_IFLNK) {
                    return Err(unknown());
                }
                let object = object_key(fd, None)?.ok_or_else(unknown)?;
                if kind == libc::S_IFLNK
                    && !self.inventory.excludes_operator_links_blocking(object)?
                {
                    return Err(unknown());
                }
                candidates.push(object);
            }
            SourceEffectKind::SymlinkMetadata => {
                let opened = if effect.object.is_none() {
                    Some(open_symlink_at(effect.parent, effect.name)?)
                } else {
                    None
                };
                let fd = effect
                    .object
                    .or_else(|| opened.as_ref().map(AsRawFd::as_raw_fd))
                    .ok_or_else(unknown)?;
                let held = stat_fd(fd)?;
                let named = stat_at_optional(effect.parent, effect.name)?.ok_or_else(unknown)?;
                let object = object_key(fd, None)?.ok_or_else(unknown)?;
                if held.st_mode & libc::S_IFMT != libc::S_IFLNK
                    || !staging::same_observation(&held, &named)
                    || object_key(effect.parent, Some(effect.name))? != Some(object)
                    || !self.inventory.excludes_operator_links_blocking(object)?
                {
                    return Err(unknown());
                }
                candidates.push(object);
            }
            SourceEffectKind::PrivateDirectoryMetadata => {
                let fd = effect.object.ok_or_else(unknown)?;
                let held = stat_fd(fd)?;
                let named = stat_at_optional(effect.parent, effect.name)?.ok_or_else(unknown)?;
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
                if object_key(effect.parent, Some(effect.name))? != Some(object) {
                    return Err(unknown());
                }
                candidates.push(object);
            }
            SourceEffectKind::DirectoryNamespaceChange => {
                if stat_at_optional(effect.parent, effect.name)?.is_some() {
                    return Err(unknown());
                }
            }
            SourceEffectKind::NamespaceRetirement => {
                let stat = stat_at_optional(effect.parent, effect.name)?.ok_or_else(unknown)?;
                // Neither leaf kind contains a subtree. A LINK can still be
                // original operator provenance outside the physical inventory.
                if !matches!(stat.st_mode & libc::S_IFMT, libc::S_IFREG | libc::S_IFLNK) {
                    return Err(unknown());
                }
                let object = object_key(effect.parent, Some(effect.name))?.ok_or_else(unknown)?;
                if stat.st_mode & libc::S_IFMT == libc::S_IFLNK
                    && !self.inventory.excludes_operator_links_blocking(object)?
                {
                    return Err(unknown());
                }
                candidates.push(object);
            }
            SourceEffectKind::InodeMetadata => {
                let named = stat_at_optional(effect.parent, effect.name)?.ok_or_else(unknown)?;
                // Regular objects cannot contain SOURCE ancestors. Directories
                // still require operator-binding provenance, not census absence.
                if named.st_mode & libc::S_IFMT != libc::S_IFREG {
                    return Err(unknown());
                }
                let named_key =
                    object_key(effect.parent, Some(effect.name))?.ok_or_else(unknown)?;
                if let Some(fd) = effect.object {
                    if stat_fd(fd)?.st_mode & libc::S_IFMT != libc::S_IFREG
                        || object_key(fd, None)? != Some(named_key)
                    {
                        return Err(unknown());
                    }
                }
                candidates.push(named_key);
            }
            SourceEffectKind::DirectoryPolicy => {
                let named = stat_at_optional(effect.parent, effect.name)?.ok_or_else(unknown)?;
                let opened = if effect.object.is_none() {
                    Some(source_observations::operator::directory_at(
                        effect.parent,
                        effect.name,
                    )?)
                } else {
                    None
                };
                let fd = effect
                    .object
                    .or_else(|| opened.as_ref().map(AsRawFd::as_raw_fd))
                    .ok_or_else(unknown)?;
                let held = stat_fd(fd)?;
                let object = object_key(fd, None)?.ok_or_else(unknown)?;
                if held.st_mode & libc::S_IFMT != libc::S_IFDIR
                    || !staging::same_inode(&held, &named)
                    || object_key(effect.parent, Some(effect.name))? != Some(object)
                    || !self
                        .inventory
                        .excludes_directory_ancestors_blocking(object)?
                {
                    return Err(unknown());
                }
                candidates.push(object);
            }
            SourceEffectKind::DirectoryTimestamp => {
                let named = stat_at_optional(effect.parent, effect.name)?.ok_or_else(unknown)?;
                if named.st_mode & libc::S_IFMT != libc::S_IFDIR {
                    return Err(unknown());
                }
                let named_key =
                    object_key(effect.parent, Some(effect.name))?.ok_or_else(unknown)?;
                if let Some(fd) = effect.object {
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
                let fd = effect.object.ok_or_else(unknown)?;
                let held = stat_fd(fd)?;
                let named = stat_at_optional(effect.parent, effect.name)?.ok_or_else(unknown)?;
                if held.st_mode & libc::S_IFMT != libc::S_IFREG
                    || !staging::same_observation(&held, &named)
                {
                    return Err(unknown());
                }
                let object = object_key(fd, None)?.ok_or_else(unknown)?;
                if object_key(effect.parent, Some(effect.name))? != Some(object) {
                    return Err(unknown());
                }
                candidates.push(object);
            }
            SourceEffectKind::Unknown => return Err(unknown()),
        }
        if !self.inventory.excludes_objects_blocking(&candidates)? {
            return Err(unknown());
        }
        // Recheck original bindings after census; held directory identity
        // alone must not hide a relocation during enumeration.
        self.validate_originals_blocking()?;
        Ok(parent)
    }

    pub(crate) fn complete_root_declarations(
        &self,
        destination: &operand::DestinationRoot,
    ) -> Result<()> {
        match destination {
            operand::DestinationRoot::Present(_) => Ok(()),
            operand::DestinationRoot::Pending(pending) => pending.complete_source_effects(self),
        }
    }

    pub(super) fn same_source(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.inventory, &other.inventory)
    }

    pub(super) fn refuse_native_effect_blocking(&self, path: &Path) -> Result<()> {
        self.validate_originals_blocking()?;
        Err(RootedFsError::SourceEffectUnqualified(path.to_path_buf()))
    }
}

impl PrivateCreationAdmission {
    pub(super) fn verify_parent_blocking(&self, parent: RawFd, path: &Path) -> Result<()> {
        match self {
            Self::Unguarded => Ok(()),
            Self::Qualified(receipt) if object_key(parent, None)? == Some(receipt.parent) => Ok(()),
            Self::Qualified(_) => Err(RootedFsError::SourceEffectUnqualified(path.to_path_buf())),
        }
    }
}

impl RootedFs {
    pub(super) fn admit_private_creation_blocking(
        &self,
        effect: SourceEffect<'_>,
    ) -> Result<PrivateCreationAdmission> {
        match &self.local_source_effects {
            Some(effects) => {
                // Cleanup needs original DST ancestry even when SOURCE becomes
                // terminal. Parent identity alone cannot preserve that proof.
                if self.destination_namespace.is_none() {
                    return Err(RootedFsError::SourceEffectUnqualified(
                        effect.path.to_path_buf(),
                    ));
                }
                self.require_namespace_sealed()?;
                Ok(PrivateCreationAdmission::Qualified(
                    PrivateCreationReceipt {
                        parent: effects.qualify_native_effect_blocking(effect)?,
                    },
                ))
            }
            None => Ok(PrivateCreationAdmission::Unguarded),
        }
    }

    pub(super) fn require_source_effect_blocking(&self, effect: SourceEffect<'_>) -> Result<()> {
        if let Some(effects) = &self.local_source_effects {
            effects.admit_native_effect_blocking(effect)?;
            #[cfg(test)]
            self.pause_mutation(PublicationPausePoint::AfterSourceValidation);
        }
        Ok(())
    }
}

impl operand::DestinationRoot {
    pub fn with_local_source_effects(mut self, effects: LocalSourceEffects) -> Self {
        match &mut self {
            Self::Present(rooted) => rooted.local_source_effects = Some(effects),
            Self::Pending(pending) => pending.bind_local_source_effects(effects),
        }
        self
    }
}
