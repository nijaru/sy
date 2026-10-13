//! FD-confined macOS ACLs. The kernel UUID is the principal, not a name or ID.
//! exacl supplies only the ordered text codec and permission/flag vocabulary.
use exacl::{AclEntry, AclEntryKind, Flag, Perm};
use std::io;
use std::os::fd::RawFd;
use std::os::raw::{c_int, c_uint, c_void};
use std::ptr;

type AclT = *mut c_void;
type AclEntryT = *mut c_void;
type AclPermsetT = *mut c_void;
type AclFlagsetT = *mut c_void;
type AclTagT = c_uint;
const ACL_EXTENDED_ALLOW: AclTagT = 1;
const ACL_EXTENDED_DENY: AclTagT = 2;
const ACL_FIRST_ENTRY: c_int = 0;
const ACL_NEXT_ENTRY: c_int = -1;
const ACL_MAX_ENTRIES: usize = 128;

extern "C" {
    fn acl_get_fd(fd: c_int) -> AclT;
    fn acl_set_fd(fd: c_int, acl: AclT) -> c_int;
    fn acl_free(obj: *mut c_void) -> c_int;
    fn acl_init(count: c_int) -> AclT;
    fn acl_create_entry(acl: *mut AclT, entry: *mut AclEntryT) -> c_int;
    fn acl_get_entry(acl: AclT, id: c_int, entry: *mut AclEntryT) -> c_int;
    fn acl_get_tag_type(entry: AclEntryT, tag: *mut AclTagT) -> c_int;
    fn acl_set_tag_type(entry: AclEntryT, tag: AclTagT) -> c_int;
    fn acl_get_qualifier(entry: AclEntryT) -> *mut c_void;
    fn acl_set_qualifier(entry: AclEntryT, qualifier: *const c_void) -> c_int;
    fn acl_get_permset(entry: AclEntryT, perms: *mut AclPermsetT) -> c_int;
    fn acl_get_perm_np(perms: AclPermsetT, perm: c_uint) -> c_int;
    fn acl_add_perm(perms: AclPermsetT, perm: c_uint) -> c_int;
    fn acl_clear_perms(perms: AclPermsetT) -> c_int;
    fn acl_get_flagset_np(entry: *mut c_void, flags: *mut AclFlagsetT) -> c_int;
    fn acl_get_flag_np(flags: AclFlagsetT, flag: c_uint) -> c_int;
    fn acl_clear_flags_np(flags: AclFlagsetT) -> c_int;
    fn acl_add_flag_np(flags: AclFlagsetT, flag: c_uint) -> c_int;
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
}
impl Drop for OwnedAcl {
    fn drop(&mut self) {
        // SAFETY: this is an owned allocation; all borrowed handles are finished.
        unsafe { acl_free(self.0) };
    }
}

pub(super) fn read_fd_entries(fd: RawFd) -> io::Result<Vec<AclEntry>> {
    Ok(read_fd_native_entries(fd)?
        .into_iter()
        .map(NativeEntry::into_entry)
        .collect())
}

pub(super) fn read_fd_native_entries(fd: RawFd) -> io::Result<Vec<NativeEntry>> {
    // SAFETY: caller keeps fd open for the entire read.
    let raw = unsafe { acl_get_fd(fd) };
    if raw.is_null() {
        let error = io::Error::last_os_error();
        // Native macOS reports ENOENT for an existing inode without an ACL.
        if error.raw_os_error() == Some(libc::ENOENT) {
            return Ok(Vec::new());
        }
        return Err(error);
    }
    let acl = OwnedAcl::checked(raw)?;
    read_native_entries(acl.0)
}

pub(super) fn write_fd_entries(fd: RawFd, entries: &[AclEntry]) -> io::Result<()> {
    let acl = build_acl(entries)?;
    // SAFETY: fd is held by caller; acl owns a fully populated native object.
    if unsafe { acl_set_fd(fd, acl.0) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub(super) fn desired_native_entries(entries: &[AclEntry]) -> io::Result<Vec<NativeEntry>> {
    let acl = build_acl(entries)?;
    read_native_entries(acl.0)
}

pub(super) fn validate_raw_entries(entries: &[AclEntry]) -> io::Result<()> {
    if entries.len() > ACL_MAX_ENTRIES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Too many ACL entries",
        ));
    }
    for entry in entries {
        if !matches!(entry.kind, AclEntryKind::User | AclEntryKind::Group) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "unsupported ACL kind",
            ));
        }
        parse_guid(&entry.name)?;
    }
    Ok(())
}

fn build_acl(entries: &[AclEntry]) -> io::Result<OwnedAcl> {
    validate_raw_entries(entries)?;
    let count = c_int::try_from(entries.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "Too many ACL entries"))?;
    // SAFETY: count is bounded and nonnegative; checked owns a non-null result.
    let mut acl = OwnedAcl::checked(unsafe { acl_init(count) })?;
    for entry in entries {
        let mut raw = ptr::null_mut();
        // SAFETY: acl owns the object including any reallocations; raw is an out parameter.
        if unsafe { acl_create_entry(&mut acl.0, &mut raw) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let tag = if entry.allow {
            ACL_EXTENDED_ALLOW
        } else {
            ACL_EXTENDED_DENY
        };
        // SAFETY: raw is a live new entry and tag is valid.
        if unsafe { acl_set_tag_type(raw, tag) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let guid = parse_guid(&entry.name)?;
        // SAFETY: guid is exactly 16 readable bytes, copied by the native call.
        // Do not round-trip through membership APIs, even for a known UUID.
        if unsafe { acl_set_qualifier(raw, guid.as_ptr().cast()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        write_perms(raw, entry.perms)?;
        write_flags(raw, entry.flags)?;
    }
    Ok(acl)
}

#[derive(Debug, PartialEq, Eq)]
pub(super) struct NativeEntry {
    tag: AclTagT,
    guid: [u8; 16],
    perms: Perm,
    flags: Flag,
}
impl NativeEntry {
    fn into_entry(self) -> AclEntry {
        AclEntry {
            // macOS tags do not distinguish users from groups. The raw UUID
            // is authoritative for both; User is the canonical codec carrier.
            kind: AclEntryKind::User,
            name: format_guid(&self.guid),
            perms: self.perms,
            flags: self.flags,
            allow: self.tag == ACL_EXTENDED_ALLOW,
        }
    }
}

fn read_native_entries(acl: AclT) -> io::Result<Vec<NativeEntry>> {
    let mut entries = Vec::new();
    let mut id = ACL_FIRST_ENTRY;
    loop {
        let mut raw = ptr::null_mut();
        // SAFETY: acl is live; raw receives a borrowed ACE. macOS uses 0 for
        // success and -1 for exhaustion (unlike Linux's 1/0 convention).
        if unsafe { acl_get_entry(acl, id, &mut raw) } != 0 {
            break;
        }
        let mut tag = 0;
        // SAFETY: raw is a live ACE and tag is writable storage.
        if unsafe { acl_get_tag_type(raw, &mut tag) } != 0 {
            return Err(io::Error::last_os_error());
        }
        if !matches!(tag, ACL_EXTENDED_ALLOW | ACL_EXTENDED_DENY) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "unknown ACL tag",
            ));
        }
        // SAFETY: raw has a qualifier-bearing tag; returned allocation is owned.
        let qualifier = unsafe { acl_get_qualifier(raw).cast::<[u8; 16]>() };
        if qualifier.is_null() {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: qualifier points to 16 readable bytes, copied before free.
        let guid = unsafe { *qualifier };
        // SAFETY: qualifier is the allocation returned by acl_get_qualifier.
        unsafe { acl_free(qualifier.cast()) };
        entries.push(NativeEntry {
            tag,
            guid,
            perms: read_perms(raw)?,
            flags: read_flags(raw)?,
        });
        id = ACL_NEXT_ENTRY;
    }
    Ok(entries)
}

fn read_perms(raw: AclEntryT) -> io::Result<Perm> {
    let mut perms = ptr::null_mut();
    // SAFETY: raw is live; perms is writable borrowed-handle storage.
    if unsafe { acl_get_permset(raw, &mut perms) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let mut result = Perm::empty();
    for bit in single_bits(Perm::all().bits()) {
        // SAFETY: perms is live and bit is a supported permission.
        match unsafe { acl_get_perm_np(perms, bit) } {
            0 => (),
            1 => result |= Perm::from_bits_retain(bit),
            _ => return Err(io::Error::last_os_error()),
        }
    }
    Ok(result)
}
fn read_flags(raw: AclEntryT) -> io::Result<Flag> {
    let mut flags = ptr::null_mut();
    // SAFETY: raw is live; flags is writable borrowed-handle storage.
    if unsafe { acl_get_flagset_np(raw, &mut flags) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let mut result = Flag::empty();
    for bit in single_bits(Flag::all().bits()) {
        // SAFETY: flags is live and bit is a supported flag.
        match unsafe { acl_get_flag_np(flags, bit) } {
            0 => (),
            1 => result |= Flag::from_bits_retain(bit),
            _ => return Err(io::Error::last_os_error()),
        }
    }
    Ok(result)
}
fn write_perms(raw: AclEntryT, value: Perm) -> io::Result<()> {
    let mut perms = ptr::null_mut();
    // SAFETY: raw is live; perms receives its borrowed permission handle.
    if unsafe { acl_get_permset(raw, &mut perms) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: perms belongs to the live ACE.
    if unsafe { acl_clear_perms(perms) } != 0 {
        return Err(io::Error::last_os_error());
    }
    for bit in single_bits(value.bits()) {
        // SAFETY: perms is live; native API validates each permission bit.
        if unsafe { acl_add_perm(perms, bit) } != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}
fn write_flags(raw: AclEntryT, value: Flag) -> io::Result<()> {
    let mut flags = ptr::null_mut();
    // SAFETY: raw is live; flags receives its borrowed flag handle.
    if unsafe { acl_get_flagset_np(raw, &mut flags) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: flags belongs to the live ACE.
    if unsafe { acl_clear_flags_np(flags) } != 0 {
        return Err(io::Error::last_os_error());
    }
    for bit in single_bits(value.bits()) {
        // SAFETY: flags is live; native API validates each flag bit.
        if unsafe { acl_add_flag_np(flags, bit) } != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}
fn single_bits(mut bits: u32) -> impl Iterator<Item = u32> {
    std::iter::from_fn(move || {
        if bits == 0 {
            return None;
        }
        let bit = 1 << bits.trailing_zeros();
        bits &= !bit;
        Some(bit)
    })
}
fn parse_guid(text: &str) -> io::Result<[u8; 16]> {
    let invalid = || {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "ACL principal must be a raw UUID",
        )
    };
    if text.len() != 36 {
        return Err(invalid());
    }
    let mut guid = [0; 16];
    let mut digits = text.bytes().enumerate().filter_map(|(i, byte)| {
        if matches!(i, 8 | 13 | 18 | 23) {
            if byte == b'-' {
                None
            } else {
                Some(None)
            }
        } else {
            Some((byte as char).to_digit(16).map(|n| n as u8))
        }
    });
    for byte in &mut guid {
        let high = digits.next().flatten().ok_or_else(invalid)?;
        let low = digits.next().flatten().ok_or_else(invalid)?;
        *byte = high * 16 + low;
    }
    if digits.next().is_some() {
        return Err(invalid());
    }
    Ok(guid)
}
fn format_guid(guid: &[u8; 16]) -> String {
    use std::fmt::Write;
    let mut text = String::with_capacity(36);
    for (i, byte) in guid.iter().enumerate() {
        if matches!(i, 4 | 6 | 8 | 10) {
            text.push('-');
        }
        // Writing to String cannot fail.
        write!(text, "{byte:02x}").expect("String formatting is infallible");
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::AsRawFd;

    #[tokio::test]
    async fn capture_wire_apply_retains_native_uuid_and_order_without_membership_remapping() {
        use crate::engine::domain::{EntryKind, RelativePath};
        use crate::protocol::WireAcl;
        use crate::rooted_fs::RootedFs;
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source");
        std::fs::write(&source, b"source").unwrap();
        std::fs::write(dir.path().join("dest"), b"dest").unwrap();
        // This compatibility UUID is a native alias for the process UID, but
        // forward membership lookup may return a different directory UUID.
        // SAFETY: getuid takes no pointers and only reads process credentials.
        let uid = unsafe { libc::getuid() };
        let mut compatibility = parse_guid("ffffeeee-dddd-cccc-bbbb-aaaa00000000").unwrap();
        compatibility[12..].copy_from_slice(&uid.to_be_bytes());
        let entries = [
            AclEntry::allow_user(&format_guid(&compatibility), Perm::READ, Flag::FILE_INHERIT),
            AclEntry::deny_user(
                "6b6b6b6b-6b6b-6b6b-6b6b-6b6b6b6b6b6b",
                Perm::WRITE,
                Flag::empty(),
            ),
            AclEntry::deny_user(&format_guid(&compatibility), Perm::WRITE, Flag::empty()),
        ];
        // Independently plant the first qualifier with the native API rather
        // than relying on the capture/apply principal conversion under test.
        let acl = build_acl(&entries).unwrap();
        let mut raw = ptr::null_mut();
        // SAFETY: acl owns a live object; raw receives its first ACE.
        assert_eq!(
            unsafe { acl_get_entry(acl.0, ACL_FIRST_ENTRY, &mut raw) },
            0
        );
        // SAFETY: raw is live and compatibility provides 16 readable bytes.
        assert_eq!(
            unsafe { acl_set_qualifier(raw, compatibility.as_ptr().cast()) },
            0
        );
        let file = std::fs::File::open(&source).unwrap();
        // SAFETY: file and acl are held for the native setter.
        assert_eq!(unsafe { acl_set_fd(file.as_raw_fd(), acl.0) }, 0);
        let root = RootedFs::open(dir.path().into()).await.unwrap();
        let source_path = RelativePath::new("source").unwrap();
        let text = root
            .read_open_file_acl_blocking(&file, &source_path)
            .unwrap()
            .unwrap();
        let exported = exacl::from_str(&text).unwrap();
        assert_eq!(exported, entries);
        assert_eq!(exported[0].name, format_guid(&compatibility));
        let wire = WireAcl::decode(&WireAcl::new(text).unwrap().encode()).unwrap();
        root.write_acl_blocking(
            &RelativePath::new("dest").unwrap(),
            EntryKind::File,
            wire.text(),
        )
        .unwrap();
        let dest = std::fs::File::open(dir.path().join("dest")).unwrap();
        let native = read_fd_native_entries(dest.as_raw_fd()).unwrap();
        assert_eq!(native[0].guid, compatibility);
        assert_eq!(native, read_fd_native_entries(file.as_raw_fd()).unwrap());
        let mut reversed = exported;
        reversed.reverse();
        assert_ne!(native, desired_native_entries(&reversed).unwrap());
        for name in [
            "root",
            "0",
            "not-a-uuid",
            "ffffeeee-dddd-cccc-bbbb-aaaa0000000",
        ] {
            assert!(
                validate_raw_entries(&[AclEntry::allow_user(name, Perm::READ, Flag::empty())])
                    .is_err()
            );
        }
        root.write_acl_blocking(&RelativePath::new("dest").unwrap(), EntryKind::File, "")
            .unwrap();
        assert!(read_fd_entries(dest.as_raw_fd()).unwrap().is_empty());
    }
}
