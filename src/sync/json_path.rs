//! JSON paths retain the usual string shape when Unicode is valid. Otherwise
//! use an explicitly tagged native representation, never a lossy replacement.

use serde::ser::SerializeSeq;
use serde::{Serialize, Serializer};
use std::path::{Path, PathBuf};

pub(super) fn serialize_path<S: Serializer>(path: &Path, serializer: S) -> Result<S::Ok, S::Error> {
    if let Some(text) = path.to_str() {
        return serializer.serialize_str(text);
    }
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        #[derive(Serialize)]
        struct NativePath<'a> {
            encoding: &'static str,
            bytes: &'a [u8],
        }
        NativePath {
            encoding: "unix_bytes",
            bytes: path.as_os_str().as_bytes(),
        }
        .serialize(serializer)
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        #[derive(Serialize)]
        struct NativePath {
            encoding: &'static str,
            units: Vec<u16>,
        }
        NativePath {
            encoding: "windows_utf16",
            units: path.as_os_str().encode_wide().collect(),
        }
        .serialize(serializer)
    }
    #[cfg(not(any(unix, windows)))]
    {
        Err(serde::ser::Error::custom(
            "native JSON path encoding is unsupported on this platform",
        ))
    }
}

pub(super) fn serialize_paths<S: Serializer>(
    paths: &[PathBuf],
    serializer: S,
) -> Result<S::Ok, S::Error> {
    struct PathRef<'a>(&'a Path);
    impl Serialize for PathRef<'_> {
        fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
            serialize_path(self.0, serializer)
        }
    }
    let mut sequence = serializer.serialize_seq(Some(paths.len()))?;
    for path in paths {
        sequence.serialize_element(&PathRef(path))?;
    }
    sequence.end()
}
