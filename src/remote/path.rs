use crate::engine::domain::{InvalidRelativePath, RelativePath};
use crate::protocol::{Platform, PlatformOs, ProtocolError, RelativeWirePath};
use std::ffi::{OsStr, OsString};
use std::path::{Component, Path, PathBuf};

#[derive(Debug, thiserror::Error)]
pub enum RemotePathError {
    #[error(transparent)]
    Protocol(#[from] ProtocolError),

    #[error(transparent)]
    InvalidRelativePath(#[from] InvalidRelativePath),

    #[error(
        "peer path encoding {peer:?} cannot preserve local ordered-path semantics on {local:?}"
    )]
    UnsupportedPathEncoding { local: PlatformOs, peer: PlatformOs },

    #[error("wire path component is not one native relative-name component")]
    InvalidPathComponent,

    #[error("wire path contains a NUL code unit")]
    PathContainsNul,
}

pub type Result<T> = std::result::Result<T, RemotePathError>;

pub fn ensure_compatible_path_encoding(peer: PlatformOs) -> Result<()> {
    let local = Platform::current().os;
    if compatible_path_encoding(local, peer) {
        Ok(())
    } else {
        Err(RemotePathError::UnsupportedPathEncoding { local, peer })
    }
}

fn compatible_path_encoding(local: PlatformOs, peer: PlatformOs) -> bool {
    matches!(
        (local, peer),
        (
            PlatformOs::Linux | PlatformOs::Macos,
            PlatformOs::Linux | PlatformOs::Macos
        ) | (PlatformOs::Windows, PlatformOs::Windows)
    )
}

#[cfg(unix)]
pub fn encode_relative_path(path: &Path) -> Result<RelativeWirePath> {
    use std::os::unix::ffi::OsStrExt;

    if path
        .components()
        .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(RemotePathError::InvalidPathComponent);
    }
    RelativeWirePath::from_components(path.components().filter_map(|component| match component {
        Component::Normal(name) => Some(name.as_bytes()),
        _ => None,
    }))
    .map_err(RemotePathError::from)
}

#[cfg(windows)]
pub fn encode_relative_path(path: &Path) -> Result<RelativeWirePath> {
    use std::os::windows::ffi::OsStrExt;

    let mut encoded = Vec::new();
    for component in path.components() {
        let Component::Normal(name) = component else {
            return Err(RemotePathError::InvalidPathComponent);
        };
        let mut bytes = Vec::new();
        for unit in name.encode_wide() {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
        encoded.push(bytes);
    }
    RelativeWirePath::from_components(encoded).map_err(RemotePathError::from)
}

#[cfg(not(any(unix, windows)))]
pub fn encode_relative_path(_path: &Path) -> Result<RelativeWirePath> {
    Err(RemotePathError::UnsupportedPathEncoding {
        local: Platform::current().os,
        peer: Platform::current().os,
    })
}

pub fn decode_relative_path(path: RelativeWirePath, peer: PlatformOs) -> Result<RelativePath> {
    ensure_compatible_path_encoding(peer)?;
    let mut native = PathBuf::new();
    for component in path.components() {
        let name = decode_native_component(component, peer)?;
        validate_native_component(&name)?;
        native.push(name);
    }
    RelativePath::new(native).map_err(RemotePathError::from)
}

fn validate_native_component(component: &OsStr) -> Result<()> {
    let mut components = Path::new(component).components();
    match (components.next(), components.next()) {
        (Some(Component::Normal(_)), None) => Ok(()),
        _ => Err(RemotePathError::InvalidPathComponent),
    }
}

#[cfg(unix)]
fn decode_native_component(bytes: &[u8], peer: PlatformOs) -> Result<OsString> {
    use std::os::unix::ffi::OsStringExt;

    ensure_compatible_path_encoding(peer)?;
    if bytes.contains(&0) {
        return Err(RemotePathError::PathContainsNul);
    }
    Ok(OsString::from_vec(bytes.to_vec()))
}

#[cfg(windows)]
fn decode_native_component(bytes: &[u8], peer: PlatformOs) -> Result<OsString> {
    use std::os::windows::ffi::OsStringExt;

    ensure_compatible_path_encoding(peer)?;
    if bytes.len() % 2 != 0 {
        return Err(RemotePathError::InvalidPathComponent);
    }
    let units = bytes
        .chunks_exact(2)
        .map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]]))
        .collect::<Vec<_>>();
    if units.contains(&0) {
        return Err(RemotePathError::PathContainsNul);
    }
    Ok(OsString::from_wide(&units))
}

#[cfg(not(any(unix, windows)))]
fn decode_native_component(_bytes: &[u8], peer: PlatformOs) -> Result<OsString> {
    Err(RemotePathError::UnsupportedPathEncoding {
        local: Platform::current().os,
        peer,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_path_round_trip_preserves_components() {
        let path = Path::new("dir").join("file.bin");
        let encoded = encode_relative_path(&path).unwrap();
        let decoded = decode_relative_path(encoded, Platform::current().os).unwrap();
        assert_eq!(decoded.as_path(), path);
    }

    #[test]
    fn rejects_cross_family_ordering() {
        let peer = match Platform::current().os {
            PlatformOs::Windows => PlatformOs::Linux,
            _ => PlatformOs::Windows,
        };
        assert!(matches!(
            ensure_compatible_path_encoding(peer),
            Err(RemotePathError::UnsupportedPathEncoding { .. })
        ));
    }
}
