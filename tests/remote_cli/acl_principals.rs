//! Principal fidelity is checked against native qualifiers, not same-host NSS text.
use super::*;
use std::path::Path;

#[tokio::test]
async fn requested_acls_keep_raw_principals_in_local_push_and_pull() {
    let fixture = tempfile::tempdir().unwrap();
    let search_path = ssh_path(fixture.path());
    for direction in ["local", "push", "pull"] {
        let source = fixture.path().join(direction).join("source");
        let destination = fixture.path().join(direction).join("destination");
        std::fs::create_dir_all(source.join("dir")).unwrap();
        std::fs::create_dir_all(&destination).unwrap();
        for name in ["created", "changed", "metadata"] {
            std::fs::write(source.join(name), b"SOURCE").unwrap();
        }
        std::fs::write(destination.join("changed"), b"old").unwrap();
        std::fs::write(destination.join("metadata"), b"TARGET").unwrap();
        // Equal size/time must preserve OLD bytes while reconciling the ACL.
        for path in [source.join("metadata"), destination.join("metadata")] {
            filetime::set_file_mtime(path, filetime::FileTime::from_unix_time(1_600_000_000, 0))
                .unwrap();
        }
        let names = ["created", "changed", "metadata", "dir"];
        for name in names {
            plant(&source.join(name));
        }
        let expected = names.map(|name| native(&source.join(name)));
        let address = |path: &Path| {
            let mut address = OsString::from("test-peer:");
            address.push(path);
            address
        };
        let mut source_arg = if direction == "pull" {
            address(&source)
        } else {
            source.as_os_str().to_owned()
        };
        source_arg.push("/");
        let destination_arg = if direction == "push" {
            address(&destination)
        } else {
            destination.as_os_str().to_owned()
        };
        copy(&source_arg, &destination_arg, &["-A"], &search_path).await;
        for (name, expected) in names.into_iter().zip(&expected) {
            assert_eq!(
                native(&destination.join(name)),
                *expected,
                "{direction}/{name}"
            );
            assert_eq!(native(&source.join(name)), *expected, "SOURCE changed");
        }
        assert_eq!(
            std::fs::read(destination.join("created")).unwrap(),
            b"SOURCE"
        );
        assert_eq!(
            std::fs::read(destination.join("changed")).unwrap(),
            b"SOURCE"
        );
        assert_eq!(
            std::fs::read(destination.join("metadata")).unwrap(),
            b"TARGET"
        );
    }
}

#[cfg(target_os = "linux")]
fn plant(path: &Path) {
    let mut entries = exacl::getfacl(path, None).unwrap();
    entries.extend([
        exacl::AclEntry::allow_user("0", exacl::Perm::READ, exacl::Flag::empty()),
        exacl::AclEntry::allow_group("0", exacl::Perm::READ, exacl::Flag::empty()),
        exacl::AclEntry::allow_mask(exacl::Perm::READ, exacl::Flag::empty()),
    ]);
    exacl::setfacl(&[path], &entries, None).unwrap();
}

#[cfg(target_os = "linux")]
fn native(path: &Path) -> Vec<u8> {
    let raw = xattr::get(path, "system.posix_acl_access")
        .unwrap()
        .unwrap();
    assert_eq!(&raw[..4], &2_u32.to_le_bytes());
    for tag in [2_u16, 8] {
        assert!(raw[4..].chunks_exact(8).any(|entry| {
            u16::from_le_bytes(entry[..2].try_into().unwrap()) == tag
                && u32::from_le_bytes(entry[4..].try_into().unwrap()) == 0
        }));
    }
    raw
}

#[cfg(target_os = "macos")]
use macos::{native, plant};

#[cfg(target_os = "macos")]
mod macos {
    use super::*;
    use std::ffi::c_void;
    use std::os::fd::AsRawFd;
    use std::ptr;

    extern "C" {
        fn acl_init(count: i32) -> *mut c_void;
        fn acl_create_entry(acl: *mut *mut c_void, entry: *mut *mut c_void) -> i32;
        fn acl_set_tag_type(entry: *mut c_void, tag: u32) -> i32;
        fn acl_set_qualifier(entry: *mut c_void, guid: *const c_void) -> i32;
        fn acl_get_permset(entry: *mut c_void, perms: *mut *mut c_void) -> i32;
        fn acl_add_perm(perms: *mut c_void, bit: u32) -> i32;
        fn acl_get_flagset_np(entry: *mut c_void, flags: *mut *mut c_void) -> i32;
        fn acl_add_flag_np(flags: *mut c_void, bit: u32) -> i32;
        fn acl_set_fd(fd: i32, acl: *mut c_void) -> i32;
        fn acl_get_fd(fd: i32) -> *mut c_void;
        fn acl_get_entry(acl: *mut c_void, id: i32, entry: *mut *mut c_void) -> i32;
        fn acl_get_tag_type(entry: *mut c_void, tag: *mut u32) -> i32;
        fn acl_get_qualifier(entry: *mut c_void) -> *mut c_void;
        fn acl_get_perm_np(perms: *mut c_void, bit: u32) -> i32;
        fn acl_get_flag_np(flags: *mut c_void, bit: u32) -> i32;
        fn acl_free(object: *mut c_void) -> i32;
    }

    fn qualifiers() -> [[u8; 16]; 3] {
        let mut alias = [
            0xff, 0xff, 0xee, 0xee, 0xdd, 0xdd, 0xcc, 0xcc, 0xbb, 0xbb, 0xaa, 0xaa, 0, 0, 0, 0,
        ];
        // SAFETY: getuid reads process credentials without pointer arguments.
        alias[12..].copy_from_slice(&unsafe { libc::getuid() }.to_be_bytes());
        [alias, [0x6b; 16], alias]
    }

    pub(super) fn plant(path: &Path) {
        let file = std::fs::File::open(path).unwrap();
        // SAFETY: these native fixture calls use live owned ACLs, borrowed
        // handles, and exact 16-byte qualifiers; all results are checked.
        unsafe {
            let mut acl = acl_init(3);
            assert!(!acl.is_null());
            for (index, guid) in qualifiers().iter().enumerate() {
                let mut entry = ptr::null_mut();
                assert_eq!(acl_create_entry(&mut acl, &mut entry), 0);
                assert_eq!(acl_set_tag_type(entry, if index == 0 { 1 } else { 2 }), 0);
                assert_eq!(acl_set_qualifier(entry, guid.as_ptr().cast()), 0);
                let mut perms = ptr::null_mut();
                assert_eq!(acl_get_permset(entry, &mut perms), 0);
                assert_eq!(
                    acl_add_perm(perms, if index == 0 { 1 << 1 } else { 1 << 2 }),
                    0
                );
                if index == 0 {
                    let mut flags = ptr::null_mut();
                    assert_eq!(acl_get_flagset_np(entry, &mut flags), 0);
                    assert_eq!(acl_add_flag_np(flags, 1 << 5), 0);
                }
            }
            assert_eq!(acl_set_fd(file.as_raw_fd(), acl), 0);
            assert_eq!(acl_free(acl), 0);
        }
    }

    pub(super) fn native(path: &Path) -> Vec<(u32, [u8; 16], u32, u32)> {
        let file = std::fs::File::open(path).unwrap();
        let mut result = Vec::new();
        // SAFETY: file and ACL remain live; qualifier allocations are copied
        // before free and borrowed handles are never used after ACL free.
        unsafe {
            let acl = acl_get_fd(file.as_raw_fd());
            assert!(!acl.is_null());
            let mut id = 0;
            loop {
                let mut entry = ptr::null_mut();
                if acl_get_entry(acl, id, &mut entry) != 0 {
                    break;
                }
                let mut tag = 0;
                assert_eq!(acl_get_tag_type(entry, &mut tag), 0);
                let qualifier = acl_get_qualifier(entry).cast::<[u8; 16]>();
                assert!(!qualifier.is_null());
                let guid = *qualifier;
                assert_eq!(acl_free(qualifier.cast()), 0);
                let mut perms = ptr::null_mut();
                let mut flags = ptr::null_mut();
                assert_eq!(acl_get_permset(entry, &mut perms), 0);
                assert_eq!(acl_get_flagset_np(entry, &mut flags), 0);
                let mut permission_bits = 0;
                let mut flag_bits = 0;
                for bit in (1..=13).chain([20]).map(|shift| 1 << shift) {
                    let present = acl_get_perm_np(perms, bit);
                    assert!(matches!(present, 0 | 1));
                    if present == 1 {
                        permission_bits |= bit;
                    }
                }
                for bit in [1 << 4, 1 << 5, 1 << 6, 1 << 7, 1 << 8] {
                    let present = acl_get_flag_np(flags, bit);
                    assert!(matches!(present, 0 | 1));
                    if present == 1 {
                        flag_bits |= bit;
                    }
                }
                result.push((tag, guid, permission_bits, flag_bits));
                id = -1;
            }
            assert_eq!(acl_free(acl), 0);
        }
        assert_eq!(
            result.iter().map(|entry| entry.1).collect::<Vec<_>>(),
            qualifiers()
        );
        assert_eq!(
            result.iter().map(|entry| entry.0).collect::<Vec<_>>(),
            [1, 2, 2]
        );
        result
    }
}
