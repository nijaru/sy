//! macOS fd-confined ACL read/write for [`crate::rooted_fs::RootedFs`].
//!
//! The public exacl API is path-based (`getfacl`/`setfacl`), and on macOS a
//! descriptor-aliased path (`/dev/fd/N`) does **not** resolve to the open
//! inode for `acl_get_file` (verified: it returns an empty list instead of
//! the target's entries). Path-based calls would therefore reintroduce the
//! TOCTOU/symlink escape that root confinement forbids at the protocol
//! boundary, so this module drives the native `acl_get_fd`/`acl_set_fd`
//! syscalls directly through the already-open held leaf descriptor.
//!
//! The entry mapping is a faithful port of exacl 0.12's own macOS backend
//! (`util_macos.rs`, `aclentry.rs`, `qualifier.rs`, `unix.rs`): the wire text
//! must stay byte-identical to what `LocalEndpoint` produces with exacl, so
//! push, pull, and local agree on one representation. Any intentional
//! divergence from exacl's mapping is a bug; the differential tests below
//! (fd implementation vs exacl path API on the same file) prove agreement.
//!
//! Linux does not need this module: there `/proc/self/fd/N` resolves to the
//! open inode for `acl_get_file`/`acl_set_file` (verified on Fedora,
//! including default entries and writes through read-only descriptors), so
//! the Linux implementation reuses exacl's public API on the fd-aliased
//! path with zero conversion code.

use exacl::{AclEntry, AclEntryKind, Flag, Perm};
use std::ffi::{CStr, CString};
use std::io;
use std::mem::MaybeUninit;
use std::os::fd::RawFd;
use std::os::raw::{c_char, c_int, c_uint, c_void};
use std::ptr;

// MARK: native types (opaque handles mirror exacl's bindgen output)

type AclT = *mut c_void;
type AclEntryT = *mut c_void;
type AclPermsetT = *mut c_void;
type AclFlagsetT = *mut c_void;
type AclTagT = c_uint;
type AclPermT = c_uint;
type AclFlagT = c_uint;
type UidT = u32;
type GidT = u32;

const ACL_EXTENDED_ALLOW: AclTagT = 1;
const ACL_EXTENDED_DENY: AclTagT = 2;
const ACL_FIRST_ENTRY: c_int = 0;
const ACL_NEXT_ENTRY: c_int = -1;
const ACL_MAX_ENTRIES: usize = 128;
const ID_TYPE_UID: c_int = 0;
const ID_TYPE_GID: c_int = 1;

// Buffer discipline copied from exacl's `unix.rs`: start at the
// `SC_GETPW_R_SIZE_MAX` default and quadruple up to 1 MiB.
const INITIAL_BUFSIZE: usize = 4096;
const MAX_BUFSIZE: usize = 1_048_576;

#[repr(C)]
struct Passwd {
    pw_name: *mut c_char,
    pw_passwd: *mut c_char,
    pw_uid: UidT,
    pw_gid: GidT,
    pw_change: i64,
    pw_class: *mut c_char,
    pw_gecos: *mut c_char,
    pw_dir: *mut c_char,
    pw_shell: *mut c_char,
    pw_expire: i64,
}

#[repr(C)]
struct Group {
    gr_name: *mut c_char,
    gr_passwd: *mut c_char,
    gr_gid: GidT,
    gr_mem: *mut *mut c_char,
}

extern "C" {
    fn acl_get_fd(fd: c_int) -> AclT;
    fn acl_set_fd(fd: c_int, acl: AclT) -> c_int;
    fn acl_free(obj: *mut c_void) -> c_int;
    fn acl_init(count: c_int) -> AclT;
    fn acl_create_entry(acl_p: *mut AclT, entry_p: *mut AclEntryT) -> c_int;
    fn acl_get_entry(acl: AclT, entry_id: c_int, entry_p: *mut AclEntryT) -> c_int;
    fn acl_get_tag_type(entry: AclEntryT, tag_p: *mut AclTagT) -> c_int;
    fn acl_set_tag_type(entry: AclEntryT, tag: AclTagT) -> c_int;
    fn acl_get_qualifier(entry: AclEntryT) -> *mut c_void;
    fn acl_set_qualifier(entry: AclEntryT, qualifier: *const c_void) -> c_int;
    fn acl_get_permset(entry: AclEntryT, permset_p: *mut AclPermsetT) -> c_int;
    fn acl_get_perm_np(permset: AclPermsetT, perm: AclPermT) -> c_int;
    fn acl_add_perm(permset: AclPermsetT, perm: AclPermT) -> c_int;
    fn acl_clear_perms(permset: AclPermsetT) -> c_int;
    fn acl_get_flagset_np(obj: *mut c_void, flagset_p: *mut AclFlagsetT) -> c_int;
    fn acl_get_flag_np(flagset: AclFlagsetT, flag: AclFlagT) -> c_int;
    fn acl_clear_flags_np(flagset: AclFlagsetT) -> c_int;
    fn acl_add_flag_np(flagset: AclFlagsetT, flag: AclFlagT) -> c_int;
    fn mbr_uid_to_uuid(uid: UidT, uu: *mut u8) -> c_int;
    fn mbr_gid_to_uuid(gid: GidT, uu: *mut u8) -> c_int;
    fn mbr_uuid_to_id(uu: *const u8, id: *mut c_uint, id_type: *mut c_int) -> c_int;
    fn getpwnam_r(
        name: *const c_char,
        pwd: *mut Passwd,
        buf: *mut c_char,
        buflen: usize,
        result: *mut *mut Passwd,
    ) -> c_int;
    fn getpwuid_r(
        uid: UidT,
        pwd: *mut Passwd,
        buf: *mut c_char,
        buflen: usize,
        result: *mut *mut Passwd,
    ) -> c_int;
    fn getgrnam_r(
        name: *const c_char,
        grp: *mut Group,
        buf: *mut c_char,
        buflen: usize,
        result: *mut *mut Group,
    ) -> c_int;
    fn getgrgid_r(
        gid: GidT,
        grp: *mut Group,
        buf: *mut c_char,
        buflen: usize,
        result: *mut *mut Group,
    ) -> c_int;
}

// MARK: entry points

/// Read the ACL of the already-open file or directory `fd` as exacl entries.
///
/// The descriptor must have been opened without following symlinks (the
/// caller holds a no-follow leaf through the pinned root); every syscall
/// below operates on the open file description, never on a path.
pub(super) fn read_fd_entries(fd: RawFd) -> io::Result<Vec<AclEntry>> {
    // SAFETY: `acl_get_fd` reads the ACL of the open file description `fd`,
    // which the caller guarantees is a live held descriptor. A null return
    // means no ACL (or an error distinguished below via errno).
    let acl = unsafe { acl_get_fd(fd) };
    if acl.is_null() {
        let err = io::Error::last_os_error();
        // Like exacl's path backend, a missing ACL on an existing file reads
        // as empty rather than failing. `acl_get_fd` reports ENOENT when the
        // file exists but carries no extended ACL.
        if err.raw_os_error() == Some(libc::ENOENT) {
            return Ok(Vec::new());
        }
        return Err(err);
    }
    // SAFETY: `acl` is a live object from `acl_get_fd`; freed exactly once
    // on every return path below.
    let result = read_entries(acl);
    unsafe { acl_free(acl.cast()) };
    result
}

/// Replace the ACL of the already-open file or directory `fd` with `entries`.
///
/// An empty slice clears the list, matching exacl's `setfacl` with no
/// entries. Entries use the same `(kind, name, perms, flags, allow)` mapping
/// as exacl's macOS backend, so text produced from these entries is identical
/// to the path-based implementation's.
pub(super) fn write_fd_entries(fd: RawFd, entries: &[AclEntry]) -> io::Result<()> {
    let count = i32::try_from(entries.len())
        .ok()
        .filter(|_| entries.len() <= ACL_MAX_ENTRIES)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "Too many ACL entries"))?;
    // SAFETY: `count` is within `ACL_MAX_ENTRIES`; a null return is checked.
    let mut acl = unsafe { acl_init(count) };
    if acl.is_null() {
        return Err(io::Error::last_os_error());
    }
    let result = (|| -> io::Result<()> {
        for (i, entry) in entries.iter().enumerate() {
            add_entry(&mut acl, entry)
                .map_err(|err| io::Error::new(err.kind(), format!("entry {i}: {err}")))?;
        }
        // SAFETY: `acl` is a live object built above; `fd` is the caller's
        // live held descriptor.
        let ret = unsafe { acl_set_fd(fd, acl) };
        if ret != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    })();
    // SAFETY: `acl` is live here on every path (freed exactly once); even
    // `acl_create_entry` reallocations keep it valid via the `&mut` handoff.
    unsafe { acl_free(acl.cast()) };
    result
}

// MARK: read conversion (native acl_t -> exacl entries)

fn read_entries(acl: AclT) -> io::Result<Vec<AclEntry>> {
    let mut out = Vec::new();
    let mut raw: AclEntryT = ptr::null_mut();
    // macOS reports success with 0 (unlike Linux/FreeBSD, which use 1).
    let mut id = ACL_FIRST_ENTRY;
    loop {
        // SAFETY: `acl` is live; `raw` receives the next entry handle or is
        // left null when iteration ends.
        let ret = unsafe { acl_get_entry(acl, id, &mut raw) };
        if ret != 0 {
            break;
        }
        out.push(read_entry(raw)?);
        id = ACL_NEXT_ENTRY;
    }
    Ok(out)
}

fn read_entry(raw: AclEntryT) -> io::Result<AclEntry> {
    let mut tag: AclTagT = 0;
    // SAFETY: `raw` is a live entry handle from iteration.
    if unsafe { acl_get_tag_type(raw, &mut tag) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let (allow, qualifier) = match tag {
        ACL_EXTENDED_ALLOW => (true, read_qualifier(raw)?),
        ACL_EXTENDED_DENY => (false, read_qualifier(raw)?),
        other => (false, Qualifier::Unknown(format!("@tag {other}"))),
    };
    let perms = read_perms(raw)?;
    let flags = read_flags(raw)?;
    let (kind, name) = match qualifier {
        Qualifier::Unknown(text) => (AclEntryKind::Unknown, text),
        Qualifier::User(_) | Qualifier::Guid(_) => (AclEntryKind::User, qualifier.name()?),
        Qualifier::Group(_) => (AclEntryKind::Group, qualifier.name()?),
    };
    Ok(AclEntry {
        kind,
        name,
        perms,
        flags,
        allow,
    })
}

fn read_qualifier(raw: AclEntryT) -> io::Result<Qualifier> {
    // SAFETY: `raw` is live; the returned qualifier buffer is freed below.
    let uuid_ptr = unsafe { acl_get_qualifier(raw).cast::<[u8; 16]>() };
    if uuid_ptr.is_null() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "acl_get_qualifier returned null",
        ));
    }
    // SAFETY: non-null qualifier from a live entry is a readable 16-byte
    // UUID; copied out before freeing.
    let guid = unsafe { *uuid_ptr };
    unsafe { acl_free(uuid_ptr.cast()) };
    Qualifier::from_guid(guid)
}

fn read_perms(raw: AclEntryT) -> io::Result<Perm> {
    let mut permset: AclPermsetT = ptr::null_mut();
    // SAFETY: `raw` is live; `permset` is populated on success.
    if unsafe { acl_get_permset(raw, &mut permset) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let mut perms = Perm::empty();
    // Query every known bit through the syscall (rather than copying raw
    // bits) so unknown kernel bits are ignored exactly like exacl does.
    for bit in single_bits(Perm::all().bits()) {
        // SAFETY: `permset` is live; `acl_get_perm_np` returns 0/1.
        let present = unsafe { acl_get_perm_np(permset, bit) };
        if present == 1 {
            perms |= Perm::from_bits_retain(bit);
        }
    }
    Ok(perms)
}

fn read_flags(raw: AclEntryT) -> io::Result<Flag> {
    let mut flagset: AclFlagsetT = ptr::null_mut();
    // SAFETY: `raw` is live; `flagset` is populated on success.
    if unsafe { acl_get_flagset_np(raw.cast(), &mut flagset) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let mut flags = Flag::empty();
    for bit in single_bits(Flag::all().bits()) {
        // SAFETY: `flagset` is live; `acl_get_flag_np` returns 0/1.
        let present = unsafe { acl_get_flag_np(flagset, bit) };
        if present == 1 {
            flags |= Flag::from_bits_retain(bit);
        }
    }
    Ok(flags)
}

// MARK: write conversion (exacl entries -> native acl_t)

fn add_entry(acl: &mut AclT, entry: &AclEntry) -> io::Result<()> {
    let mut raw: AclEntryT = ptr::null_mut();
    // SAFETY: `acl` points at a live object; the new handle is written to
    // `raw` on success (the object may move, hence `&mut`).
    if unsafe { acl_create_entry(acl, &mut raw) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let qualifier = entry_qualifier(entry)?;
    let tag = match &qualifier {
        Qualifier::Unknown(_) => {
            debug_assert!(!entry.allow);
            ACL_EXTENDED_DENY
        }
        _ if entry.allow => ACL_EXTENDED_ALLOW,
        _ => ACL_EXTENDED_DENY,
    };
    // SAFETY: `raw` is a live fresh entry.
    if unsafe { acl_set_tag_type(raw, tag) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let guid = qualifier.guid()?;
    // SAFETY: `raw` is live with a valid tag; `guid` outlives the call.
    if unsafe { acl_set_qualifier(raw, guid.as_ptr().cast()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    write_perms(raw, entry.perms)?;
    write_flags(raw, entry.flags)?;
    Ok(())
}

fn entry_qualifier(entry: &AclEntry) -> io::Result<Qualifier> {
    match entry.kind {
        AclEntryKind::User => Qualifier::user_named(&entry.name),
        AclEntryKind::Group => Qualifier::group_named(&entry.name),
        AclEntryKind::Unknown => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "unsupported kind: \"unknown\"",
        )),
    }
}

fn write_perms(raw: AclEntryT, perms: Perm) -> io::Result<()> {
    let mut permset: AclPermsetT = ptr::null_mut();
    // SAFETY: `raw` is live; `permset` is populated on success.
    if unsafe { acl_get_permset(raw, &mut permset) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `permset` is live.
    if unsafe { acl_clear_perms(permset) } != 0 {
        return Err(io::Error::last_os_error());
    }
    for bit in single_bits(perms.bits()) {
        // SAFETY: `permset` is live.
        let ret = unsafe { acl_add_perm(permset, bit) };
        debug_assert_eq!(ret, 0);
        if ret != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

fn write_flags(raw: AclEntryT, flags: Flag) -> io::Result<()> {
    let mut flagset: AclFlagsetT = ptr::null_mut();
    // SAFETY: `raw` is live; `flagset` is populated on success.
    if unsafe { acl_get_flagset_np(raw.cast(), &mut flagset) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `flagset` is live.
    if unsafe { acl_clear_flags_np(flagset) } != 0 {
        return Err(io::Error::last_os_error());
    }
    for bit in single_bits(flags.bits()) {
        // SAFETY: `flagset` is live.
        let ret = unsafe { acl_add_flag_np(flagset, bit) };
        debug_assert_eq!(ret, 0);
        if ret != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

// MARK: principal resolution (port of exacl's qualifier.rs/unix.rs)

/// Internal qualifier mirroring exacl's private type so the name mapping
/// stays identical.
#[derive(Debug, PartialEq, Eq)]
enum Qualifier {
    User(UidT),
    Group(GidT),
    Guid([u8; 16]),
    Unknown(String),
}

impl Qualifier {
    fn from_guid(guid: [u8; 16]) -> io::Result<Self> {
        let mut id: c_uint = 0;
        let mut id_type: c_int = 0;
        // SAFETY: `mbr_uuid_to_id` only reads the 16 bytes; `id`/`id_type`
        // are live out-params.
        let ret = unsafe { mbr_uuid_to_id(guid.as_ptr(), &mut id, &mut id_type) };
        if ret == libc::ENOENT {
            return Ok(Qualifier::Guid(guid));
        }
        if ret != 0 {
            return Err(io::Error::from_raw_os_error(ret));
        }
        match id_type {
            ID_TYPE_UID => Ok(Qualifier::User(id)),
            ID_TYPE_GID => Ok(Qualifier::Group(id)),
            other => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("mbr_uuid_to_id: unknown id type {other}"),
            )),
        }
    }

    fn user_named(name: &str) -> io::Result<Self> {
        match name_to_uid(name) {
            Ok(uid) => Ok(Qualifier::User(uid)),
            // Try to parse the name as a GUID, preserving the original
            // lookup error when that fails too (matches exacl).
            Err(err) => match parse_guid(name) {
                Ok(guid) => Qualifier::from_guid(guid),
                Err(_) => Err(err),
            },
        }
    }

    fn group_named(name: &str) -> io::Result<Self> {
        match name_to_gid(name) {
            Ok(gid) => Ok(Qualifier::Group(gid)),
            Err(err) => match parse_guid(name) {
                Ok(guid) => Qualifier::from_guid(guid),
                Err(_) => Err(err),
            },
        }
    }

    fn guid(&self) -> io::Result<[u8; 16]> {
        match self {
            Qualifier::User(uid) => uid_to_guid(*uid),
            Qualifier::Group(gid) => gid_to_guid(*gid),
            Qualifier::Guid(guid) => Ok(*guid),
            Qualifier::Unknown(tag) => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("unknown tag: {tag:?}"),
            )),
        }
    }

    fn name(&self) -> io::Result<String> {
        match self {
            Qualifier::User(uid) => uid_to_name(*uid),
            Qualifier::Group(gid) => gid_to_name(*gid),
            Qualifier::Guid(guid) => Ok(format_guid(guid)),
            Qualifier::Unknown(text) => Ok(text.clone()),
        }
    }
}

fn name_to_uid(name: &str) -> io::Result<UidT> {
    let cstr = CString::new(name)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL in name"))?;
    let mut pwd = MaybeUninit::<Passwd>::uninit();
    let mut buf = Vec::<c_char>::with_capacity(INITIAL_BUFSIZE);
    let mut result: *mut Passwd = ptr::null_mut();
    loop {
        // SAFETY: `pwd`/`buf`/`result` are live for this call; `buf` has the
        // passed capacity. Retried with a larger buffer on ERANGE.
        let ret = unsafe {
            getpwnam_r(
                cstr.as_ptr(),
                pwd.as_mut_ptr(),
                buf.as_mut_ptr(),
                buf.capacity(),
                &mut result,
            )
        };
        if ret == 0 || ret != libc::ERANGE || buf.capacity() >= MAX_BUFSIZE {
            if ret != 0 {
                return Err(io::Error::from_raw_os_error(ret));
            }
            break;
        }
        buf.reserve(4 * buf.capacity());
    }
    if !result.is_null() {
        // SAFETY: `result` non-null means `pwd` was initialized.
        return Ok(unsafe { pwd.assume_init().pw_uid });
    }
    // Fall back to a decimal uid, matching exacl.
    name.parse::<UidT>().map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("unknown user name: {name:?}"),
        )
    })
}

fn name_to_gid(name: &str) -> io::Result<GidT> {
    let cstr = CString::new(name)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL in name"))?;
    let mut grp = MaybeUninit::<Group>::uninit();
    let mut buf = Vec::<c_char>::with_capacity(INITIAL_BUFSIZE);
    let mut result: *mut Group = ptr::null_mut();
    loop {
        // SAFETY: same contract as `getpwnam_r` above.
        let ret = unsafe {
            getgrnam_r(
                cstr.as_ptr(),
                grp.as_mut_ptr(),
                buf.as_mut_ptr(),
                buf.capacity(),
                &mut result,
            )
        };
        if ret == 0 || ret != libc::ERANGE || buf.capacity() >= MAX_BUFSIZE {
            if ret != 0 {
                return Err(io::Error::from_raw_os_error(ret));
            }
            break;
        }
        buf.reserve(4 * buf.capacity());
    }
    if !result.is_null() {
        // SAFETY: `result` non-null means `grp` was initialized.
        return Ok(unsafe { grp.assume_init().gr_gid });
    }
    name.parse::<GidT>().map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("unknown group name: {name:?}"),
        )
    })
}

fn uid_to_name(uid: UidT) -> io::Result<String> {
    let mut pwd = MaybeUninit::<Passwd>::uninit();
    let mut buf = Vec::<c_char>::with_capacity(INITIAL_BUFSIZE);
    let mut result: *mut Passwd = ptr::null_mut();
    loop {
        // SAFETY: same contract as above.
        let ret = unsafe {
            getpwuid_r(
                uid,
                pwd.as_mut_ptr(),
                buf.as_mut_ptr(),
                buf.capacity(),
                &mut result,
            )
        };
        if ret == 0 || ret != libc::ERANGE || buf.capacity() >= MAX_BUFSIZE {
            if ret != 0 {
                return Err(io::Error::from_raw_os_error(ret));
            }
            break;
        }
        buf.reserve(4 * buf.capacity());
    }
    if !result.is_null() {
        // SAFETY: `result` non-null means `pwd` was initialized and
        // `pw_name` points at a live NUL-terminated string.
        let name = unsafe { CStr::from_ptr(pwd.assume_init().pw_name) };
        return Ok(name.to_string_lossy().into_owned());
    }
    Ok(uid.to_string())
}

fn gid_to_name(gid: GidT) -> io::Result<String> {
    let mut grp = MaybeUninit::<Group>::uninit();
    let mut buf = Vec::<c_char>::with_capacity(INITIAL_BUFSIZE);
    let mut result: *mut Group = ptr::null_mut();
    loop {
        // SAFETY: same contract as above.
        let ret = unsafe {
            getgrgid_r(
                gid,
                grp.as_mut_ptr(),
                buf.as_mut_ptr(),
                buf.capacity(),
                &mut result,
            )
        };
        if ret == 0 || ret != libc::ERANGE || buf.capacity() >= MAX_BUFSIZE {
            if ret != 0 {
                return Err(io::Error::from_raw_os_error(ret));
            }
            break;
        }
        buf.reserve(4 * buf.capacity());
    }
    if !result.is_null() {
        // SAFETY: `result` non-null means `grp` was initialized and
        // `gr_name` points at a live NUL-terminated string.
        let name = unsafe { CStr::from_ptr(grp.assume_init().gr_name) };
        return Ok(name.to_string_lossy().into_owned());
    }
    Ok(gid.to_string())
}

fn uid_to_guid(uid: UidT) -> io::Result<[u8; 16]> {
    let mut bytes = [0_u8; 16];
    // SAFETY: `mbr_uid_to_uuid` writes exactly 16 bytes on success.
    let ret = unsafe { mbr_uid_to_uuid(uid, bytes.as_mut_ptr()) };
    if ret != 0 {
        return Err(io::Error::from_raw_os_error(ret));
    }
    Ok(bytes)
}

fn gid_to_guid(gid: GidT) -> io::Result<[u8; 16]> {
    let mut bytes = [0_u8; 16];
    // SAFETY: `mbr_gid_to_uuid` writes exactly 16 bytes on success.
    let ret = unsafe { mbr_gid_to_uuid(gid, bytes.as_mut_ptr()) };
    if ret != 0 {
        return Err(io::Error::from_raw_os_error(ret));
    }
    Ok(bytes)
}

// MARK: small helpers

/// Yield each set single bit, lowest first (matches exacl's `BitIter` order,
/// including unknown bits, so set-path behavior is identical).
fn single_bits(mut bits: u32) -> Vec<u32> {
    let mut out = Vec::new();
    while bits != 0 {
        let bit = 1_u32 << bits.trailing_zeros();
        bits &= !bit;
        out.push(bit);
    }
    out
}

/// Parse a hyphenated lowercase UUID (`8-4-4-4-12` hex), the inverse of
/// [`format_guid`].
fn parse_guid(text: &str) -> io::Result<[u8; 16]> {
    let hex: String = text.chars().filter(|c| *c != '-').collect();
    if text.len() != 36 || hex.len() != 32 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("not a UUID: {text:?}"),
        ));
    }
    // Dashes must sit at the 8-4-4-4-12 boundaries.
    for (i, c) in text.chars().enumerate() {
        let want_dash = matches!(i, 8 | 13 | 18 | 23);
        if (c == '-') != want_dash {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("not a UUID: {text:?}"),
            ));
        }
    }
    let mut guid = [0_u8; 16];
    for (i, chunk) in hex.as_bytes().chunks(2).enumerate() {
        let pair = std::str::from_utf8(chunk).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidInput, format!("not a UUID: {text:?}"))
        })?;
        guid[i] = u8::from_str_radix(pair, 16).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidInput, format!("not a UUID: {text:?}"))
        })?;
    }
    Ok(guid)
}

/// Format a UUID lowercase hyphenated, matching the `uuid` crate's
/// `to_string` that exacl uses for `Guid` qualifier names.
fn format_guid(guid: &[u8; 16]) -> String {
    let h = |i: usize| format!("{:02x}", guid[i]);
    format!(
        "{}{}{}{}-{}{}-{}{}-{}{}-{}{}{}{}{}{}",
        h(0),
        h(1),
        h(2),
        h(3),
        h(4),
        h(5),
        h(6),
        h(7),
        h(8),
        h(9),
        h(10),
        h(11),
        h(12),
        h(13),
        h(14),
        h(15),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guid_text_round_trip_is_stable() {
        let guid = [
            0xff, 0xff, 0xee, 0xee, 0xdd, 0xdd, 0xcc, 0xcc, 0xbb, 0xbb, 0xaa, 0xaa, 0x00, 0x00,
            0x00, 0x59,
        ];
        assert_eq!(format_guid(&guid), "ffffeeee-dddd-cccc-bbbb-aaaa00000059");
        assert_eq!(parse_guid(&format_guid(&guid)).unwrap(), guid);
        assert!(parse_guid("not-a-uuid").is_err());
        assert!(parse_guid("ffffeeee-dddd-cccc-bbbb-aaaa0000005").is_err());
    }

    #[test]
    fn single_bits_yields_lsb_first() {
        assert_eq!(single_bits(0), Vec::<u32>::new());
        assert_eq!(single_bits(2 + 4 + 16), vec![2, 4, 16]);
    }

    /// Differential test against exacl's path API: whatever the fd
    /// implementation reads must equal what exacl reads from the path, and a
    /// write through the fd must be visible through the path API.
    #[test]
    fn fd_read_matches_path_read_and_fd_write_round_trips() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("file");
        std::fs::write(&path, b"data").unwrap();

        let file = std::fs::File::open(&path).unwrap();
        let fd_entries = read_fd_entries(fd_of(&file)).unwrap();
        let path_entries = exacl::getfacl(&path, None).unwrap();
        assert_eq!(fd_entries, path_entries);

        // Plant one entry through the fd and confirm both APIs agree.
        let uid = unsafe { libc::getuid() };
        let planted = vec![AclEntry::allow_user(
            &uid.to_string(),
            Perm::READ,
            Flag::empty(),
        )];
        write_fd_entries(fd_of(&file), &planted).unwrap();
        let reread_fd = read_fd_entries(fd_of(&file)).unwrap();
        let reread_path = exacl::getfacl(&path, None).unwrap();
        assert_eq!(reread_fd, reread_path);
        assert_eq!(reread_fd.len(), 1);
        assert_eq!(reread_fd[0].kind, AclEntryKind::User);
        assert!(reread_fd[0].allow);

        // Clearing through the fd matches exacl's empty-set clear.
        write_fd_entries(fd_of(&file), &[]).unwrap();
        assert!(read_fd_entries(fd_of(&file)).unwrap().is_empty());
        assert!(exacl::getfacl(&path, None).unwrap().is_empty());
    }

    /// Directories with allow + deny + inherit flags: the mapped entries
    /// must still match exacl's path API exactly.
    #[test]
    fn fd_directory_with_flags_matches_path_api() {
        let dir = tempfile::TempDir::new().unwrap();
        let sub = dir.path().join("sub");
        std::fs::create_dir(&sub).unwrap();
        let uid = unsafe { libc::getuid() };
        let gid = unsafe { libc::getgid() };
        // Decimal ids always resolve (exacl falls back to numeric parse),
        // so this needs no fixture user/group names.
        let planted = vec![
            AclEntry::allow_user(
                &uid.to_string(),
                Perm::READ | Perm::EXECUTE,
                Flag::FILE_INHERIT | Flag::DIRECTORY_INHERIT,
            ),
            AclEntry::deny_group(&gid.to_string(), Perm::WRITE, Flag::empty()),
        ];
        let handle = std::fs::File::open(&sub).unwrap();
        write_fd_entries(fd_of(&handle), &planted).unwrap();
        let reread_fd = read_fd_entries(fd_of(&handle)).unwrap();
        let reread_path = exacl::getfacl(&sub, None).unwrap();
        assert_eq!(reread_fd, reread_path);
        assert_eq!(reread_fd.len(), 2);
        // exacl text round-trips through the same representation the wire
        // carries, so this also pins the on-wire format agreement.
        let text = exacl::to_string(&reread_fd).unwrap();
        assert_eq!(exacl::from_str(&text).unwrap(), reread_fd);
    }

    #[cfg(unix)]
    fn fd_of(file: &std::fs::File) -> RawFd {
        use std::os::unix::io::AsRawFd;
        file.as_raw_fd()
    }
}
