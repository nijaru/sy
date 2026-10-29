//! Domain-scoped physical parents, not a lexical prefix or global alias proof.
use super::*;

pub(in crate::rooted_fs::local_source_effects) fn unique_directory_parents(
    fd: RawFd,
) -> Result<bool> {
    // The native-key lane admits only ext2/3/4, XFS and Btrfs. These backends
    // do not permit directory hardlinks; separate mount/subvolume views do not
    // qualify as the same domain.
    Ok(object_key(fd, None)?.is_some())
}

mod proc_address;
use proc_address::ProcAddress;

pub(in crate::rooted_fs) fn excludes(
    root: RawFd,
    expected: NativeObjectKey,
    candidate: NativeObjectKey,
) -> Result<bool> {
    if expected.0[..24] != candidate.0[..24] || object_key(root, None)? != Some(expected) {
        return Ok(false);
    }
    let Some(address) = ProcAddress::new(root)? else {
        return Ok(false);
    };
    let Some(original_address) = address.read()? else {
        return Ok(false);
    };
    let mut cursor = source_observations::operator::directory_at(root, OsStr::new("."))?;
    let mut object = expected;
    let mut found = false;
    let mut complete = false;
    for _ in 0..crate::protocol::MAX_WIRE_COMPONENTS {
        found |= object == candidate;
        // Deliberate native ancestry metadata query, not SOURCE following.
        let next =
            source_observations::operator::directory_at(cursor.as_raw_fd(), OsStr::new(".."))?;
        let Some(next_object) = object_key(next.as_raw_fd(), None)? else {
            return Ok(false);
        };
        if next_object == object {
            // '..' also stops at chroot, which may conceal same-domain native
            // ancestors. Only an independently known filesystem root qualifies.
            complete = native_object::linux::directory_domain_root(object);
            break;
        }
        if next_object.0[..24] != expected.0[..24] {
            complete = true;
            break;
        }
        cursor = next;
        object = next_object;
    }
    // A scan is not a snapshot. Rechecking the genuine root's full FD address
    // catches persistent relocation of an intermediate ancestor, not just '..'.
    Ok(complete
        && !found
        && address.read()?.as_ref() == Some(&original_address)
        && object_key(root, None)? == Some(expected))
}

#[cfg(test)]
mod tests;
