//! Local operand binding and selected-source effect protection.
use crate::cli::SymlinkMode;
use crate::error::{Result, SyncError};
use std::path::{Path, PathBuf};
use sy::endpoint::source_root::SourceRoot;
use sy::engine::domain::{RelativePath, SyncScope};
use sy::rooted_fs::operand::{BoundSource, SourceShape};

pub(super) async fn resolve(
    source: PathBuf,
    destination: PathBuf,
    contents: bool,
    links: SymlinkMode,
) -> Result<(SourceRoot, PathBuf, SyncScope)> {
    let bound = if links == SymlinkMode::Follow {
        BoundSource::open_local_following(source, contents).await
    } else {
        BoundSource::open_local(source, contents).await
    }
    .map_err(sy::endpoint::local::map_source_rooted_fs_error)?;
    let authority = SourceRoot::from_rooted(bound.rooted)
        .map_err(sy::endpoint::local::map_source_rooted_fs_error)?;
    tokio::task::spawn_blocking(move || {
        match bound.shape {
            SourceShape::Directory { basename } => {
                let destination =
                    basename.map_or(destination.clone(), |name| destination.join(name.as_path()));
                Ok((authority, destination, SyncScope::Tree))
            }
            SourceShape::Leaf { name: source_name } => {
                let destination = match std::fs::metadata(&destination) {
                    Ok(metadata) if metadata.is_dir() => destination.join(source_name.as_path()),
                    Ok(_) => destination,
                    // Missing/cyclic targets cannot designate a directory.
                    Err(error)
                        if error.kind() == std::io::ErrorKind::NotFound
                            || error.raw_os_error() == Some(libc::ELOOP) =>
                    {
                        destination
                    }
                    Err(error) => return Err(error.into()),
                };
                let destination_name = leaf_name(&destination)?;
                Ok((
                    authority,
                    parent(&destination).to_path_buf(),
                    SyncScope::SelectedLeaf {
                        source: source_name,
                        destination: destination_name,
                    },
                ))
            }
        }
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
