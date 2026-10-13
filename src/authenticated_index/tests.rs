use super::*;

type Index = AuthenticatedIndex<80>;
const RECORD_BYTES: usize = 113;
fn state(n: u64) -> [u8; 80] {
    let mut value = [0; 80];
    value[..8].copy_from_slice(&n.to_le_bytes());
    value
}
fn assert_terminal(index: &mut Index) {
    for key in [[0; 32], [1; 32], [255; 32]] {
        let error = index.get_bytes(key).unwrap_err();
        assert!(matches!(
            error.get_ref().and_then(|e| e.downcast_ref::<StateError>()),
            Some(StateError::Terminal)
        ));
        assert!(index.set_bytes(key, state(9)).is_err());
    }
}

#[test]
fn full_depth_lookup_update_and_absence_are_bounded_without_history_growth() {
    let mut index = Index::new().unwrap();
    index.set_bytes([0; 32], state(0)).unwrap();
    for depth in (0..LEVELS).rev() {
        let mut key = [0; 32];
        key[depth / 2] = if depth.is_multiple_of(2) { 0x10 } else { 1 };
        index.set_bytes(key, state(depth as u64 + 1)).unwrap();
    }
    assert_eq!(index.proof([0; 32]).unwrap().len, LEVELS);
    let extent = index.end;
    assert_eq!(
        extent,
        ((LEVELS + 1) * RECORD_BYTES + LEVELS * BRANCH_BYTES) as u64
    );
    for update in 0..100 {
        index.set_bytes([0; 32], state(update)).unwrap();
        assert_eq!(index.get_bytes([0; 32]).unwrap(), Some(state(update)));
        assert_eq!(index.end, extent);
        assert_eq!(index.file.metadata().unwrap().len(), extent);
    }
    for depth in 0..LEVELS {
        let mut key = [0; 32];
        key[depth / 2] = if depth.is_multiple_of(2) { 0x10 } else { 1 };
        assert_eq!(index.get_bytes(key).unwrap(), Some(state(depth as u64 + 1)));
        key[depth / 2] *= 2;
        assert_eq!(index.get_bytes(key).unwrap(), None);
    }
    assert_eq!(index.get_bytes([255; 32]).unwrap(), None);
}

#[test]
fn exact_map_keeps_all_siblings_during_mixed_insert_and_update() {
    let keys: [[u8; 32]; 256] =
        std::array::from_fn(|n| *blake3::hash(&(n as u64).to_le_bytes()).as_bytes());
    let mut index = Index::new().unwrap();
    for (n, key) in keys.iter().enumerate() {
        index.set_bytes(*key, state(n as u64)).unwrap();
    }
    let end = index.end;
    for (n, key) in keys.iter().enumerate().rev() {
        assert_eq!(index.get_bytes(*key).unwrap(), Some(state(n as u64)));
        index.set_bytes(*key, state(n as u64 + 256)).unwrap();
    }
    for (n, key) in keys.iter().enumerate() {
        assert_eq!(index.get_bytes(*key).unwrap(), Some(state(n as u64 + 256)));
        let absent = *blake3::hash(&(n as u64 + 256).to_le_bytes()).as_bytes();
        assert_eq!(index.get_bytes(absent).unwrap(), None);
    }
    assert_eq!(index.end, end);
    assert!(end <= (keys.len() * RECORD_BYTES + (keys.len() - 1) * BRANCH_BYTES) as u64);
}

#[test]
fn payload_index_and_truncation_fail_terminally_before_decode_or_absence() {
    for fault in 0..5 {
        let mut index = Index::new().unwrap();
        index.set_bytes([0; 32], state(1)).unwrap();
        index.set_bytes([255; 32], state(2)).unwrap();
        let root = index.root;
        match fault {
            0 => {
                index.file.seek(SeekFrom::Start(33)).unwrap();
                index.file.write_all(&state(7)).unwrap();
            }
            1 => {
                index
                    .file
                    .seek(SeekFrom::Start(root.unwrap().offset + 34))
                    .unwrap();
                index
                    .file
                    .write_all(&(RECORD_BYTES as u64).to_le_bytes())
                    .unwrap();
            }
            2 => index.file.set_len(index.end - 1).unwrap(),
            3 => {
                index
                    .file
                    .seek(SeekFrom::Start(root.unwrap().offset))
                    .unwrap();
                index.file.write_all(&[255]).unwrap();
            }
            _ => {
                // A fully self-consistent replacement map still cannot replace
                // the retained native root digest, even at identical offsets.
                let mut forged = Index::new().unwrap();
                forged.set_bytes([0; 32], state(3)).unwrap();
                forged.set_bytes([255; 32], state(4)).unwrap();
                assert_eq!(forged.end, index.end);
                index.file.seek(SeekFrom::Start(0)).unwrap();
                forged.file.seek(SeekFrom::Start(0)).unwrap();
                std::io::copy(&mut forged.file, &mut index.file).unwrap();
            }
        }
        assert!(index.get_bytes([0; 32]).is_err());
        assert_eq!(index.root, root);
        assert_terminal(&mut index);
    }
}

#[test]
fn failed_state_write_retains_old_root_and_poisons_all_operations() {
    let mut index = Index::new().unwrap();
    index.set_bytes([0; 32], state(1)).unwrap();
    index.set_bytes([255; 32], state(2)).unwrap();
    let root = index.root;
    let end = index.end;
    let mut scratch = tempfile::NamedTempFile::new().unwrap();
    index.file.seek(SeekFrom::Start(0)).unwrap();
    std::io::copy(&mut index.file, scratch.as_file_mut()).unwrap();
    index.file = File::open(scratch.path()).unwrap();
    assert_eq!(index.get_bytes([0; 32]).unwrap(), Some(state(1)));
    assert!(index.set_bytes([0; 32], state(3)).is_err());
    assert_eq!(index.root, root);
    assert_eq!(index.end, end);
    assert_terminal(&mut index);
}

#[test]
fn malformed_native_extents_and_authenticated_node_structure_are_rejected() {
    let mut index = Index::new().unwrap();
    index.set_bytes([0; 32], state(1)).unwrap();
    index.root.as_mut().unwrap().offset = u64::MAX - 1;
    assert!(index.get_bytes([0; 32]).is_err());
    assert_terminal(&mut index);
    for bad_depth in [false, true] {
        let mut index = Index::new().unwrap();
        index.set_bytes([0; 32], state(1)).unwrap();
        index.set_bytes([255; 32], state(2)).unwrap();
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
        // Exercise structural checks independently of authentication. Scratch
        // attackers cannot rewrite this native-owned RAM digest.
        index.root.as_mut().unwrap().digest = digest(&bytes);
        assert!(index.get_bytes([0; 32]).is_err());
        assert_terminal(&mut index);
    }
}
