//! APFS physical ROOT-prefix qualification for directory policy effects.
//! A pathname selects the prefix; native parent IDs must certify every edge.
use super::*;
use native_object::NativeObjectKey;

#[cfg(target_os = "macos")]
struct Directory {
    object: NativeObjectKey,
    parent_id: u64,
}

#[cfg(target_os = "macos")]
fn directory(fd: RawFd, object: NativeObjectKey) -> Result<Option<Directory>> {
    let common = libc::ATTR_CMN_RETURNED_ATTRS | libc::ATTR_CMN_PARENTID;
    let mut attrs = libc::attrlist {
        bitmapcount: libc::ATTR_BIT_MAP_COUNT,
        reserved: 0,
        commonattr: common,
        volattr: 0,
        dirattr: 0,
        fileattr: 0,
        forkattr: 0,
    };
    // Length, returned attribute set, then the 64-bit parent ID.
    let mut bytes = [0u32; 8];
    // SAFETY: fd is held; attrs and aligned output have their declared sizes.
    if unsafe {
        libc::fgetattrlist(
            fd,
            (&mut attrs as *mut libc::attrlist).cast(),
            bytes.as_mut_ptr().cast(),
            std::mem::size_of_val(&bytes),
            libc::FSOPT_RETURN_REALDEV,
        )
    } != 0
    {
        return Err(std::io::Error::last_os_error().into());
    }
    if bytes[..6] != [32, common, 0, 0, 0, 0] {
        return Ok(None);
    }
    let mut parent = [0u8; 8];
    parent[..4].copy_from_slice(&bytes[6].to_ne_bytes());
    parent[4..].copy_from_slice(&bytes[7].to_ne_bytes());
    Ok(Some(Directory {
        object,
        parent_id: u64::from_ne_bytes(parent),
    }))
}

/// APFS has one native parent per directory. Require the volume capability
/// query to corroborate the absence of directory hardlinks, not just its name.
#[cfg(target_os = "macos")]
fn unique_directory_parents(fd: RawFd) -> Result<bool> {
    let mut attrs = libc::attrlist {
        bitmapcount: libc::ATTR_BIT_MAP_COUNT,
        reserved: 0,
        commonattr: 0,
        volattr: libc::ATTR_VOL_INFO | libc::ATTR_VOL_CAPABILITIES,
        dirattr: 0,
        fileattr: 0,
        forkattr: 0,
    };
    let mut bytes = [0u32; 9];
    // SAFETY: fd is held; the aligned buffer holds length and capabilities.
    if unsafe {
        libc::fgetattrlist(
            fd,
            (&mut attrs as *mut libc::attrlist).cast(),
            bytes.as_mut_ptr().cast(),
            std::mem::size_of_val(&bytes),
            0,
        )
    } != 0
    {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(bytes[0] == 36
        && bytes[5] & libc::VOL_CAP_FMT_DIR_HARDLINKS != 0
        && bytes[1] & libc::VOL_CAP_FMT_DIR_HARDLINKS == 0)
}

#[cfg(target_os = "macos")]
fn file_id(object: NativeObjectKey) -> u64 {
    let mut bytes = [0; 8];
    bytes.copy_from_slice(&object.0[24..]);
    u64::from_ne_bytes(bytes)
}

#[cfg(target_os = "macos")]
fn physical_address(root: RawFd) -> Result<OsString> {
    let mut bytes = [0u8; libc::PATH_MAX as usize];
    // SAFETY: root is held; this request writes at most PATH_MAX bytes.
    if unsafe { libc::fcntl(root, libc::F_GETPATH_NOFIRMLINK, bytes.as_mut_ptr()) } == -1 {
        return Err(std::io::Error::last_os_error().into());
    }
    let bytes = std::ffi::CStr::from_bytes_until_nul(&bytes)
        .map_err(|_| std::io::Error::other("unterminated native ROOT address"))?;
    Ok(OsStr::from_bytes(bytes.to_bytes()).to_os_string())
}

/// Additional ancestor predicate, never a substitute for complete original
/// object exclusion. Only the native FD-derived ROOT prefix is inspected.
#[cfg(target_os = "macos")]
pub(in crate::rooted_fs) fn excludes(
    root: RawFd,
    expected: NativeObjectKey,
    candidate: NativeObjectKey,
) -> Result<bool> {
    if expected.0[..24] != candidate.0[..24]
        || object_key(root, None)? != Some(expected)
        || !unique_directory_parents(root)?
    {
        return Ok(false);
    }
    let address = physical_address(root)?;
    let bytes = address.as_bytes();
    if !bytes.starts_with(b"/") {
        return Ok(false);
    }
    let mut cursor = source_observations::operator::directory_at(libc::AT_FDCWD, OsStr::new("/"))?;
    let mut parent: Option<NativeObjectKey> = None;
    let mut found = false;
    for name in std::iter::once(b".".as_slice()).chain(
        bytes
            .split(|byte| *byte == b'/')
            .filter(|name| !name.is_empty()),
    ) {
        if name == b".." || name.contains(&0) {
            return Ok(false);
        }
        cursor = source_observations::operator::directory_at(
            cursor.as_raw_fd(),
            OsStr::from_bytes(name),
        )?;
        let object = object_key(cursor.as_raw_fd(), None)?;
        if let Some(object) = object.filter(|object| object.0[..24] == expected.0[..24]) {
            let Some(current) = directory(cursor.as_raw_fd(), object)? else {
                return Ok(false);
            };
            if let Some(parent) = parent {
                if file_id(current.object) == 2 || current.parent_id != file_id(parent) {
                    return Ok(false);
                }
            } else if file_id(current.object) != 2 {
                // APFS Reference: ROOT_DIR_INO_NUM=2. The native API reports
                // this root as its own parent; disk parent 1 is a sentinel.
                // A partial chain is not a negative ancestry certificate.
                return Ok(false);
            }
            found |= current.object == candidate;
            parent = Some(current.object);
        } else if parent.is_some() {
            return Ok(false);
        }
    }
    if parent != Some(expected)
        || physical_address(root)? != address
        || object_key(root, None)? != Some(expected)
    {
        return Ok(false);
    }
    let Some(reached) = directory(cursor.as_raw_fd(), expected)? else {
        return Ok(false);
    };
    let Some(held) = directory(root, expected)? else {
        return Ok(false);
    };
    Ok(reached.parent_id == held.parent_id && !found)
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;
    #[test]
    fn physical_root_chain_protects_native_ancestors_not_independent_directories() {
        let fixture = tempfile::tempdir().unwrap();
        std::fs::create_dir(fixture.path().join("source")).unwrap();
        std::fs::create_dir(fixture.path().join("destination")).unwrap();
        let root = File::open(fixture.path().join("source")).unwrap();
        let parent = File::open(fixture.path()).unwrap();
        let destination = File::open(fixture.path().join("destination")).unwrap();
        let expected = object_key(root.as_raw_fd(), None).unwrap().unwrap();
        let ancestor = object_key(parent.as_raw_fd(), None).unwrap().unwrap();
        let independent = object_key(destination.as_raw_fd(), None).unwrap().unwrap();
        assert!(excludes(root.as_raw_fd(), expected, independent).unwrap());
        assert!(!excludes(root.as_raw_fd(), expected, expected).unwrap());
        assert!(!excludes(root.as_raw_fd(), expected, ancestor).unwrap());
        let mut foreign_view = expected;
        foreign_view.0[16] ^= 1;
        assert!(!excludes(root.as_raw_fd(), foreign_view, independent).unwrap());
        assert!(!excludes(root.as_raw_fd(), expected, foreign_view).unwrap());
    }
}

#[cfg(not(target_os = "macos"))]
pub(in crate::rooted_fs) fn excludes(
    _root: RawFd,
    _expected: NativeObjectKey,
    _candidate: NativeObjectKey,
) -> Result<bool> {
    Ok(false)
}
