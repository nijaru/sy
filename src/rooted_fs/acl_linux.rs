//! Held-inode ACL capture and application use kernel UID/GIDs exclusively.
//! exacl owns only the ordered text codec, never principal resolution.
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
    fn acl_init(count: c_int) -> AclT;
    fn acl_create_entry(acl: *mut AclT, entry: *mut EntryT) -> c_int;
    fn acl_set_tag_type(entry: EntryT, tag: c_int) -> c_int;
    fn acl_set_qualifier(entry: EntryT, qualifier: *const c_void) -> c_int;
    fn acl_clear_perms(permset: PermsetT) -> c_int;
    fn acl_add_perm(permset: PermsetT, perm: c_uint) -> c_int;
    fn acl_valid(acl: AclT) -> c_int;
    fn acl_set_fd(fd: c_int, acl: AclT) -> c_int;
    fn acl_set_file(path: *const c_char, acl_type: c_uint, acl: AclT) -> c_int;
    fn acl_delete_def_file(path: *const c_char) -> c_int;
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

fn invalid_entry() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        "ACL requires numeric UID/GID, POSIX tags, permissions and flags",
    )
}

pub(super) fn validate_numeric_entries(entries: &[AclEntry]) -> io::Result<()> {
    for entry in entries {
        if !entry.allow
            || !(entry.flags - Flag::DEFAULT).is_empty()
            || !(entry.perms - (Perm::READ | Perm::WRITE | Perm::EXECUTE)).is_empty()
        {
            return Err(invalid_entry());
        }
        match entry.kind {
            AclEntryKind::User | AclEntryKind::Group if !entry.name.is_empty() => {
                let id = entry.name.parse::<u32>().map_err(|_| invalid_entry())?;
                if id == u32::MAX || id.to_string() != entry.name {
                    return Err(invalid_entry());
                }
            }
            AclEntryKind::User | AclEntryKind::Group | AclEntryKind::Mask | AclEntryKind::Other
                if entry.name.is_empty() => {}
            _ => return Err(invalid_entry()),
        }
    }
    Ok(())
}

fn build_acl(entries: &[AclEntry], default: bool) -> io::Result<OwnedAcl> {
    let count = c_int::try_from(entries.len()).map_err(|_| invalid_entry())?;
    // SAFETY: count is nonnegative; checked establishes ownership on success.
    let mut acl = OwnedAcl::checked(unsafe { acl_init(count) })?;
    for entry in entries
        .iter()
        .filter(|entry| entry.flags.contains(Flag::DEFAULT) == default)
    {
        let tag = match (entry.kind, entry.name.is_empty()) {
            (AclEntryKind::User, true) => ACL_USER_OBJ,
            (AclEntryKind::User, false) => ACL_USER,
            (AclEntryKind::Group, true) => ACL_GROUP_OBJ,
            (AclEntryKind::Group, false) => ACL_GROUP,
            (AclEntryKind::Mask, _) => ACL_MASK,
            (AclEntryKind::Other, _) => ACL_OTHER,
            _ => return Err(invalid_entry()),
        };
        let mut raw = ptr::null_mut();
        // SAFETY: acl owns the live allocation, including any reallocation;
        // raw receives a borrowed entry handle, used before the next append.
        if unsafe { acl_create_entry(&mut acl.0, &mut raw) } != 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: raw is a live new entry; tag is a supported POSIX tag.
        if unsafe { acl_set_tag_type(raw, tag) } != 0 {
            return Err(io::Error::last_os_error());
        }
        if matches!(tag, ACL_USER | ACL_GROUP) {
            let id = entry.name.parse::<u32>().map_err(|_| invalid_entry())?;
            // SAFETY: raw has a qualifier-bearing tag; id is a live u32
            // (Linux uid_t/gid_t), copied by libacl without NSS lookup.
            if unsafe { acl_set_qualifier(raw, (&id as *const u32).cast()) } != 0 {
                return Err(io::Error::last_os_error());
            }
        }
        let mut perms = ptr::null_mut();
        // SAFETY: raw is live and perms is writable out-parameter storage.
        if unsafe { acl_get_permset(raw, &mut perms) } != 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: perms is borrowed from raw in the owned ACL.
        if unsafe { acl_clear_perms(perms) } != 0 {
            return Err(io::Error::last_os_error());
        }
        for bit in [Perm::READ, Perm::WRITE, Perm::EXECUTE] {
            // SAFETY: perms is live; bit is a valid POSIX permission.
            if entry.perms.contains(bit) && unsafe { acl_add_perm(perms, bit.bits()) } != 0 {
                return Err(io::Error::last_os_error());
            }
        }
    }
    Ok(acl)
}

pub(super) fn write_numeric_entries(file: &File, entries: &[AclEntry]) -> io::Result<()> {
    validate_numeric_entries(entries)?;
    let access = build_acl(entries, false)?;
    let default = build_acl(entries, true)?;
    let has_default = entries
        .iter()
        .any(|entry| entry.flags.contains(Flag::DEFAULT));
    let directory = file.metadata()?.is_dir();
    // Validate both ACLs before the first setter; never calculate a new mask.
    // SAFETY: both objects are live owned allocations.
    if unsafe { acl_valid(access.0) } != 0
        || (has_default && unsafe { acl_valid(default.0) } != 0)
        || (has_default && !directory)
    {
        return Err(invalid_entry());
    }
    // SAFETY: file holds the destination inode; access is a valid ACL.
    if unsafe { acl_set_fd(file.as_raw_fd(), access.0) } != 0 {
        return Err(io::Error::last_os_error());
    }
    if directory {
        let alias = CString::new(fd_alias_path(file).as_os_str().as_bytes())?;
        // SAFETY: alias addresses the held inode; default is a valid ACL
        // when present. Both calls copy state and retain no input pointers.
        let result = unsafe {
            if has_default {
                acl_set_file(alias.as_ptr(), ACL_TYPE_DEFAULT, default.0)
            } else {
                acl_delete_def_file(alias.as_ptr())
            }
        };
        if result != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rooted_fs::acl_state::acl_matches_fd;

    #[tokio::test]
    async fn capture_wire_apply_keeps_numeric_principals_defaults_and_held_fd_authority() {
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
        let actual = read_numeric_entries(&file).unwrap();
        // UID/GID 0 normally reverse-resolve to account names; assert the
        // exported identity independently of our own encode/decode round trip.
        for kind in [AclEntryKind::User, AclEntryKind::Group] {
            for flags in [Flag::empty(), Flag::DEFAULT] {
                assert!(actual
                    .iter()
                    .any(|entry| entry.kind == kind && entry.name == "0" && entry.flags == flags));
            }
        }
        use crate::engine::domain::{EntryKind, RelativePath};
        use crate::protocol::WireAcl;
        use crate::rooted_fs::RootedFs;
        let root = RootedFs::open(dir.path().into()).await.unwrap();
        let held = RelativePath::new("held").unwrap();
        let source_identity = root.path_identity_blocking(&held).unwrap().unwrap().1;
        let text = root
            .read_observed_acl_blocking(&held, EntryKind::Directory, source_identity)
            .unwrap()
            .unwrap();
        let wire = WireAcl::decode(&WireAcl::new(text).unwrap().encode()).unwrap();
        assert_eq!(exacl::from_str(wire.text()).unwrap(), actual);
        let destination = RelativePath::new("directory").unwrap();
        let identity = root
            .path_identity_blocking(&destination)
            .unwrap()
            .unwrap()
            .1;
        root.finalize_directory_blocking(
            &destination,
            identity,
            None,
            None,
            &crate::rooted_fs::DirectoryPreservation {
                acl: Some(wire.text().to_owned()),
                ..Default::default()
            },
        )
        .unwrap();
        let received = File::open(&path).unwrap();
        assert_eq!(read_numeric_entries(&received).unwrap(), actual);
        assert!(acl_matches_fd(&file, &actual).unwrap());
        // Receiver account tables cannot reinterpret numeric strings because
        // application goes directly to acl_set_qualifier, not exacl/NSS.
        let mut named = actual.clone();
        named
            .iter_mut()
            .find(|entry| entry.name == "0")
            .unwrap()
            .name = "root".into();
        assert!(write_numeric_entries(&received, &named).is_err());
        let mut reversed = actual.clone();
        reversed.reverse();
        assert!(!acl_matches_fd(&file, &reversed).unwrap());
        // Native IDs remain distinct regardless of their reverse NSS names.
        let mut different = actual;
        different
            .iter_mut()
            .find(|entry| {
                entry.kind == AclEntryKind::User && !entry.name.is_empty() && entry.flags.is_empty()
            })
            .unwrap()
            .name = "1".into();
        assert!(!acl_matches_fd(&file, &different).unwrap());
        let identity = root
            .path_identity_blocking(&destination)
            .unwrap()
            .unwrap()
            .1;
        root.finalize_directory_blocking(
            &destination,
            identity,
            None,
            None,
            &crate::rooted_fs::DirectoryPreservation {
                acl: Some(String::new()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(
            read_numeric_entries(&received).unwrap(),
            exacl::from_mode(0o740)
        );
    }
}
