use super::*;
use futures::TryStreamExt;

#[tokio::test]
async fn candidate_substitution_never_becomes_original_authority() {
    for named in [false, true] {
        for fault in 0..6 {
            let fixture = tempfile::tempdir().unwrap();
            for name in ["a", "b"] {
                std::fs::write(fixture.path().join(name), b"SOURCE").unwrap();
            }
            let rooted = RootedFs::open(fixture.path().to_path_buf()).await.unwrap();
            rooted
                .source_entry_stream(Default::default())
                .try_collect::<Vec<_>>()
                .await
                .unwrap();
            let inventory = rooted.seal_source_inventory_blocking().unwrap();
            let (ordinal, key) = {
                let mut owner = rooted.source_observations.lock().unwrap();
                let c = owner.active().unwrap();
                let ordinal = u64::from(fault != 5);
                let (key, original, _) = c.original_at(ordinal).unwrap();
                let replacement = match fault {
                    0 => c.original_hint(2).unwrap(), // Duplicate another original.
                    1 => [0; 32],                     // Substitute ROOT for a child.
                    2 => [71; 32],                    // No canonical entry at all.
                    3 => {
                        c.append([72; 32], &original, None).unwrap();
                        [72; 32] // Authenticated derived entry, not an original.
                    }
                    4 => {
                        c.set_index_record([73; 32], IndexRecord::Epoch(0)).unwrap();
                        [73; 32] // Authenticated record in a different namespace.
                    }
                    _ => c.original_hint(1).unwrap(), // Substitute a child for ROOT.
                };
                c.original_hints
                    .write_all_at(&replacement, ordinal * 32)
                    .unwrap();
                (ordinal, key)
            };
            let result = inventory.with_original_batch(|c| {
                if named {
                    c.original_named(key).map(|_| ())
                } else {
                    c.original_at(ordinal).map(|_| ())
                }
            });
            assert!(
                matches!(result, Err(RootedFsError::Io(ref error)) if error.kind() == io::ErrorKind::InvalidData),
                "named={named}, fault={fault}: {result:?}"
            );
            {
                let mut owner = rooted.source_observations.lock().unwrap();
                let CatalogueState::Active(c) = &mut *owner else {
                    panic!("retained owner")
                };
                c.original_hints.write_all_at(&key, ordinal * 32).unwrap();
            }
            assert!(matches!(
                inventory.validate_lifetime_blocking(),
                Err(RootedFsError::SourceCatalogueTerminal)
            ));
        }
    }
}

#[tokio::test]
async fn hint_extents_are_checked_at_entry_and_batch_closure() {
    for late in [false, true] {
        for length in [33, 64, 0, 31] {
            let fixture = tempfile::tempdir().unwrap();
            let rooted = RootedFs::open(fixture.path().to_path_buf()).await.unwrap();
            rooted
                .source_entry_stream(Default::default())
                .try_collect::<Vec<_>>()
                .await
                .unwrap();
            let inventory = rooted.seal_source_inventory_blocking().unwrap();
            let tape = {
                let mut owner = rooted.source_observations.lock().unwrap();
                owner.active().unwrap().original_hints.try_clone().unwrap()
            };
            let original = {
                let mut bytes = [0; 32];
                tape.read_exact_at(&mut bytes, 0).unwrap();
                bytes
            };
            if !late {
                tape.set_len(length).unwrap();
            }
            let result = inventory.with_original_batch(|_| {
                if late {
                    tape.set_len(length)?;
                }
                Ok(())
            });
            assert!(
                matches!(result, Err(RootedFsError::Io(ref error)) if error.kind() == io::ErrorKind::InvalidData),
                "late={late}, length={length}: {result:?}"
            );
            tape.set_len(32).unwrap();
            tape.write_all_at(&original, 0).unwrap();
            assert!(matches!(
                inventory.validate_lifetime_blocking(),
                Err(RootedFsError::SourceCatalogueTerminal)
            ));
        }
    }
}

#[test]
fn ordinal_bounds_and_state_updates_do_not_rewrite_candidates() {
    let fixture = tempfile::tempdir().unwrap();
    let root = File::open(fixture.path()).unwrap();
    let mut c = Catalogue::new(root.as_raw_fd()).unwrap();
    let original = c.original_hint(0).unwrap();
    let mut state = c.original_state(original).unwrap();
    state.children = 17;
    state.directory = SourceDirectoryDisposition::Protected;
    c.set_original_state(original, state).unwrap();
    assert_eq!(c.original_hint(0).unwrap(), original);
    state.ordinal = 1;
    assert!(c.set_original_state(original, state).is_err());
    assert_eq!(c.original_state(original).unwrap().ordinal, 0);
    assert!(c.original_hint(1).is_err());
    assert!(c.original_hint(u64::MAX).is_err());
    c.count = u64::MAX / 32 + 1;
    assert!(c.check_original_hints().is_err());
    let old_end = c.end;
    let (_, observation, _) = {
        c.count = 1;
        c.original_at(0).unwrap()
    };
    c.count = u64::MAX / 32;
    assert!(c.insert([1; 32], &observation).is_err());
    assert_eq!(c.end, old_end);
    assert_eq!(c.original_hints.metadata().unwrap().len(), 32);
}

#[test]
fn partial_candidate_append_cannot_publish_count_or_revive_owner() {
    const CHILD: &str = "SY_TEST_PARTIAL_SOURCE_HINT";
    if std::env::var_os(CHILD).is_none() {
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "rooted_fs::source_observations::inventory::hints_tests::partial_candidate_append_cannot_publish_count_or_revive_owner", "--test-threads=1"])
            .env(CHILD, "1").status().unwrap();
        assert!(status.success());
        return;
    }
    let fixture = tempfile::tempdir().unwrap();
    let root = File::open(fixture.path()).unwrap();
    let mut c = Catalogue::new(root.as_raw_fd()).unwrap();
    let (_, mut observation, _) = c.original_at(0).unwrap();
    observation.name = OsString::from("new-original");
    let candidate = key([0; 32], &observation.name);
    let old_end = c.end;
    c.begin().unwrap();
    let mut previous = std::mem::MaybeUninit::<libc::rlimit>::uninit();
    // SAFETY: writable rlimit output and valid resource; this isolated child
    // owns the signal disposition and file-size limit for its entire lifetime.
    assert_eq!(
        unsafe { libc::getrlimit(libc::RLIMIT_FSIZE, previous.as_mut_ptr()) },
        0
    );
    // SAFETY: getrlimit successfully initialized the complete value above.
    let previous = unsafe { previous.assume_init() };
    let limit = libc::rlimit {
        rlim_cur: 48,
        rlim_max: previous.rlim_max,
    };
    // SAFETY: SIG_IGN is a valid disposition; errors remain visible as EFBIG.
    assert_ne!(
        unsafe { libc::signal(libc::SIGXFSZ, libc::SIG_IGN) },
        libc::SIG_ERR
    );
    // SAFETY: valid resource and live initialized limit pointer in this child.
    assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_FSIZE, &limit) }, 0);
    let result = c.insert(candidate, &observation);
    // SAFETY: restore the saved valid limit before further writes/assertions.
    assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_FSIZE, &previous) }, 0);
    assert!(
        matches!(result, Err(RootedFsError::Io(ref error)) if error.raw_os_error() == Some(libc::EFBIG)),
        "{result:?}"
    );
    assert_eq!(c.count, 1);
    assert_eq!(c.end, old_end);
    assert_eq!(c.original_hints.metadata().unwrap().len(), 48);
    let mut partial = [0; 16];
    c.original_hints.read_exact_at(&mut partial, 32).unwrap();
    assert_eq!(partial, candidate[..16]);
    assert!(c.entry(candidate).unwrap().is_none());
    c.original_hints.set_len(32).unwrap();
    assert!(matches!(
        c.begin(),
        Err(RootedFsError::SourceCatalogueTerminal)
    ));
}
