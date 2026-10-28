//! LOCAL exclusion admission through original SOURCE and held native effects.
use super::*;
use source_observations::inventory::{SourceExclusion, SourceInventory};
pub(super) mod ancestry;
mod empty_directory;
mod footprint;
pub(super) mod native_object;
use native_object::object_key;
#[cfg(all(test, target_os = "macos"))]
mod private_file_tests;
#[cfg(all(test, target_os = "macos"))]
mod symlink_tests;
#[cfg(test)]
#[path = "local_source_effects_tests.rs"]
mod tests;

/// Directory retirement is nonrecursive and requires a genuinely empty OLD.
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
            .map_err(source_error)
    }

    pub(super) fn validate_lifetime_blocking(&self) -> Result<()> {
        self.inventory
            .validate_lifetime_blocking()
            .map_err(source_error)
    }

    pub(super) fn excludes_private_creator_blocking(&self, creator: RawFd) -> Result<bool> {
        self.inventory
            .excludes_private_creator_blocking(creator)
            .map_err(source_error)
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
        let unknown = || RootedFsError::SourceEffectUnqualified(effect.path.to_path_buf());
        let footprint = match effect.capture_footprint() {
            Ok(footprint) => footprint,
            Err(error) => {
                // Even an unsupported destination footprint must not hide a
                // changed SOURCE. Qualified footprints validate all originals
                // in the fresh exact census instead of walking them twice more.
                self.validate_originals_blocking()?;
                return Err(error);
            }
        };
        let independent = self
            .inventory
            .excludes_objects_blocking(&footprint.candidates, footprint.additional)
            .map_err(source_error)?;
        if !independent {
            return Err(unknown());
        }
        if let Some(directory) = footprint.empty_directory {
            directory.verify(&effect)?;
        }
        Ok(footprint.parent)
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

// SOURCE failures cross destination-role error mapping during publication.
fn source_error(error: RootedFsError) -> RootedFsError {
    match error {
        RootedFsError::RootChanged(_) => RootedFsError::Io(std::io::Error::other(error)),
        error => error,
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
