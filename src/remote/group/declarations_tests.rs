use super::*;
use crate::protocol::{WireEntryKind, WireGroupMember, WireGroupSeal};
use std::io::{Seek, SeekFrom, Write};

fn member(ordinal: u64, name: &str) -> WireGroupMember {
    WireGroupMember {
        ordinal,
        path: crate::remote::path::encode_relative_path(std::path::Path::new(name)).unwrap(),
        observation: Some((WireEntryKind::File, [9; 32])),
    }
}
fn seal(members: &[WireGroupMember]) -> WireGroupSeal {
    let mut digest = blake3::Hasher::new();
    for member in members {
        add_to_seal(&mut digest, member).unwrap();
    }
    WireGroupSeal {
        count: members.len() as u64,
        digest: *digest.finalize().as_bytes(),
    }
}

#[test]
fn declarations_are_ordered_exact_and_repeatably_replayed() {
    let members = [member(0, "a"), member(1, "later/b")];
    let mut declarations = Declarations::new().unwrap();
    for member in &members {
        declarations.append(member).unwrap();
    }
    declarations.seal(seal(&members)).unwrap();
    for _ in 0..2 {
        let mut cursor = declarations.cursor().unwrap();
        for member in &members {
            assert_eq!(
                declarations.next(&mut cursor).unwrap().as_ref(),
                Some(member)
            );
        }
        assert!(declarations.next(&mut cursor).unwrap().is_none());
    }
    assert!(declarations.append(&member(2, "c")).is_err());
}

#[test]
fn bad_ordinals_duplicate_paths_and_wrong_seals_poison_declarations() {
    for bad in [member(0, "a"), member(2, "b")] {
        let mut declarations = Declarations::new().unwrap();
        declarations.append(&member(0, "a")).unwrap();
        assert!(declarations.append(&bad).is_err());
        assert!(declarations.seal(seal(&[member(0, "a")])).is_err());
    }
    let mut declarations = Declarations::new().unwrap();
    declarations.append(&member(0, "a")).unwrap();
    assert!(declarations.seal(seal(&[member(0, "b")])).is_err());
    assert!(declarations.cursor().is_err());
}

#[test]
fn changed_records_and_extents_cannot_become_completion() {
    // The journal's data-integrity envelope protects against substituted paths,
    // not just count changes. Tests live beside the owner to corrupt its file.
    let members = [member(0, "a")];
    let mut declarations = Declarations::new().unwrap();
    declarations.append(&members[0]).unwrap();
    declarations.seal(seal(&members)).unwrap();
    let mut cursor = declarations.cursor().unwrap();
    let file = &mut declarations.file;
    file.seek(SeekFrom::Start(4)).unwrap();
    file.write_all(&[255]).unwrap();
    assert!(declarations.next(&mut cursor).is_err());
    assert!(declarations.cursor().is_err());
    let mut declarations = Declarations::new().unwrap();
    declarations.append(&members[0]).unwrap();
    declarations.seal(seal(&members)).unwrap();
    declarations.file.set_len(0).unwrap();
    assert!(declarations.cursor().is_err());
}
