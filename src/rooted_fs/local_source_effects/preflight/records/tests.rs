use super::*;
use std::os::unix::fs::FileExt;

fn object(domain: u8, id: u64) -> native_object::NativeObjectKey {
    let mut bytes = [domain; 32];
    bytes[..4].copy_from_slice(b"apfs");
    bytes[24..].copy_from_slice(&id.to_le_bytes());
    native_object::NativeObjectKey(bytes)
}

fn record(candidates: Vec<native_object::NativeObjectKey>) -> Record {
    Record {
        root: EffectRoot::Main,
        path: RelativePath::new("target").unwrap(),
        kind: SourceEffectKind::LeafPublication,
        snapshot: Snapshot {
            candidates,
            additional: SourceExclusion::Objects,
            scope: [7; 32],
        },
    }
}

#[test]
fn indexed_candidates_match_native_domain_independence() {
    for candidates in [
        vec![object(1, 1)],
        vec![object(1, 1), object(1, 2), object(1, 1)],
        vec![object(1, 1), object(2, 2)],
    ] {
        let mut declarations = Declarations::new().unwrap();
        for candidate in &candidates {
            declarations.append(record(vec![*candidate])).unwrap();
        }
        for source in [
            object(1, 1),
            object(1, 2),
            object(1, 3),
            object(2, 1),
            object(2, 2),
            object(2, 3),
        ] {
            assert_eq!(
                declarations.independent(source).unwrap(),
                candidates
                    .iter()
                    .all(|candidate| source.independent(*candidate))
            );
        }
        for ordinal in 0..declarations.count {
            declarations.read(ordinal).unwrap();
        }
    }
}

#[test]
fn scratch_corruption_and_restoration_never_revive_declarations() {
    for fault in ["byte", "truncate", "append", "index"] {
        let mut declarations = Declarations::new().unwrap();
        declarations.append(record(vec![object(1, 1)])).unwrap();
        let mut original = vec![0; declarations.end as usize];
        declarations.tape.read_exact_at(&mut original, 0).unwrap();
        let index_end = declarations.index.file.metadata().unwrap().len();
        match fault {
            "byte" => declarations
                .tape
                .write_all_at(&[original[0] ^ 1], 0)
                .unwrap(),
            "truncate" => declarations.tape.set_len(declarations.end - 1).unwrap(),
            "append" => declarations.tape.set_len(declarations.end + 1).unwrap(),
            "index" => declarations.index.file.set_len(index_end + 1).unwrap(),
            _ => unreachable!(),
        }
        assert!(declarations.read(0).is_err(), "{fault}");
        declarations.tape.write_all_at(&original, 0).unwrap();
        declarations.tape.set_len(declarations.end).unwrap();
        declarations.index.file.set_len(index_end).unwrap();
        assert!(declarations.read(0).is_err(), "{fault} restored");
        assert!(
            declarations.independent(object(1, 2)).is_err(),
            "{fault} absent"
        );
        assert!(
            declarations.append(record(vec![object(1, 2)])).is_err(),
            "{fault} append"
        );
    }
}

#[test]
fn signed_invalid_declaration_lengths_ordinals_and_exclusions_are_rejected() {
    for fault in [
        "offset",
        "length",
        "ordinal",
        "record",
        "kind",
        "path",
        "additional",
        "trailing",
    ] {
        let mut declarations = Declarations::new().unwrap();
        let candidate = object(1, 1);
        let entry = record(vec![candidate]);
        let mut bytes = entry.encode().unwrap();
        declarations.append(entry).unwrap();
        let key = index_key(b"record", &0u64.to_le_bytes());
        let mut locator = declarations.index.get_bytes(key).unwrap().unwrap();
        match fault {
            "offset" => locator[..8].copy_from_slice(&u64::MAX.to_le_bytes()),
            "length" => locator[8..16].copy_from_slice(&((MAX_RECORD + 1) as u64).to_le_bytes()),
            "ordinal" => {
                let mut state = [0; 48];
                state[..8].copy_from_slice(&1u64.to_le_bytes());
                declarations
                    .index
                    .set_bytes(index_key(b"candidate", &candidate.0), state)
                    .unwrap();
            }
            "record" => {
                declarations.append(record(vec![candidate])).unwrap();
                locator = declarations
                    .index
                    .get_bytes(index_key(b"record", &1u64.to_le_bytes()))
                    .unwrap()
                    .unwrap();
            }
            "kind" => bytes[1] = 7,
            "path" => bytes[2..6].copy_from_slice(&u32::MAX.to_le_bytes()),
            "additional" => {
                let offset = bytes.len() - 33;
                bytes[offset] = 1;
                bytes.splice(offset + 1..offset + 1, object(1, 2).0);
            }
            "trailing" => bytes.push(0),
            _ => unreachable!(),
        }
        if ["kind", "path", "additional", "trailing"].contains(&fault) {
            // Retain authentication while violating the semantic format.
            declarations.tape.write_all_at(&bytes, 0).unwrap();
            declarations.end = bytes.len() as u64;
            locator[8..16].copy_from_slice(&declarations.end.to_le_bytes());
            locator[16..].copy_from_slice(&record_digest(0, &bytes));
        }
        declarations.index.set_bytes(key, locator).unwrap();
        assert!(declarations.read(0).is_err(), "{fault}");
        assert!(declarations.read(0).is_err(), "{fault} sticky");
    }
}
