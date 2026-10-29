//! Linux 6.12+ native export handles, never bare device/inode inequality.
use super::*;

#[derive(Clone, Copy)]
enum Backend {
    Ext,
    Xfs,
    Btrfs,
}

#[repr(C)]
struct Handle {
    length: u32,
    kind: i32,
    bytes: [u8; 128],
}

struct MountedHandle {
    mount: u64,
    handle: Handle,
}

impl Backend {
    fn key(self, value: &MountedHandle) -> Option<NativeObjectKey> {
        let h = &value.handle;
        // Linux exportfs.h and the filesystem's encode_fh implementation.
        // FID asks for identity, not a reconnectable parent-bearing handle.
        let (tag, inode, root) = match (self, h.kind, h.length) {
            (Self::Ext, 1, 8) | (Self::Xfs, 1, 8) => (
                if matches!(self, Self::Ext) {
                    *b"extN"
                } else {
                    *b"xfs\0"
                },
                u64::from(u32::from_ne_bytes(h.bytes[..4].try_into().ok()?)),
                0,
            ),
            (Self::Xfs, 0x81, 12) => (
                *b"xfs\0",
                u64::from_ne_bytes(h.bytes[..8].try_into().ok()?),
                0,
            ),
            (Self::Btrfs, 0x4d, 20) => (
                *b"btrf",
                u64::from_ne_bytes(h.bytes[..8].try_into().ok()?),
                u64::from_ne_bytes(h.bytes[8..16].try_into().ok()?),
            ),
            _ => return None,
        };
        if value.mount == 0 || inode == 0 || (matches!(self, Self::Btrfs) && root == 0) {
            return None;
        }
        let mut key = [0; 32];
        key[..4].copy_from_slice(&tag);
        key[4..12].copy_from_slice(&value.mount.to_ne_bytes());
        key[12..20].copy_from_slice(&root.to_ne_bytes());
        key[24..].copy_from_slice(&inode.to_ne_bytes());
        // Generation is intentionally not part of the domain: it varies by
        // object. Reused inodes can collide, conservatively refusing an effect;
        // this key never replaces original SOURCE binding/ctime validation.
        Some(NativeObjectKey(key))
    }
}

fn handle(fd: RawFd, name: Option<&OsStr>) -> Result<Option<MountedHandle>> {
    let name = name.map(component_cstring).transpose()?;
    let mut value = MountedHandle {
        mount: 0,
        handle: Handle {
            length: 128,
            kind: 0,
            bytes: [0; 128],
        },
    };
    let flags = libc::AT_HANDLE_FID
        | libc::AT_HANDLE_MNT_ID_UNIQUE
        | if name.is_none() {
            libc::AT_EMPTY_PATH
        } else {
            0
        };
    let name = name.as_ref().map_or(c"".as_ptr(), |name| name.as_ptr());
    // SAFETY: fd is held; name is a live NUL-terminated component or empty.
    // Handle has the file_handle header and 128 writable bytes. UNIQUE requests
    // an eight-byte mount ID, so its output points to u64, NOT libc's old int*.
    // No AT_SYMLINK_FOLLOW: named links and O_PATH link FDs describe the link.
    let result = unsafe {
        libc::syscall(
            libc::SYS_name_to_handle_at,
            fd,
            name,
            &mut value.handle as *mut Handle,
            &mut value.mount as *mut u64,
            flags,
        )
    };
    if result != 0 {
        let error = std::io::Error::last_os_error();
        if matches!(
            error.raw_os_error(),
            Some(
                libc::ENOSYS
                    | libc::EINVAL
                    | libc::EOPNOTSUPP
                    | libc::EPERM
                    | libc::EACCES
                    | libc::EOVERFLOW
            )
        ) {
            return Ok(None);
        }
        return Err(error.into());
    }
    Ok(Some(value))
}

pub(in crate::rooted_fs) fn object_key(
    fd: RawFd,
    name: Option<&OsStr>,
) -> Result<Option<NativeObjectKey>> {
    let mut fs = MaybeUninit::<libc::statfs>::zeroed();
    // SAFETY: fd is held and fs is writable statfs storage.
    if unsafe { libc::fstatfs(fd, fs.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    // SAFETY: successful fstatfs initialized fs.
    let fs = unsafe { fs.assume_init() };
    // statfs magic signedness varies between GNU/musl and Linux targets.
    let backend = match i128::from(fs.f_type) {
        value if value == i128::from(libc::EXT4_SUPER_MAGIC) => Backend::Ext,
        value if value == i128::from(libc::XFS_SUPER_MAGIC) => Backend::Xfs,
        value if value == i128::from(libc::BTRFS_SUPER_MAGIC) => Backend::Btrfs,
        _ => return Ok(None),
    };
    let Some(parent) = handle(fd, None)? else {
        return Ok(None);
    };
    let Some(parent_key) = backend.key(&parent) else {
        return Ok(None);
    };
    let Some(name) = name else {
        return Ok(Some(parent_key));
    };
    let Some(named) = handle(fd, Some(name))? else {
        return Ok(None);
    };
    // The named object must belong to the held parent's recognized backend.
    // A mount transition cannot borrow its parent's filesystem qualification.
    if named.mount != parent.mount {
        return Ok(None);
    }
    Ok(backend.key(&named))
}

/// Corroborate a self-parent termination without mistaking chroot for a native
/// domain root. ext2/3/4 reserve inode 2; Btrfs subvolume roots are inode 256.
/// XFS root inode numbers are not fixed: unknown self-parentage must refuse.
pub(in crate::rooted_fs::local_source_effects) fn directory_domain_root(
    object: NativeObjectKey,
) -> bool {
    let mut inode = [0; 8];
    inode.copy_from_slice(&object.0[24..]);
    matches!(
        (&object.0[..4], u64::from_ne_bytes(inode)),
        (b"extN", 2) | (b"btrf", 256)
    )
}

#[cfg(test)]
mod tests;
