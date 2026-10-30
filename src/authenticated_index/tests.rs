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

// Construct digits bit by bit, independently of the reader's two-byte window.
fn digit_key(depth: usize, position: usize) -> [u8; 32] {
    let mut key = [0; 32];
    for offset in 0..DIGIT_BITS {
        if position & (1 << (DIGIT_BITS - 1 - offset)) != 0 {
            let bit = depth * DIGIT_BITS + offset;
            assert!(bit < 256, "nonzero padding bit");
            key[bit / 8] |= 1 << (7 - bit % 8);
        }
    }
    key
}

#[test]
fn every_key_bit_routes_through_cross_byte_digits_and_compressed_prefixes() {
    let keys: [[u8; 32]; 256] = std::array::from_fn(|bit| {
        let mut key = [0; 32];
        key[bit / 8] = 1 << (7 - bit % 8);
        key
    });
    for (bit, key) in keys.iter().enumerate() {
        for depth in 0..LEVELS {
            let expected_digit = if bit / 6 == depth {
                1 << (5 - bit % 6)
            } else {
                0
            };
            assert_eq!(
                digit(key, depth),
                expected_digit,
                "bit {bit}, depth {depth}"
            );
        }
        for depth in 0..=LEVELS {
            let expected_prefix = if bit < depth * 6 { *key } else { [0; 32] };
            assert_eq!(
                prefix(*key, depth),
                expected_prefix,
                "bit {bit}, depth {depth}"
            );
        }
    }
    let mut index = Index::new().unwrap();
    for (bit, key) in keys.iter().enumerate() {
        index.set_bytes(*key, state(bit as u64)).unwrap();
    }
    for (bit, key) in keys.iter().enumerate().rev() {
        assert_eq!(index.get_bytes(*key).unwrap(), Some(state(bit as u64)));
        index.set_bytes(*key, state(bit as u64 + 256)).unwrap();
    }
    for (bit, key) in keys.iter().enumerate() {
        assert_eq!(
            index.get_bytes(*key).unwrap(),
            Some(state(bit as u64 + 256))
        );
        let mut absent = *key;
        absent[(bit + 1) % 256 / 8] |= 1 << (7 - (bit + 1) % 8);
        assert_eq!(index.get_bytes(absent).unwrap(), None);
    }
}

#[test]
fn canonical_digests_match_independent_transcripts_across_all_bitmap_windows() {
    // Exhaust every 16-bit pattern in each of the four bitmap windows. Dense
    // prefixes additionally cross every occupancy count and BLAKE3 boundary.
    let masks = (0..=u16::MAX)
        .flat_map(|mask| (0..4).map(move |window| u64::from(mask) << (window * 16)))
        .chain((0..=64).map(|count| {
            if count == 64 {
                u64::MAX
            } else {
                (1u64 << count) - 1
            }
        }));
    for mask in masks {
        let mut bytes = [0; BRANCH_BYTES];
        bytes[1..4].copy_from_slice(&[3, 0x12, 0x30]);
        let mut transcript = b"sy/authenticated-index/v1\0".to_vec();
        transcript.extend_from_slice(&bytes[..34]);
        transcript.extend_from_slice(&mask.to_le_bytes());
        for child in 0..64 {
            let start = 34 + child * 40;
            if mask & (1 << child) == 0 {
                bytes[start..start + 8].copy_from_slice(&u64::MAX.to_le_bytes());
            } else {
                // Distinct locators, including offset zero and bitmap bit 63.
                bytes[start..start + 8]
                    .copy_from_slice(&(child as u64 * RECORD_BYTES as u64).to_le_bytes());
                bytes[start + 8..start + 40].fill(child as u8 + 1);
                transcript.extend_from_slice(&bytes[start..start + 40]);
            }
        }
        assert_eq!(
            digest(&bytes).unwrap(),
            *blake3::hash(&transcript).as_bytes(),
            "mask {mask:016x}"
        );
    }
    let mut leaf = [0x6d; BRANCH_BYTES];
    leaf[0] = 1;
    // Include every supported leaf payload size and all compression boundaries.
    for length in 33..=BRANCH_BYTES {
        let mut reference = blake3::Hasher::new();
        reference.update(b"sy/authenticated-index/v1\0");
        reference.update(&leaf[..length]);
        assert_eq!(
            digest(&leaf[..length]).unwrap(),
            *reference.finalize().as_bytes(),
            "leaf length {length}"
        );
    }
}

#[test]
fn every_branch_bit_is_authenticated_or_rejected_as_noncanonical() {
    let mut index = Index::new().unwrap();
    index.set_bytes([0; 32], state(1)).unwrap();
    index.set_bytes([255; 32], state(2)).unwrap();
    let root = index.root.unwrap();
    let mut bytes = [0; BRANCH_BYTES];
    index
        .file
        .seek(SeekFrom::Start(root.locator.offset))
        .unwrap();
    index.file.read_exact(&mut bytes).unwrap();
    for byte in 0..bytes.len() {
        for bit in 0..8 {
            bytes[byte] ^= 1 << bit;
            if let Ok(changed) = digest(&bytes) {
                assert_ne!(changed, root.locator.digest, "byte {byte}, bit {bit}");
            }
            bytes[byte] ^= 1 << bit;
        }
    }
}

#[test]
fn canonical_child_slots_cannot_authorize_absence_or_updates_after_tampering() {
    for at_root in [true, false] {
        for fault in 0..5 {
            for operation in 0..5 {
                let mut index = Index::new().unwrap();
                index.set_bytes([0; 32], state(1)).unwrap();
                index.set_bytes(digit_key(2, FANOUT - 1), state(2)).unwrap();
                let target = index.root.unwrap().locator;
                if !at_root {
                    index.set_bytes([0xf0; 32], state(3)).unwrap();
                }
                assert_eq!(index.get_bytes([0; 32]).unwrap(), Some(state(1)));
                let root = index.root.unwrap();
                let end = index.end;
                let mut bytes = [0; BRANCH_BYTES];
                index.file.seek(SeekFrom::Start(target.offset)).unwrap();
                index.file.read_exact(&mut bytes).unwrap();
                match fault {
                    0 => bytes[34 + 40 + 8] = 1, // Noncanonical empty digest.
                    1 => bytes[34..42].copy_from_slice(&u64::MAX.to_le_bytes()),
                    2 => {
                        bytes[34..74].fill(0);
                        bytes[34..42].copy_from_slice(&u64::MAX.to_le_bytes());
                    }
                    3 => {
                        // Preserve the ordered list of occupied locators, moving
                        // child 0 to child 1. Only its authenticated position differs.
                        bytes.copy_within(34..74, 74);
                        bytes[34..74].fill(0);
                        bytes[34..42].copy_from_slice(&u64::MAX.to_le_bytes());
                    }
                    _ => bytes.copy_within(34..74, 74), // Empty becomes occupied.
                }
                index.file.seek(SeekFrom::Start(target.offset)).unwrap();
                index.file.write_all(&bytes).unwrap();
                let missing = digit_key(2, 1);
                // The first digit still reaches the inner branch; the second
                // mismatches its compressed prefix before the third digit.
                let mismatch = digit_key(1, 1);
                let error = match operation {
                    0 => index.get_bytes([0; 32]).unwrap_err(),
                    1 => index.get_bytes(missing).unwrap_err(),
                    2 => index.get_bytes(mismatch).unwrap_err(), // Prefix mismatch.
                    3 => index.set_bytes([0; 32], state(3)).unwrap_err(),
                    _ => index.set_bytes(missing, state(3)).unwrap_err(),
                };
                let error = error
                    .get_ref()
                    .unwrap()
                    .downcast_ref::<StateError>()
                    .unwrap();
                if !at_root && fault <= 1 {
                    assert!(matches!(error, StateError::Child));
                } else {
                    assert!(matches!(error, StateError::Authentication));
                }
                assert_eq!(index.root, Some(root));
                assert_eq!(index.end, end);
                assert_eq!(index.file.metadata().unwrap().len(), end);
                assert_terminal(&mut index);
            }
        }
    }
}

#[test]
fn full_depth_lookup_update_and_absence_are_bounded_without_history_growth() {
    let mut index = Index::new().unwrap();
    index.set_bytes([0; 32], state(0)).unwrap();
    for depth in (0..LEVELS).rev() {
        index
            .set_bytes(digit_key(depth, 32), state(depth as u64 + 1))
            .unwrap();
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
        assert_eq!(
            index.get_bytes(digit_key(depth, 32)).unwrap(),
            Some(state(depth as u64 + 1))
        );
        assert_eq!(index.get_bytes(digit_key(depth, 16)).unwrap(), None);
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
                    .seek(SeekFrom::Start(root.unwrap().locator.offset + 34))
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
                    .seek(SeekFrom::Start(root.unwrap().locator.offset))
                    .unwrap();
                index.file.write_all(&[255]).unwrap();
            }
            _ => {
                // A fully self-consistent replacement map still cannot replace
                // the retained native root authority, even at identical offsets.
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
fn authenticated_child_edges_cannot_become_false_absence_or_update_paths() {
    for fault in 0..3 {
        for update in [false, true] {
            let mut index = Index::new().unwrap();
            index.set_bytes([0; 32], state(1)).unwrap();
            index.set_bytes(digit_key(1, FANOUT - 1), state(2)).unwrap();
            let root = index.root.unwrap();
            let mut frame = [0; BRANCH_BYTES];
            let ReadNode::Branch(branch) = index.read(root.locator, &mut frame).unwrap() else {
                panic!("two distinct keys require a branch");
            };
            let mut branch = branch.into_owned();
            assert_eq!(branch.depth, 1);
            let malformed = if fault == 2 {
                // A locally valid child branch must still advance its parent's
                // depth, even if it has matching routing bits and valid leaves.
                Node::Branch(branch)
            } else {
                let key = digit_key(if fault == 0 { 0 } else { 1 }, 1);
                // Independently violate the shared prefix or selected digit.
                Node::Leaf {
                    key,
                    state: state(3),
                }
            };
            // Re-sign a structurally inconsistent edge in this native-owned
            // fixture. Scratch attackers cannot replace the root authority.
            let mut end = index.end;
            branch.children[0] = Some(index.write(end, malformed, &mut end).unwrap().locator);
            index.root = Some(
                index
                    .write(root.locator.offset, Node::Branch(branch), &mut end)
                    .unwrap(),
            );
            index.end = end;
            let error = if update {
                index.set_bytes([0; 32], state(4)).unwrap_err()
            } else {
                index.get_bytes([0; 32]).unwrap_err()
            };
            assert!(matches!(
                error
                    .get_ref()
                    .and_then(|error| error.downcast_ref::<StateError>()),
                Some(StateError::Path)
            ));
            assert_terminal(&mut index);
        }
    }
}

#[test]
fn invalid_siblings_and_cardinality_block_lookup_absence_and_updates() {
    for fault in 0..5 {
        for operation in 0..6 {
            let mut index = Index::new().unwrap();
            index.set_bytes([0; 32], state(1)).unwrap();
            index.set_bytes(digit_key(1, FANOUT - 1), state(2)).unwrap();
            let root = index.root.unwrap();
            let mut frame = [0; BRANCH_BYTES];
            let ReadNode::Branch(branch) = index.read(root.locator, &mut frame).unwrap() else {
                panic!("two distinct keys require a branch");
            };
            let mut branch = branch.into_owned();
            assert_eq!(branch.depth, 1);
            // All operations below select child 0 or 1, or mismatch the prefix.
            // Forge native authority to isolate validation from authentication.
            match fault {
                0..=2 => {
                    branch.children[FANOUT - 1].as_mut().unwrap().offset = match fault {
                        0 => u64::MAX - 1,
                        1 => index.end - RECORD_BYTES as u64 + 1,
                        _ => root.locator.offset,
                    };
                }
                3 => branch.children[FANOUT - 1] = None,
                _ => branch.children.fill(None),
            }
            let mut end = index.end;
            index.root = Some(
                index
                    .write(root.locator.offset, Node::Branch(branch), &mut end)
                    .unwrap(),
            );
            let root = index.root;
            let error = match operation {
                0 => index.get_bytes([0; 32]).unwrap_err(),
                1 => index.get_bytes(digit_key(1, 1)).unwrap_err(),
                2 => index.get_bytes(digit_key(0, 1)).unwrap_err(),
                3 => index.set_bytes([0; 32], state(3)).unwrap_err(),
                4 => index.set_bytes(digit_key(1, 1), state(3)).unwrap_err(),
                _ => index.set_bytes(digit_key(0, 1), state(3)).unwrap_err(),
            };
            let error = error
                .get_ref()
                .unwrap()
                .downcast_ref::<StateError>()
                .unwrap();
            if fault >= 2 {
                assert!(matches!(error, StateError::Child));
            } else {
                assert!(matches!(error, StateError::Offset));
            }
            assert_eq!(index.root, root);
            assert_eq!(index.end, end);
            assert_terminal(&mut index);
        }
    }
}

#[test]
fn final_digit_padding_cannot_authorize_lookup_absence_or_updates() {
    let mut index = Index::new().unwrap();
    for position in (0..64).step_by(4) {
        index
            .set_bytes(digit_key(42, position), state(position as u64))
            .unwrap();
    }
    for position in (0..64).step_by(4) {
        let key = digit_key(42, position);
        assert_eq!(index.get_bytes(key).unwrap(), Some(state(position as u64)));
        index.set_bytes(key, state(position as u64 + 64)).unwrap();
    }
    for position in (0..64).step_by(4) {
        assert_eq!(
            index.get_bytes(digit_key(42, position)).unwrap(),
            Some(state(position as u64 + 64))
        );
    }

    for at_root in [true, false] {
        for position in (0..64).filter(|position| position % 4 != 0) {
            for operation in 0..6 {
                let mut index = Index::new().unwrap();
                index.set_bytes([0; 32], state(1)).unwrap();
                index.set_bytes(digit_key(42, 60), state(2)).unwrap();
                let target = index.root.unwrap().locator;
                if !at_root {
                    index.set_bytes(digit_key(0, 1), state(3)).unwrap();
                }
                let root = index.root.unwrap();
                let mut frame = [0; BRANCH_BYTES];
                let parent = if at_root {
                    None
                } else {
                    let ReadNode::Branch(branch) = index.read(root.locator, &mut frame).unwrap()
                    else {
                        panic!("outer routing branch");
                    };
                    Some(branch.into_owned())
                };
                let ReadNode::Branch(branch) = index.read(target, &mut frame).unwrap() else {
                    panic!("final-digit branch");
                };
                let mut branch = branch.into_owned();
                assert_eq!(branch.depth, 42);
                // A valid locator in an impossible, unselected final slot.
                // Native signing isolates canonicality from authentication.
                branch.children[position] = branch.children[60];
                let mut end = index.end;
                let malformed = index
                    .write(target.offset, Node::Branch(branch), &mut end)
                    .unwrap();
                index.root = Some(if let Some(mut parent) = parent {
                    parent.children[0] = Some(malformed.locator);
                    index
                        .write(root.locator.offset, Node::Branch(parent), &mut end)
                        .unwrap()
                } else {
                    malformed
                });
                let root = index.root;
                let missing = digit_key(42, 4);
                let mismatch = digit_key(1, 1);
                let error = match operation {
                    0 => index.get_bytes([0; 32]).unwrap_err(),
                    1 => index.get_bytes(missing).unwrap_err(),
                    2 => index.get_bytes(mismatch).unwrap_err(),
                    3 => index.set_bytes([0; 32], state(4)).unwrap_err(),
                    4 => index.set_bytes(missing, state(4)).unwrap_err(),
                    _ => index.set_bytes(mismatch, state(4)).unwrap_err(),
                };
                assert!(matches!(
                    error.get_ref().and_then(|e| e.downcast_ref::<StateError>()),
                    Some(StateError::Child)
                ));
                assert_eq!(index.root, root);
                assert_eq!(index.end, end);
                assert_terminal(&mut index);
            }
        }
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

#[cfg(unix)]
#[test]
fn partial_ancestor_write_retains_complete_root_and_poisons_owner() {
    const CHILD: &str = "SY_TEST_PARTIAL_INDEX_WRITE";
    if std::env::var_os(CHILD).is_none() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "authenticated_index::tests::partial_ancestor_write_retains_complete_root_and_poisons_owner",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        return;
    }

    let mut index = Index::new().unwrap();
    index.set_bytes([0; 32], state(1)).unwrap();
    index.set_bytes([255; 32], state(2)).unwrap();
    let root = index.root.unwrap();
    let end = index.end;
    assert_eq!(root.locator.offset, 226);
    assert_eq!(end, 226 + BRANCH_BYTES as u64);
    // Permit the changed first child's digest, but not the rest of its ancestor.
    // write_all must encounter a short write before its subsequent EFBIG.
    let boundary = root.locator.offset + 74;
    let limit = libc::rlimit {
        rlim_cur: boundary,
        rlim_max: boundary,
    };
    // SAFETY: SIG_IGN is a valid disposition and installs no Rust callback.
    // Only this isolated exact-test child changes process signals and limits.
    assert_ne!(
        unsafe { libc::signal(libc::SIGXFSZ, libc::SIG_IGN) },
        libc::SIG_ERR
    );
    // SAFETY: limit is initialized and remains live for this synchronous call.
    assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_FSIZE, &limit) }, 0);

    let error = index.set_bytes([0; 32], state(3)).unwrap_err();
    assert_eq!(error.raw_os_error(), Some(libc::EFBIG));
    let mut actual = [0; 80];
    index.file.read_exact_at(&mut actual, 33).unwrap();
    assert_eq!(actual, state(3)); // The leaf write really preceded ancestor failure.
    let mut leaf = [0; RECORD_BYTES];
    leaf[0] = 1;
    leaf[33..].copy_from_slice(&state(3));
    let mut expected = root.bytes;
    expected[42..74].copy_from_slice(&digest(&leaf).unwrap());
    assert_ne!(expected, root.bytes);
    let mut ancestor = [0; BRANCH_BYTES];
    index
        .file
        .read_exact_at(&mut ancestor, root.locator.offset)
        .unwrap();
    assert_eq!(ancestor, expected); // Changed prefix plus the untouched old suffix.
    assert_eq!(index.root, Some(root));
    assert_eq!(index.end, end);
    assert_eq!(index.file.metadata().unwrap().len(), end);
    assert_terminal(&mut index);
}

#[test]
fn root_images_are_fresh_for_leaf_payloads_and_valid_tag_substitutions() {
    for fault in 0..4 {
        for update in [false, true] {
            let mut index = Index::new().unwrap();
            index.set_bytes([0; 32], state(1)).unwrap();
            if fault == 0 {
                index.set_bytes([255; 32], state(2)).unwrap();
            }
            assert_eq!(index.get_bytes([0; 32]).unwrap(), Some(state(1)));
            let root = index.root.unwrap();
            let (offset, byte) = match fault {
                0 => (0, 1),  // Branch becomes leaf; comparison must include tag.
                1 => (0, 0),  // Leaf becomes branch, exceeding native extent.
                2 => (1, 1),  // Root leaf key changes.
                _ => (33, 7), // Root leaf payload changes.
            };
            index
                .file
                .seek(SeekFrom::Start(root.locator.offset + offset))
                .unwrap();
            index.file.write_all(&[byte]).unwrap();
            let error = if update {
                index.set_bytes([0; 32], state(3)).unwrap_err()
            } else {
                index.get_bytes([0; 32]).unwrap_err()
            };
            let error = error
                .get_ref()
                .unwrap()
                .downcast_ref::<StateError>()
                .unwrap();
            if fault == 1 {
                assert!(matches!(error, StateError::Offset));
            } else {
                assert!(matches!(error, StateError::Authentication));
            }
            assert_eq!(index.root, Some(root));
            assert_terminal(&mut index);
        }
    }
}

#[test]
fn packed_leaf_reads_do_not_authenticate_untraversed_neighbor_bytes() {
    let mut index = Index::new().unwrap();
    index.set_bytes([0; 32], state(1)).unwrap();
    index.set_bytes([255; 32], state(2)).unwrap();
    index
        .file
        .seek(SeekFrom::Start(RECORD_BYTES as u64 + 33))
        .unwrap();
    index.file.write_all(&state(3)).unwrap();
    assert_eq!(index.get_bytes([0; 32]).unwrap(), Some(state(1)));
    let error = index.get_bytes([255; 32]).unwrap_err();
    assert!(matches!(
        error.get_ref().and_then(|e| e.downcast_ref::<StateError>()),
        Some(StateError::Authentication)
    ));
    assert_terminal(&mut index);
}

#[test]
fn malformed_native_extents_and_authenticated_node_structure_are_rejected() {
    let mut index = Index::new().unwrap();
    index.set_bytes([0; 32], state(1)).unwrap();
    index.root.as_mut().unwrap().locator.offset = u64::MAX - 1;
    assert!(index.get_bytes([0; 32]).is_err());
    assert_terminal(&mut index);
    for fault in 0..6 {
        let mut index = Index::new().unwrap();
        index.set_bytes([0; 32], state(1)).unwrap();
        index.set_bytes([255; 32], state(2)).unwrap();
        let root = index.root.unwrap();
        let mut bytes = [0; BRANCH_BYTES];
        index
            .file
            .seek(SeekFrom::Start(root.locator.offset))
            .unwrap();
        index.file.read_exact(&mut bytes).unwrap();
        match fault {
            0 => bytes[1] = LEVELS as u8,
            1 => bytes[34..42].copy_from_slice(&u64::MAX.wrapping_sub(1).to_le_bytes()),
            2 => bytes[34..42].copy_from_slice(&root.locator.offset.to_le_bytes()),
            3 => bytes[2] = 1, // Noncanonical depth-zero prefix.
            _ => {
                // Canonical empty encodings still cannot turn a branch into
                // zero or one children, even with a native-owned digest.
                let first = if fault == 4 { 0 } else { 1 };
                for slot in bytes[34 + first * 40..].as_chunks_mut::<40>().0 {
                    slot.fill(0);
                    slot[..8].copy_from_slice(&u64::MAX.to_le_bytes());
                }
            }
        }
        index
            .file
            .seek(SeekFrom::Start(root.locator.offset))
            .unwrap();
        index.file.write_all(&bytes).unwrap();
        // Exercise structural checks independently of authentication. Scratch
        // attackers cannot rewrite this native-owned RAM authority.
        let root = index.root.as_mut().unwrap();
        root.locator.digest = digest(&bytes).unwrap();
        root.bytes = bytes;
        assert!(index.get_bytes([0; 32]).is_err());
        assert_terminal(&mut index);
    }
}
