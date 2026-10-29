//! Object equivalence only; this does not certify namespace ancestry.
use super::*;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(in crate::rooted_fs) struct NativeObjectKey(pub(in crate::rooted_fs) [u8; 32]);

impl NativeObjectKey {
    pub(in crate::rooted_fs) fn independent(self, other: Self) -> bool {
        // Different/unknown volumes are not negative certificates.
        self.0[..24] == other.0[..24] && self != other
    }
}

#[cfg(target_os = "macos")]
pub(in crate::rooted_fs) fn object_key(
    fd: RawFd,
    name: Option<&OsStr>,
) -> Result<Option<NativeObjectKey>> {
    let mut fs = MaybeUninit::<libc::statfs>::zeroed();
    // SAFETY: fd is held by the caller, fs is writable statfs storage.
    if unsafe { libc::fstatfs(fd, fs.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    // SAFETY: successful fstatfs initialized fs.
    let fs = unsafe { fs.assume_init() };
    // Recognition narrows the implementation, but is not the certificate: the
    // actual authorized returned attributes below must also qualify the view.
    if fs.f_fstypename[..5] != [b'a' as i8, b'p' as i8, b'f' as i8, b's' as i8, 0] {
        return Ok(None);
    }
    let mask = libc::ATTR_CMN_RETURNED_ATTRS
        | libc::ATTR_CMN_DEVID
        | libc::ATTR_CMN_FSID
        | libc::ATTR_CMN_FILEID;
    let mut attrs = libc::attrlist {
        bitmapcount: libc::ATTR_BIT_MAP_COUNT,
        reserved: 0,
        commonattr: mask,
        volattr: 0,
        dirattr: 0,
        fileattr: 0,
        forkattr: 0,
    };
    // getattrlist(2): fixed attributes are packed on 4-byte boundaries, in
    // attribute-bit order. RETURN_REALDEV requests physical DEVID/FSID for
    // volume-group views. FILEID is the mounted-volume object ID, not LINKID.
    let mut bytes = [0u32; 11];
    let name = name.map(component_cstring).transpose()?;
    // SAFETY: attrs and the aligned output buffer have their declared sizes;
    // any name is one live component, relative to the genuine held directory.
    let result = unsafe {
        match name {
            Some(name) => libc::getattrlistat(
                fd,
                name.as_ptr(),
                (&mut attrs as *mut libc::attrlist).cast(),
                bytes.as_mut_ptr().cast(),
                std::mem::size_of_val(&bytes),
                libc::c_ulong::from(libc::FSOPT_NOFOLLOW | libc::FSOPT_RETURN_REALDEV),
            ),
            None => libc::fgetattrlist(
                fd,
                (&mut attrs as *mut libc::attrlist).cast(),
                bytes.as_mut_ptr().cast(),
                std::mem::size_of_val(&bytes),
                libc::FSOPT_RETURN_REALDEV,
            ),
        }
    };
    if result != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    if bytes[0] != 44 || bytes[1] != mask || bytes[2..6] != [0; 4] {
        return Ok(None);
    }
    let mut key = [0u8; 32];
    key[..4].copy_from_slice(b"apfs");
    for (output, value) in key[4..16]
        .as_chunks_mut::<4>()
        .0
        .iter_mut()
        .zip(&bytes[6..9])
    {
        output.copy_from_slice(&value.to_ne_bytes());
    }
    // Keep the actual FD's mounted native view as well as real volume IDs.
    // Physical-volume equality does not qualify mixed logical/physical views.
    // SAFETY: Darwin fsid_t is two initialized i32 values (8 bytes), with no
    // padding; the output owns exactly 8 writable bytes and does not overlap.
    unsafe {
        std::ptr::copy_nonoverlapping(
            (&fs.f_fsid as *const libc::fsid_t).cast::<u8>(),
            key[16..24].as_mut_ptr(),
            8,
        );
    }
    key[24..28].copy_from_slice(&bytes[9].to_ne_bytes());
    key[28..32].copy_from_slice(&bytes[10].to_ne_bytes());
    Ok(Some(NativeObjectKey(key)))
}

#[cfg(target_os = "linux")]
pub(super) mod linux;
#[cfg(target_os = "linux")]
pub(in crate::rooted_fs) use linux::object_key;

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub(in crate::rooted_fs) fn object_key(
    _fd: RawFd,
    _name: Option<&OsStr>,
) -> Result<Option<NativeObjectKey>> {
    Ok(None)
}
