use super::{ProtocolError, Result};
use bytes::Bytes;

pub const MAX_WIRE_PATH_BYTES: usize = 64 * 1024;

/// Opaque filesystem path bytes carried by the protocol.
///
/// The wire layer does not assume UTF-8. Platform conversion happens at an
/// endpoint boundary where the remote OS is known.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct WirePath(Bytes);

impl WirePath {
    pub fn new(path: impl Into<Bytes>) -> Result<Self> {
        let path = path.into();
        validate_common(&path)?;
        Ok(Self(path))
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    pub fn into_bytes(self) -> Bytes {
        self.0
    }
}

/// Canonical slash-delimited path relative to a negotiated endpoint root.
///
/// Relative paths reject traversal, absolute paths, NULs, and ambiguous empty
/// components before they can reach endpoint-specific resolution code. This is
/// an input invariant, not the remote security boundary: the server must still
/// resolve components relative to a held root directory handle without
/// following escape symlinks.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RelativeWirePath(Bytes);

impl RelativeWirePath {
    pub fn new(path: impl Into<Bytes>) -> Result<Self> {
        let path = path.into();
        validate_common(&path)?;
        if path.is_empty() {
            return Err(ProtocolError::InvalidRelativePath("path is empty"));
        }
        if path[0] == b'/' {
            return Err(ProtocolError::InvalidRelativePath("path is absolute"));
        }

        for component in path.split(|byte| *byte == b'/') {
            if component.is_empty() {
                return Err(ProtocolError::InvalidRelativePath(
                    "path contains an empty component",
                ));
            }
            if component == b"." {
                return Err(ProtocolError::InvalidRelativePath(
                    "path contains a current-directory component",
                ));
            }
            if component == b".." {
                return Err(ProtocolError::InvalidRelativePath(
                    "path contains a parent-directory component",
                ));
            }
        }

        Ok(Self(path))
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    pub fn into_bytes(self) -> Bytes {
        self.0
    }
}

fn validate_common(path: &[u8]) -> Result<()> {
    if path.len() > MAX_WIRE_PATH_BYTES {
        return Err(ProtocolError::PathTooLong {
            len: path.len(),
            max: MAX_WIRE_PATH_BYTES,
        });
    }
    if path.contains(&0) {
        return Err(ProtocolError::InvalidField {
            field: "path",
            reason: "NUL bytes are not valid filesystem path data",
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_path_accepts_non_utf8_bytes() {
        let path = RelativeWirePath::new(Bytes::from_static(b"dir/\xfffile")).unwrap();
        assert_eq!(path.as_bytes(), b"dir/\xfffile");
    }

    #[test]
    fn relative_path_rejects_traversal_and_ambiguous_components() {
        for path in [
            b"../file".as_slice(),
            b"dir/../file".as_slice(),
            b"./file".as_slice(),
            b"dir//file".as_slice(),
            b"/absolute".as_slice(),
            b"dir/".as_slice(),
        ] {
            assert!(RelativeWirePath::new(Bytes::copy_from_slice(path)).is_err());
        }
    }

    #[test]
    fn rejects_nul_and_oversized_paths() {
        assert!(WirePath::new(Bytes::from_static(b"bad\0path")).is_err());
        assert!(WirePath::new(vec![b'a'; MAX_WIRE_PATH_BYTES + 1]).is_err());
    }
}
