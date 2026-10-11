use super::*;
use crate::endpoint::source_root::SourceRoot;
use crate::engine::domain::{Entry, RelativePath, Timestamp};
use crate::engine::source_removals::{NativeState, RemovalCause};

fn state(n: u64) -> HardlinkSourceState {
    HardlinkSourceState {
        dev: 1,
        ino: n,
        size: 4,
        mode: 0o100644,
        mtime: 2,
        mtime_nsec: 0,
        ctime: 3,
        ctime_nsec: 0,
        nlink: 2,
        uid: 1000,
        gid: 1000,
        flags: 0,
    }
}

fn observed(root: &SourceRoot, name: &str) -> Entry {
    let path = RelativePath::new(name).unwrap();
    let (kind, identity) = root
        .rooted()
        .path_identity_blocking(&path)
        .unwrap()
        .unwrap();
    let mut entry = Entry::file(path, 4, Timestamp::UNIX_EPOCH);
    entry.kind = kind;
    entry.identity = Some(identity);
    entry
}

fn assert_terminal(index: &mut StateIndex) {
    for key in [[0; 32], [1; 32], [255; 32]] {
        let error = index.get(key).unwrap_err();
        assert!(matches!(
            error.get_ref().and_then(|e| e.downcast_ref::<StateError>()),
            Some(StateError::Terminal)
        ));
        assert!(index.set(key, state(9)).is_err());
    }
}

#[test]
fn self_consistent_payload_and_index_rewrite_cannot_authorize_foreign_source_alias() {
    for replace in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a"), b"same").unwrap();
        std::fs::hard_link(dir.path().join("a"), dir.path().join("b")).unwrap();
        let root = SourceRoot::open_blocking(dir.path().into()).unwrap();
        let a = observed(&root, "a");
        let b = observed(&root, "b");
        let key = *a.identity.unwrap().as_bytes();
        let mut native = NativeState::new(root.clone())
            .unwrap()
            .validate(&a)
            .unwrap()
            .validate(&b)
            .unwrap()
            .remove(&a)
            .unwrap();
        // Ensure the attack also rewrites a real index branch, not just a leaf.
        let mut other = key;
        other[0] ^= 0xf0;
        native.states.set(other, state(99)).unwrap();
        if replace {
            std::fs::write(dir.path().join("replacement"), b"foreign bytes").unwrap();
            std::fs::rename(dir.path().join("replacement"), dir.path().join("b")).unwrap();
        } else {
            std::fs::write(dir.path().join("b"), b"foreign bytes").unwrap();
        }
        // Obtain the exact foreign b state through a sacrificial alias, without
        // touching b. It is a state the old unauthenticated map could authorize.
        std::fs::hard_link(dir.path().join("b"), dir.path().join("probe")).unwrap();
        let probe = observed(&root, "probe");
        let foreign = root
            .rooted()
            .remove_hardlink_source_member_blocking(&probe.path, probe.identity.unwrap(), None)
            .unwrap();
        let mut forged = StateIndex::new().unwrap();
        forged.set(key, foreign).unwrap();
        forged.set(other, state(99)).unwrap();
        assert_eq!(forged.get(key).unwrap(), Some(foreign));
        assert_eq!(forged.end, native.states.end);
        assert_eq!(
            forged.root.unwrap().offset,
            native.states.root.unwrap().offset
        );
        assert_ne!(forged.root, native.states.root);
        // Replace ALL scratch bytes with a perfectly self-consistent Merkle map.
        // Only the native owner's RAM root/extent stay out of the attacker's reach.
        native.states.file.seek(SeekFrom::Start(0)).unwrap();
        forged.file.seek(SeekFrom::Start(0)).unwrap();
        std::io::copy(&mut forged.file, &mut native.states.file).unwrap();
        let error = match native.remove(&b) {
            Err(error) => error,
            Ok(_) => panic!("forged scratch authorized foreign source removal"),
        };
        assert_eq!(error.removed, 1);
        assert!(!error.removal_uncertain);
        let RemovalCause::Io(error) = error.cause else {
            panic!("scratch authentication failure lost its cause");
        };
        assert!(matches!(
            error.get_ref().and_then(|e| e.downcast_ref::<StateError>()),
            Some(StateError::Authentication)
        ));
        assert_eq!(
            std::fs::read(dir.path().join("b")).unwrap(),
            b"foreign bytes"
        );
    }
}

#[test]
fn native_unlink_updates_keep_original_authority_across_multiple_groups() {
    let dir = tempfile::tempdir().unwrap();
    for names in [["a", "b"], ["c", "d"]] {
        std::fs::write(dir.path().join(names[0]), b"same").unwrap();
        std::fs::hard_link(dir.path().join(names[0]), dir.path().join(names[1])).unwrap();
    }
    let root = SourceRoot::open_blocking(dir.path().into()).unwrap();
    let entries = ["a", "c", "b", "d"].map(|name| observed(&root, name));
    let mut native = NativeState::new(root).unwrap();
    for entry in &entries {
        native = native.validate(entry).unwrap();
    }
    for entry in &entries {
        native = native.remove(entry).unwrap();
    }
    assert_eq!(native.removed, 4);
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
}

#[test]
fn full_depth_lookup_update_and_absence_are_bounded_without_history_growth() {
    let mut index = StateIndex::new().unwrap();
    index.set([0; 32], state(0)).unwrap();
    // Force one branch at every nibble, and exercise splitting above old roots.
    for depth in (0..LEVELS).rev() {
        let mut key = [0; 32];
        key[depth / 2] = if depth.is_multiple_of(2) { 0x10 } else { 1 };
        index.set(key, state(depth as u64 + 1)).unwrap();
    }
    assert_eq!(index.proof([0; 32]).unwrap().len, LEVELS);
    let extent = index.end;
    assert_eq!(
        extent,
        ((LEVELS + 1) * STATE_RECORD_BYTES + LEVELS * BRANCH_BYTES) as u64
    );
    for update in 0..100 {
        index.set([0; 32], state(update)).unwrap();
        assert_eq!(index.get([0; 32]).unwrap(), Some(state(update)));
        assert_eq!(index.end, extent);
        assert_eq!(index.file.metadata().unwrap().len(), extent);
    }
    for depth in 0..LEVELS {
        let mut key = [0; 32];
        key[depth / 2] = if depth.is_multiple_of(2) { 0x10 } else { 1 };
        assert_eq!(index.get(key).unwrap(), Some(state(depth as u64 + 1)));
        key[depth / 2] *= 2;
        assert_eq!(index.get(key).unwrap(), None);
    }
    assert_eq!(index.get([255; 32]).unwrap(), None);
}

#[test]
fn exact_map_keeps_all_siblings_during_mixed_insert_and_update() {
    let keys: [[u8; 32]; 256] =
        std::array::from_fn(|n| *blake3::hash(&(n as u64).to_le_bytes()).as_bytes());
    let mut index = StateIndex::new().unwrap();
    for (n, key) in keys.iter().enumerate() {
        index.set(*key, state(n as u64)).unwrap();
    }
    let end = index.end;
    for (n, key) in keys.iter().enumerate().rev() {
        assert_eq!(index.get(*key).unwrap(), Some(state(n as u64)));
        index.set(*key, state(n as u64 + 256)).unwrap();
    }
    for (n, key) in keys.iter().enumerate() {
        assert_eq!(index.get(*key).unwrap(), Some(state(n as u64 + 256)));
        let absent = *blake3::hash(&(n as u64 + 256).to_le_bytes()).as_bytes();
        assert_eq!(index.get(absent).unwrap(), None);
    }
    assert_eq!(index.end, end);
    assert!(end <= (keys.len() * STATE_RECORD_BYTES + (keys.len() - 1) * BRANCH_BYTES) as u64);
}

#[test]
fn payload_index_and_truncation_fail_terminally_before_decode_or_absence() {
    for fault in 0..4 {
        let mut index = StateIndex::new().unwrap();
        index.set([0; 32], state(1)).unwrap();
        index.set([255; 32], state(2)).unwrap();
        let root = index.root;
        match fault {
            0 => {
                // Validly encoded state, exact key, but not our write.
                index.file.seek(SeekFrom::Start(33)).unwrap();
                index.file.write_all(&state(7).encode()).unwrap();
            }
            1 => {
                // Rewrite a mapping offset with another valid leaf locator.
                let offset = root.unwrap().offset + 34;
                index.file.seek(SeekFrom::Start(offset)).unwrap();
                index
                    .file
                    .write_all(&(STATE_RECORD_BYTES as u64).to_le_bytes())
                    .unwrap();
            }
            2 => index.file.set_len(index.end - 1).unwrap(),
            _ => {
                index
                    .file
                    .seek(SeekFrom::Start(root.unwrap().offset))
                    .unwrap();
                index.file.write_all(&[255]).unwrap();
            }
        }
        assert!(index.get([0; 32]).is_err());
        assert_eq!(index.root, root);
        assert_terminal(&mut index);
    }
}

#[test]
fn failed_state_write_retains_old_root_and_poisons_all_operations() {
    let mut index = StateIndex::new().unwrap();
    index.set([0; 32], state(1)).unwrap();
    index.set([255; 32], state(2)).unwrap();
    let root = index.root;
    let end = index.end;
    let mut scratch = tempfile::NamedTempFile::new().unwrap();
    index.file.seek(SeekFrom::Start(0)).unwrap();
    std::io::copy(&mut index.file, scratch.as_file_mut()).unwrap();
    index.file = File::open(scratch.path()).unwrap();
    assert_eq!(index.get([0; 32]).unwrap(), Some(state(1)));
    assert!(index.set([0; 32], state(3)).is_err());
    assert_eq!(index.root, root);
    assert_eq!(index.end, end);
    assert_terminal(&mut index);
}

#[test]
fn malformed_native_extents_and_authenticated_node_structure_are_rejected() {
    let mut index = StateIndex::new().unwrap();
    index.set([0; 32], state(1)).unwrap();
    index.root.as_mut().unwrap().offset = u64::MAX - 1;
    assert!(index.get([0; 32]).is_err());
    assert_terminal(&mut index);

    for bad_depth in [false, true] {
        let mut index = StateIndex::new().unwrap();
        index.set([0; 32], state(1)).unwrap();
        index.set([255; 32], state(2)).unwrap();
        let root = index.root.unwrap();
        let mut bytes = [0; BRANCH_BYTES];
        index.file.seek(SeekFrom::Start(root.offset)).unwrap();
        index.file.read_exact(&mut bytes).unwrap();
        if bad_depth {
            bytes[1] = 64;
        } else {
            bytes[34..42].copy_from_slice(&u64::MAX.wrapping_sub(1).to_le_bytes());
        }
        index.file.seek(SeekFrom::Start(root.offset)).unwrap();
        index.file.write_all(&bytes).unwrap();
        // Test decoder validation independently of tamper authentication. Real
        // scratch attackers cannot perform this mutation of the retained root.
        index.root.as_mut().unwrap().digest = digest(&bytes);
        assert!(index.get([0; 32]).is_err());
        assert_terminal(&mut index);
    }
}
