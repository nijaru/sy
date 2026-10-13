//! Native unlink integration; radix fault/structure tests live with the shared primitive.
use super::*;
use crate::endpoint::source_root::SourceRoot;
use crate::engine::domain::{Entry, RelativePath, Timestamp};
use crate::engine::source_removals::{NativeState, RemovalCause};
use crate::rooted_fs::RootedFs;
use std::io::{Seek, SeekFrom};

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
#[tokio::test]
async fn self_consistent_payload_and_index_rewrite_cannot_authorize_foreign_source_alias() {
    use futures::TryStreamExt;
    for replace in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a"), b"same").unwrap();
        std::fs::hard_link(dir.path().join("a"), dir.path().join("b")).unwrap();
        let root = SourceRoot::open(dir.path().into()).await.unwrap();
        root.entries(Default::default())
            .try_collect::<Vec<_>>()
            .await
            .unwrap();
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
        let mut other = key;
        other[0] ^= 0xf0;
        native.states.set(other, state(99)).unwrap();
        if replace {
            std::fs::write(dir.path().join("replacement"), b"foreign bytes").unwrap();
            std::fs::rename(dir.path().join("replacement"), dir.path().join("b")).unwrap();
        } else {
            std::fs::write(dir.path().join("b"), b"foreign bytes").unwrap();
        }
        // Obtain a real foreign continuation through a separately owned probe,
        // not by recapturing originals in the already-running SOURCE owner.
        std::fs::hard_link(dir.path().join("b"), dir.path().join("probe")).unwrap();
        let probe_root = RootedFs::open_blocking_for_worker(dir.path().into()).unwrap();
        let probe = observed(&root, "probe");
        let foreign = probe_root
            .remove_hardlink_source_member_blocking(&probe.path, probe.identity.unwrap(), None)
            .unwrap();
        let mut forged = StateIndex::new().unwrap();
        forged.set(key, foreign).unwrap();
        forged.set(other, state(99)).unwrap();
        assert_eq!(forged.get(key).unwrap(), Some(foreign));
        assert_eq!(
            forged.file.metadata().unwrap().len(),
            native.states.file.metadata().unwrap().len()
        );
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
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert_eq!(
            std::fs::read(dir.path().join("b")).unwrap(),
            b"foreign bytes"
        );
    }
}
#[tokio::test]
async fn native_unlink_updates_keep_original_authority_across_multiple_groups() {
    use futures::TryStreamExt;
    let dir = tempfile::tempdir().unwrap();
    for names in [["a", "b"], ["c", "d"]] {
        std::fs::write(dir.path().join(names[0]), b"same").unwrap();
        std::fs::hard_link(dir.path().join(names[0]), dir.path().join(names[1])).unwrap();
    }
    let root = SourceRoot::open(dir.path().into()).await.unwrap();
    root.entries(Default::default())
        .try_collect::<Vec<_>>()
        .await
        .unwrap();
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
