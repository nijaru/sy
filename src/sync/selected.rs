//! Local operand binding and selected-source effect protection.
use crate::cli::SymlinkMode;
use crate::error::{Result, SyncError};
use crate::sync::SyncConfig;
use std::path::{Path, PathBuf};
use sy::engine::controller::{ControllerError, SyncPlan};
use sy::engine::domain::{RelativePath, SyncOp, SyncScope};

pub(super) async fn resolve(
    source: PathBuf,
    destination: PathBuf,
    contents: bool,
    links: SymlinkMode,
) -> Result<(PathBuf, PathBuf, SyncScope)> {
    tokio::task::spawn_blocking(move || {
        let observed = std::fs::symlink_metadata(&source)?;
        let is_directory = if observed.file_type().is_symlink() && links == SymlinkMode::Follow {
            std::fs::metadata(&source)?.is_dir()
        } else {
            observed.is_dir()
        };
        if is_directory {
            let destination = if contents {
                destination
            } else if let Some(name) = source.file_name() {
                destination.join(name)
            } else {
                destination
            };
            return Ok((source, destination, SyncScope::Tree));
        }
        let source_name = leaf_name(&source)?;
        let destination = match std::fs::metadata(&destination) {
            Ok(metadata) if metadata.is_dir() => destination.join(source_name.as_path()),
            Ok(_) => destination,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => destination,
            Err(error) => return Err(error.into()),
        };
        let destination_name = leaf_name(&destination)?;
        Ok((
            parent(&source).to_path_buf(),
            parent(&destination).to_path_buf(),
            SyncScope::SelectedLeaf {
                source: source_name,
                destination: destination_name,
            },
        ))
    })
    .await
    .map_err(|error| SyncError::Io(std::io::Error::other(error)))?
}

fn parent(path: &Path) -> &Path {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."))
}

fn leaf_name(path: &Path) -> Result<RelativePath> {
    let name = path.file_name().ok_or_else(|| SyncError::InvalidPath {
        path: path.to_path_buf(),
    })?;
    RelativePath::new(PathBuf::from(name)).map_err(|_| SyncError::InvalidPath {
        path: path.to_path_buf(),
    })
}

pub(super) async fn validate_effects(
    plan: &mut SyncPlan,
    source_root: &Path,
    source: &RelativePath,
    destination_root: &Path,
    config: &SyncConfig,
) -> Result<()> {
    let source = source_root.join(source.as_path());
    let destination_root = destination_root.to_path_buf();
    let backup_dir = super::local::backup_dir(config, &destination_root);
    let backup = config.backup.is_some();
    let suffix = config.suffix.clone();
    let unchanged_mutates = config.preserve.xattrs || config.preserve.acls || config.preserve.flags;
    plan.validate_operations(move |operation| {
        let source = source.clone();
        let destination_root = destination_root.clone();
        let backup_dir = backup_dir.clone();
        let suffix = suffix.clone();
        async move {
            if matches!(operation, SyncOp::Skip { .. })
                || (matches!(operation, SyncOp::Unchanged { .. }) && !unchanged_mutates)
            {
                return Ok(());
            }
            tokio::task::spawn_blocking(move || {
                protect_source(&source, &destination_root.join(operation.path().as_path()))?;
                let existing = match &operation {
                    SyncOp::Update { destination, .. }
                    | SyncOp::Replace { destination, .. }
                    | SyncOp::Unchanged { destination, .. } => Some(destination),
                    _ => None,
                };
                if let Some(existing) = existing.filter(|entry| backup && entry.is_file()) {
                    let path = sy::remote::local_executor::backup_destination_path(
                        &destination_root,
                        &existing.path,
                        backup_dir.as_deref(),
                        &suffix,
                    )
                    .map_err(std::io::Error::other)?;
                    protect_source(&source, &path)?;
                }
                Ok::<(), std::io::Error>(())
            })
            .await
            .map_err(|error| ControllerError::backend("selected source protection", error))?
            .map_err(|error| ControllerError::backend("selected source protection", error))
        }
    })
    .await
    .map_err(super::policy::map_controller_error)
}

fn protect_source(source: &Path, effect: &Path) -> std::io::Result<()> {
    // Canonical parents catch aliases through directory symlinks. Comparing
    // inode observations also catches hardlinks with distinct physical names.
    let source_name = std::fs::canonicalize(parent(source))?.join(
        source
            .file_name()
            .ok_or_else(|| std::io::Error::other("missing selected source name"))?,
    );
    let effect_name = resolved_effect(effect)?;
    let source_metadata = std::fs::symlink_metadata(source)?;
    let effect_metadata = match std::fs::symlink_metadata(effect) {
        Ok(metadata) => Some(metadata),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error),
    };
    #[cfg(unix)]
    let inode_alias = {
        use std::os::unix::fs::MetadataExt;
        effect_metadata.as_ref().is_some_and(|metadata| {
            metadata.dev() == source_metadata.dev() && metadata.ino() == source_metadata.ino()
        })
    };
    #[cfg(not(unix))]
    let inode_alias = {
        let _ = (source_metadata, effect_metadata);
        false
    };
    let followed_alias = std::fs::canonicalize(source)
        .ok()
        .zip(std::fs::canonicalize(effect).ok())
        .is_some_and(|(source, effect)| source == effect);
    if source_name == effect_name || inode_alias || followed_alias {
        return Err(std::io::Error::other(format!(
            "planned destination effect {} aliases selected source {}",
            effect.display(),
            source.display()
        )));
    }
    Ok(())
}

fn resolved_effect(path: &Path) -> std::io::Result<PathBuf> {
    match std::fs::canonicalize(parent(path)) {
        Ok(parent) => Ok(parent.join(
            path.file_name()
                .ok_or_else(|| std::io::Error::other("missing effect name"))?,
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let ancestor = resolved_effect(parent(path))?;
            Ok(ancestor.join(
                path.file_name()
                    .ok_or_else(|| std::io::Error::other("missing effect name"))?,
            ))
        }
        Err(error) => Err(error),
    }
}
