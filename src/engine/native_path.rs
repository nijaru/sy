//! Native filesystem-name bytes for disk-backed engine spools.
//!
//! Plan journals and namespace preflight runs spool relative paths through
//! temporary files. Both must round-trip non-UTF-8 Unix names and Windows wide
//! names without loss, so the platform encoding has one owner instead of
//! diverging per spool.

use std::ffi::{OsStr, OsString};
use std::io;
use std::path::PathBuf;

#[cfg(unix)]
pub(crate) fn encode(path: &OsStr) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    path.as_bytes().to_vec()
}

#[cfg(unix)]
pub(crate) fn decode(bytes: &[u8]) -> io::Result<PathBuf> {
    use std::os::unix::ffi::OsStringExt;
    Ok(PathBuf::from(OsString::from_vec(bytes.to_vec())))
}

#[cfg(windows)]
pub(crate) fn encode(path: &OsStr) -> Vec<u8> {
    use std::os::windows::ffi::OsStrExt;
    let mut bytes = Vec::new();
    for unit in path.encode_wide() {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }
    bytes
}

#[cfg(windows)]
pub(crate) fn decode(bytes: &[u8]) -> io::Result<PathBuf> {
    use std::os::windows::ffi::OsStringExt;
    if bytes.len() % 2 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "odd byte length in encoded Windows path",
        ));
    }
    let wide = bytes
        .chunks_exact(2)
        .map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]]))
        .collect::<Vec<_>>();
    Ok(PathBuf::from(OsString::from_wide(&wide)))
}

#[cfg(not(any(unix, windows)))]
pub(crate) fn encode(path: &OsStr) -> Vec<u8> {
    path.to_string_lossy().into_owned().into_bytes()
}

#[cfg(not(any(unix, windows)))]
pub(crate) fn decode(bytes: &[u8]) -> io::Result<PathBuf> {
    String::from_utf8(bytes.to_vec())
        .map(PathBuf::from)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "non-Unicode path bytes"))
}
