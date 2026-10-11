//! Held-inode comparison reads numeric qualifiers, never reverse NSS names.
//! exacl still owns wire text, desired entries and SET; its public read API
//! resolves native IDs to names, which need not resolve back to the same IDs.
use super::fd_alias_path;
use exacl::{AclEntry, AclEntryKind, Flag, Perm};
use std::ffi::CString;
use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;
use std::os::raw::{c_char, c_int, c_uint, c_void};
use std::os::unix::ffi::OsStrExt;
use std::ptr;

// Linux libacl ABI, matching exacl's bindings_linux.rs.
type AclT = *mut c_void;
type EntryT = *mut c_void;
type PermsetT = *mut c_void;
const ACL_USER_OBJ: c_int = 1;
const ACL_USER: c_int = 2;
const ACL_GROUP_OBJ: c_int = 4;
const ACL_GROUP: c_int = 8;
const ACL_MASK: c_int = 16;
const ACL_OTHER: c_int = 32;
const ACL_TYPE_DEFAULT: c_uint = 0x4000;
const ACL_FIRST_ENTRY: c_int = 0;
const ACL_NEXT_ENTRY: c_int = 1;

#[link(name = "acl")]
extern "C" {
    fn acl_get_fd(fd: c_int) -> AclT;
    fn acl_get_file(path: *const c_char, acl_type: c_uint) -> AclT;
    fn acl_free(object: *mut c_void) -> c_int;
    fn acl_get_entry(acl: AclT, id: c_int, entry: *mut EntryT) -> c_int;
    fn acl_get_tag_type(entry: EntryT, tag: *mut c_int) -> c_int;
    fn acl_get_qualifier(entry: EntryT) -> *mut c_void;
    fn acl_get_permset(entry: EntryT, permset: *mut PermsetT) -> c_int;
    fn acl_get_perm(permset: PermsetT, perm: c_uint) -> c_int;
}

struct OwnedAcl(AclT);

impl OwnedAcl {
    fn checked(raw: AclT) -> io::Result<Self> {
        if raw.is_null() {
            Err(io::Error::last_os_error())
        } else {
            Ok(Self(raw))
        }
    }

    fn append_entries(&self, flags: Flag, entries: &mut Vec<AclEntry>) -> io::Result<()> {
        let mut id = ACL_FIRST_ENTRY;
        loop {
            let mut entry = ptr::null_mut();
            // SAFETY: self owns a live acl_t; entry is writable handle storage.
            match unsafe { acl_get_entry(self.0, id, &mut entry) } {
                0 => return Ok(()),
                1 => entries.push(read_numeric_entry(entry, flags)?),
                _ => return Err(io::Error::last_os_error()),
            }
            id = ACL_NEXT_ENTRY;
        }
    }
}

impl Drop for OwnedAcl {
    fn drop(&mut self) {
        // SAFETY: this is the non-null owned allocation from acl_get_fd/file,
        // and all borrowed entry/permission handles have finished being used.
        unsafe { acl_free(self.0) };
    }
}

pub(super) fn read_numeric_entries(file: &File) -> io::Result<Vec<AclEntry>> {
    // SAFETY: file keeps the descriptor alive throughout this read.
    let access = OwnedAcl::checked(unsafe { acl_get_fd(file.as_raw_fd()) })?;
    let mut entries = Vec::new();
    access.append_entries(Flag::empty(), &mut entries)?;
    if file.metadata()?.is_dir() {
        // Linux has no acl_get_fd for defaults. The alias addresses the SAME
        // held inode, even after rename, not the original user pathname.
        let alias = CString::new(fd_alias_path(file).as_os_str().as_bytes())?;
        // SAFETY: alias is NUL-terminated and file keeps its target FD alive.
        let default = OwnedAcl::checked(unsafe { acl_get_file(alias.as_ptr(), ACL_TYPE_DEFAULT) })?;
        default.append_entries(Flag::DEFAULT, &mut entries)?;
    }
    Ok(entries)
}

fn read_numeric_entry(entry: EntryT, flags: Flag) -> io::Result<AclEntry> {
    let mut tag = 0;
    // SAFETY: entry is borrowed from the live OwnedAcl during iteration.
    if unsafe { acl_get_tag_type(entry, &mut tag) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let kind = match tag {
        ACL_USER_OBJ | ACL_USER => AclEntryKind::User,
        ACL_GROUP_OBJ | ACL_GROUP => AclEntryKind::Group,
        ACL_MASK => AclEntryKind::Mask,
        ACL_OTHER => AclEntryKind::Other,
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "unknown ACL tag",
            ))
        }
    };
    let name = if matches!(tag, ACL_USER | ACL_GROUP) {
        // SAFETY: entry has a qualifier-bearing tag and belongs to a live ACL.
        let qualifier = unsafe { acl_get_qualifier(entry).cast::<u32>() };
        if qualifier.is_null() {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: Linux uid_t/gid_t are both u32; libacl allocated this ID.
        let id = unsafe { *qualifier };
        // SAFETY: qualifier is the owned allocation from acl_get_qualifier,
        // copied above and freed exactly once, independently of the ACL.
        unsafe { acl_free(qualifier.cast()) };
        id.to_string()
    } else {
        String::new()
    };
    let mut permset = ptr::null_mut();
    // SAFETY: entry is live; permset receives a borrowed permission handle.
    if unsafe { acl_get_permset(entry, &mut permset) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let mut perms = Perm::empty();
    for bit in [Perm::READ, Perm::WRITE, Perm::EXECUTE] {
        // SAFETY: permset belongs to the live ACL; bit is a valid POSIX perm.
        match unsafe { acl_get_perm(permset, bit.bits()) } {
            0 => (),
            1 => perms |= bit,
            _ => return Err(io::Error::last_os_error()),
        }
    }
    Ok(AclEntry {
        kind,
        name,
        perms,
        flags,
        allow: true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rooted_fs::acl_state::acl_matches_fd;

    #[test]
    fn comparison_keeps_numeric_principals_defaults_and_held_fd_authority() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("directory");
        std::fs::create_dir(&path).unwrap();
        let mut expected = exacl::from_mode(0o700);
        expected.extend([
            AclEntry::allow_user("0", Perm::READ | Perm::WRITE, Flag::empty()),
            AclEntry::allow_group("0", Perm::EXECUTE, Flag::empty()),
            AclEntry::allow_mask(Perm::READ, Flag::empty()),
        ]);
        expected.extend(expected.clone().into_iter().map(|mut entry| {
            entry.flags |= Flag::DEFAULT;
            entry
        }));
        exacl::setfacl(&[&path], &expected, None).unwrap();
        let file = File::open(&path).unwrap();
        // The original pathname now addresses a different inode with no ACL.
        std::fs::rename(&path, dir.path().join("held")).unwrap();
        std::fs::create_dir(&path).unwrap();
        let mut actual = read_numeric_entries(&file).unwrap();
        actual.sort();
        expected.sort();
        assert_eq!(actual, expected);
        let wire = exacl::getfacl(fd_alias_path(&file), None).unwrap();
        assert!(acl_matches_fd(&file, &wire).unwrap());
        assert!(acl_matches_fd(&file, &expected).unwrap());
        // Native IDs remain distinct regardless of their reverse NSS names.
        let mut different = expected;
        different
            .iter_mut()
            .find(|entry| {
                entry.kind == AclEntryKind::User && !entry.name.is_empty() && entry.flags.is_empty()
            })
            .unwrap()
            .name = "1".into();
        assert!(!acl_matches_fd(&file, &different).unwrap());
    }
}
