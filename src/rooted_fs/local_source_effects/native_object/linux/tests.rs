use super::*;

fn mounted(kind: i32, length: u32, inode: u64, root: u64) -> MountedHandle {
    let mut bytes = [0; 128];
    if kind == 1 {
        bytes[..4].copy_from_slice(&u32::try_from(inode).unwrap().to_ne_bytes());
    } else {
        bytes[..8].copy_from_slice(&inode.to_ne_bytes());
    }
    if kind == 0x4d {
        bytes[8..16].copy_from_slice(&root.to_ne_bytes());
    }
    MountedHandle {
        mount: 42,
        handle: Handle {
            length,
            kind,
            bytes,
        },
    }
}

#[test]
fn only_exact_backend_handle_layouts_qualify() {
    for (backend, kind, length) in [
        (Backend::Ext, 1, 8),
        (Backend::Xfs, 1, 8),
        (Backend::Xfs, 0x81, 12),
        (Backend::Btrfs, 0x4d, 20),
    ] {
        let mut handle = mounted(kind, length, 29, 257);
        let key = backend.key(&handle).unwrap();
        let other = backend.key(&mounted(kind, length, 30, 257)).unwrap();
        assert!(key.independent(other));
        // Generations cannot split a volume domain or mint historical identity.
        handle.handle.bytes[length as usize - 1] = 7;
        assert!(backend.key(&handle) == Some(key));
        handle.handle.length = length - 1;
        assert!(backend.key(&handle).is_none());
        handle.handle.length = 129;
        assert!(backend.key(&handle).is_none());
        handle.handle.length = length;
        handle.handle.kind = 0x4e;
        assert!(backend.key(&handle).is_none());
    }
    assert!(Backend::Ext.key(&mounted(0x4d, 20, 29, 257)).is_none());
    let first = Backend::Btrfs.key(&mounted(0x4d, 20, 256, 257)).unwrap();
    let other_root = Backend::Btrfs.key(&mounted(0x4d, 20, 256, 258)).unwrap();
    assert!(!first.independent(other_root));
    let mut other_mount = mounted(0x4d, 20, 29, 257);
    other_mount.mount += 1;
    assert!(!first.independent(Backend::Btrfs.key(&other_mount).unwrap()));
}

#[test]
fn self_parent_needs_a_native_domain_root_not_a_chroot_boundary() {
    for (backend, kind, length, root_inode) in
        [(Backend::Ext, 1, 8, 2), (Backend::Btrfs, 0x4d, 20, 256)]
    {
        let root = backend
            .key(&mounted(kind, length, root_inode, 257))
            .unwrap();
        assert!(directory_domain_root(root));
        let ordinary = backend.key(&mounted(kind, length, 999, 257)).unwrap();
        assert!(!directory_domain_root(ordinary));
    }
    let unknown_root = Backend::Xfs.key(&mounted(0x81, 12, 2, 0)).unwrap();
    assert!(!directory_domain_root(unknown_root));
}

#[test]
fn unsupported_procfs_does_not_borrow_export_handle_semantics() {
    let proc = File::open("/proc/self").unwrap();
    assert!(object_key(proc.as_raw_fd(), None).unwrap().is_none());
}

#[test]
#[ignore = "requires Linux 6.12+ and TMPDIR on native ext2/3/4, XFS or Btrfs"]
fn native_handles_need_no_data_authority_and_do_not_follow_links() {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let fixture = tempfile::tempdir().unwrap();
    std::fs::write(fixture.path().join("file"), b"SOURCE").unwrap();
    std::fs::hard_link(fixture.path().join("file"), fixture.path().join("alias")).unwrap();
    std::os::unix::fs::symlink("file", fixture.path().join("link")).unwrap();
    std::fs::set_permissions(
        fixture.path().join("file"),
        std::fs::Permissions::from_mode(0o000),
    )
    .unwrap();
    let parent = File::open(fixture.path()).unwrap();
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_PATH | libc::O_NOFOLLOW)
        .open(fixture.path().join("file"))
        .unwrap();
    let link = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_PATH | libc::O_NOFOLLOW)
        .open(fixture.path().join("link"))
        .unwrap();
    let key = object_key(file.as_raw_fd(), None).unwrap().unwrap();
    let alias = object_key(parent.as_raw_fd(), Some(OsStr::new("alias")))
        .unwrap()
        .unwrap();
    let named_link = object_key(parent.as_raw_fd(), Some(OsStr::new("link")))
        .unwrap()
        .unwrap();
    assert!(key == alias);
    assert!(named_link == object_key(link.as_raw_fd(), None).unwrap().unwrap());
    assert!(key.independent(named_link));
}
