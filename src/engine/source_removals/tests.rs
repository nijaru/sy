use super::*;
use crate::endpoint::io::VerificationStatus;
use crate::rooted_fs::RootedFs;

fn rel(path: &str) -> RelativePath {
    RelativePath::new(path).unwrap()
}

fn observed(root: &SourceRoot, name: &str) -> Entry {
    let path = rel(name);
    let (kind, identity) = root
        .rooted()
        .path_identity_blocking(&path)
        .unwrap()
        .unwrap();
    let metadata = std::fs::symlink_metadata(root.path().join(name)).unwrap();
    let mut entry = Entry::file(path, metadata.len(), Timestamp::UNIX_EPOCH);
    entry.kind = kind;
    entry.identity = Some(identity);
    if kind == EntryKind::Symlink {
        entry.symlink_target = Some(std::fs::read_link(root.path().join(name)).unwrap());
    }
    entry
}

fn published(dest: &RootedFs, source: &Entry, name: &str) -> PublishedDestinationReceipt {
    let path = rel(name);
    if source.kind == EntryKind::Symlink {
        let proof = dest
            .publish_symlink_blocking(
                &path,
                source.symlink_target.as_ref().unwrap(),
                crate::endpoint::ExpectedDestination::Absent,
                None,
            )
            .unwrap();
        return PublishedDestinationReceipt::for_symlink(
            source.path.clone(),
            path,
            source.identity,
            proof,
        );
    }
    let mut stage = dest
        .begin_staged_file_with_expectation_blocking(
            &path,
            crate::endpoint::ExpectedDestination::Absent,
        )
        .unwrap();
    stage.file_mut().write_all(b"same").unwrap();
    let proof = stage.commit().unwrap().finalize_blocking(None).unwrap();
    PublishedDestinationReceipt::for_file(
        source.path.clone(),
        path,
        source.identity,
        &VerificationStatus::Verified,
        proof,
    )
}

async fn existing_receipt(
    source_root: &SourceRoot,
    source: &Entry,
    dest: &RootedFs,
    name: &str,
) -> VerifiedExistingDestinationReceipt {
    let path = rel(name);
    let (kind, identity) = dest.path_identity_blocking(&path).unwrap().unwrap();
    let mut destination = Entry::file(path, 4, Timestamp::UNIX_EPOCH);
    destination.kind = kind;
    destination.identity = Some(identity);
    let source_fingerprint =
        existing::fingerprint(source_root.rooted(), source.clone(), Default::default())
            .await
            .unwrap();
    let dest_fingerprint =
        existing::fingerprint(dest.clone(), destination.clone(), Default::default())
            .await
            .unwrap();
    existing::receipt(source, &destination, source_fingerprint, dest_fingerprint).unwrap()
}

fn validate(proof: SourceDestinationProof, dest: &RootedFs) -> io::Result<()> {
    match proof {
        SourceDestinationProof::Published(proof) => proof.revalidate_blocking(dest),
        SourceDestinationProof::Existing { path, identity, .. } => {
            dest.verify_retired_destination_blocking(&path, EntryKind::File, identity)
        }
    }
    .map_err(io::Error::other)
}

#[tokio::test]
async fn a_destination_changed_during_another_candidate_gate_cannot_lose_its_source() {
    let source = tempfile::tempdir().unwrap();
    let destination = tempfile::tempdir().unwrap();
    for name in ["a", "b"] {
        std::fs::write(source.path().join(name), b"same").unwrap();
    }
    let root = SourceRoot::open(source.path().into()).await.unwrap();
    let dest = RootedFs::open(destination.path().into()).await.unwrap();
    let journal = SourceRemovalJournal::default();
    for name in ["a", "b"] {
        let original = observed(&root, name);
        let receipt = published(&dest, &original, name);
        journal.append_published(&original, &receipt).await.unwrap();
    }
    let mut changed = false;
    let error = journal
        .replay(root, |proof| {
            if !changed
                && matches!(&proof, SourceDestinationProof::Published(proof)
            if proof.path == rel("b"))
            {
                std::fs::write(destination.path().join("foreign"), b"race").unwrap();
                std::fs::rename(
                    destination.path().join("foreign"),
                    destination.path().join("a"),
                )
                .unwrap();
                changed = true;
            }
            std::future::ready(validate(proof, &dest))
        })
        .await
        .unwrap_err();
    assert_eq!(error.removed, 0);
    assert!(!error.removal_uncertain);
    for name in ["a", "b"] {
        assert_eq!(std::fs::read(source.path().join(name)).unwrap(), b"same");
    }
    assert_eq!(
        std::fs::read(destination.path().join("a")).unwrap(),
        b"race"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn existing_late_gates_reject_foreign_replacement_before_and_after_own_unlink() {
    for after_own_unlink in [false, true] {
        let source = tempfile::tempdir().unwrap();
        let destination = tempfile::tempdir().unwrap();
        std::fs::write(source.path().join("a"), b"same").unwrap();
        if after_own_unlink {
            std::fs::hard_link(source.path().join("a"), source.path().join("b")).unwrap();
        } else {
            std::fs::write(source.path().join("b"), b"same").unwrap();
        }
        for name in ["a", "b"] {
            if after_own_unlink {
                std::fs::hard_link(source.path().join(name), destination.path().join(name))
                    .unwrap();
            } else {
                std::fs::write(destination.path().join(name), b"same").unwrap();
            }
        }
        let root = SourceRoot::open(source.path().into()).await.unwrap();
        let source_rooted = root.rooted();
        let dest = RootedFs::open(destination.path().into()).await.unwrap();
        let journal = SourceRemovalJournal::default();
        for name in ["a", "b"] {
            let entry = observed(&root, name);
            let receipt = existing_receipt(&root, &entry, &dest, name).await;
            journal.append_existing(&entry, &receipt).await.unwrap();
        }
        let mut changed = false;
        let error = journal
            .replay(root, |proof| {
                let SourceDestinationProof::Existing {
                    path,
                    own_source_effect,
                    ..
                } = &proof
                else {
                    panic!("expected verified-existing authority")
                };
                if !changed && path == &rel("b") && own_source_effect.is_some() == after_own_unlink
                {
                    let target = if after_own_unlink { "b" } else { "a" };
                    std::fs::write(destination.path().join("foreign"), b"race").unwrap();
                    std::fs::rename(
                        destination.path().join("foreign"),
                        destination.path().join(target),
                    )
                    .unwrap();
                    changed = true;
                }
                std::future::ready(proof.revalidate_local_blocking(&source_rooted, &dest))
            })
            .await
            .unwrap_err();
        assert!(changed);
        assert_eq!(error.removed, u64::from(after_own_unlink));
        assert!(!error.removal_uncertain);
        let RemovalCause::Io(cause) = error.cause else {
            panic!("lost late destination error")
        };
        assert!(matches!(
            cause
                .get_ref()
                .and_then(|e| e.downcast_ref::<RootedFsError>()),
            Some(RootedFsError::DestinationChanged(_))
        ));
        assert_eq!(std::fs::read(source.path().join("b")).unwrap(), b"same");
        if !after_own_unlink {
            assert_eq!(std::fs::read(source.path().join("a")).unwrap(), b"same");
        }
    }
}

#[tokio::test]
async fn published_and_existing_authorities_keep_exact_independent_addresses() {
    let source = tempfile::tempdir().unwrap();
    let destination = tempfile::tempdir().unwrap();
    for name in ["published-input", "existing-input"] {
        std::fs::write(source.path().join(name), b"same").unwrap();
    }
    std::fs::write(destination.path().join("existing-output"), b"same").unwrap();
    let root = SourceRoot::open(source.path().into()).await.unwrap();
    let dest = RootedFs::open(destination.path().into()).await.unwrap();
    let published_source = observed(&root, "published-input");
    let existing_source = observed(&root, "existing-input");
    let publication = published(&dest, &published_source, "published-output");
    let parity = existing_receipt(&root, &existing_source, &dest, "existing-output").await;
    let journal = SourceRemovalJournal::default();
    journal
        .append_published(&published_source, &publication)
        .await
        .unwrap();
    journal
        .append_existing(&existing_source, &parity)
        .await
        .unwrap();
    let mut proofs = Vec::new();
    assert_eq!(
        journal
            .replay(root, |proof| {
                proofs.push(proof.clone());
                std::future::ready(validate(proof, &dest))
            })
            .await
            .unwrap(),
        2
    );
    let expected = [
        SourceDestinationProof::Published(publication.publication().unwrap().clone()),
        SourceDestinationProof::Existing {
            source_path: rel("existing-input"),
            path: rel("existing-output"),
            identity: parity.destination_identity(),
            own_source_effect: None,
        },
    ];
    assert!(proofs.iter().all(|proof| expected.contains(proof)));
    assert!(expected.iter().all(|proof| proofs.contains(proof)));
    for name in ["published-input", "existing-input"] {
        assert!(!source.path().join(name).exists());
    }
    for name in ["published-output", "existing-output"] {
        assert_eq!(
            std::fs::read(destination.path().join(name)).unwrap(),
            b"same"
        );
    }
    assert!(journal
        .replay(
            SourceRoot::open(source.path().into()).await.unwrap(),
            |_| std::future::ready(Ok(()))
        )
        .await
        .is_err());
}

#[tokio::test]
async fn late_destination_gate_failure_removes_no_sources_and_preserves_cause() {
    let source = tempfile::tempdir().unwrap();
    let destination = tempfile::tempdir().unwrap();
    let root = SourceRoot::open(source.path().into()).await.unwrap();
    let dest = RootedFs::open(destination.path().into()).await.unwrap();
    let journal = SourceRemovalJournal::default();
    for name in ["a", "b"] {
        std::fs::write(source.path().join(name), b"same").unwrap();
        let entry = observed(&root, name);
        journal
            .append_published(&entry, &published(&dest, &entry, name))
            .await
            .unwrap();
    }
    std::fs::write(destination.path().join("b"), b"changed").unwrap();
    let error = journal
        .replay(root, |proof| std::future::ready(validate(proof, &dest)))
        .await
        .unwrap_err();
    assert_eq!(error.removed, 0);
    assert!(!error.removal_uncertain);
    let RemovalCause::Io(cause) = error.cause else {
        panic!("lost destination cause")
    };
    assert!(matches!(
        cause
            .get_ref()
            .and_then(|e| e.downcast_ref::<RootedFsError>()),
        Some(RootedFsError::DestinationChanged(_))
    ));
    for name in ["a", "b"] {
        assert!(source.path().join(name).exists());
    }
    assert_eq!(journal.close().await.unwrap(), 0);
}

#[tokio::test]
async fn late_original_source_race_fails_the_complete_source_gate() {
    let source = tempfile::tempdir().unwrap();
    let destination = tempfile::tempdir().unwrap();
    let root = SourceRoot::open(source.path().into()).await.unwrap();
    let dest = RootedFs::open(destination.path().into()).await.unwrap();
    let journal = SourceRemovalJournal::default();
    for name in ["a", "b"] {
        std::fs::write(source.path().join(name), b"same").unwrap();
        let entry = observed(&root, name);
        journal
            .append_published(&entry, &published(&dest, &entry, name))
            .await
            .unwrap();
    }
    std::fs::write(source.path().join("b"), b"changed").unwrap();
    let error = journal
        .replay(root, |proof| std::future::ready(validate(proof, &dest)))
        .await
        .unwrap_err();
    assert_eq!(error.removed, 0);
    assert!(matches!(
        error.cause,
        RemovalCause::Rooted(RootedFsError::DestinationChanged(_))
    ));
    for name in ["a", "b"] {
        assert!(source.path().join(name).exists());
    }
}

#[cfg(unix)]
#[tokio::test]
async fn relocated_original_root_cannot_be_replaced_by_identical_aliases() {
    let parent = tempfile::tempdir().unwrap();
    let original = parent.path().join("source");
    let replacement = parent.path().join("replacement");
    let held = parent.path().join("held");
    for path in [&original, &replacement] {
        std::fs::create_dir(path).unwrap();
    }
    std::fs::write(original.join("a"), b"same").unwrap();
    std::fs::hard_link(original.join("a"), replacement.join("a")).unwrap();
    let root = SourceRoot::open(original.clone()).await.unwrap();
    let entry = observed(&root, "a");
    let destination = tempfile::tempdir().unwrap();
    let dest = RootedFs::open(destination.path().into()).await.unwrap();
    let journal = SourceRemovalJournal::default();
    journal
        .append_published(&entry, &published(&dest, &entry, "out"))
        .await
        .unwrap();
    std::fs::rename(&original, &held).unwrap();
    std::fs::rename(&replacement, &original).unwrap();
    let error = journal
        .replay(root, |proof| std::future::ready(validate(proof, &dest)))
        .await
        .unwrap_err();
    assert_eq!(error.removed, 0);
    assert!(
        matches!(error.cause, RemovalCause::Rooted(RootedFsError::RootChanged(path)) if path == original)
    );
    assert!(held.join("a").exists());
    assert!(original.join("a").exists());
}

#[tokio::test]
async fn receipt_path_identity_kind_and_verification_mismatches_poison_eligibility() {
    let source = tempfile::tempdir().unwrap();
    std::fs::write(source.path().join("input"), b"same").unwrap();
    let root = SourceRoot::open(source.path().into()).await.unwrap();
    let entry = observed(&root, "input");
    let destination = tempfile::tempdir().unwrap();
    std::fs::write(destination.path().join("equal"), b"same").unwrap();
    let dest = RootedFs::open(destination.path().into()).await.unwrap();
    let receipt = published(&dest, &entry, "output");
    let parity = existing_receipt(&root, &entry, &dest, "equal").await;
    for case in 0..5 {
        let journal = SourceRemovalJournal::default();
        journal.append_published(&entry, &receipt).await.unwrap();
        let mut mismatch = entry.clone();
        match case {
            0 => mismatch.path = receipt.destination_path().clone(),
            1 => mismatch.identity = Some(EntryIdentity::from_bytes([255; 32])),
            2 => mismatch.kind = EntryKind::Symlink,
            3 => mismatch.kind = EntryKind::Directory,
            4 => mismatch.identity = None,
            _ => unreachable!(),
        }
        assert!(journal.append_published(&mismatch, &receipt).await.is_err());
        assert!(journal
            .replay(root.clone(), |_| std::future::ready(Ok(())))
            .await
            .is_err());
        let journal = SourceRemovalJournal::default();
        assert!(journal.append_existing(&mismatch, &parity).await.is_err());
    }
    let failed = PublishedDestinationReceipt::for_file(
        entry.path.clone(),
        receipt.destination_path().clone(),
        entry.identity,
        &VerificationStatus::Failed {
            expected: [1; 32].into(),
            actual: [2; 32].into(),
        },
        crate::rooted_fs::PublishedFileProof {
            path: receipt.destination_path().clone(),
            identity: receipt.publication().unwrap().identity,
        },
    );
    let journal = SourceRemovalJournal::default();
    assert!(journal.append_published(&entry, &failed).await.is_err());
    assert!(source.path().join("input").exists());
}

#[cfg(unix)]
#[tokio::test]
async fn symlink_removal_does_not_follow_targets_or_remove_directories() {
    let source = tempfile::tempdir().unwrap();
    std::fs::create_dir(source.path().join("target")).unwrap();
    std::fs::write(source.path().join("target/keep"), b"keep").unwrap();
    std::os::unix::fs::symlink("target", source.path().join("link")).unwrap();
    let root = SourceRoot::open(source.path().into()).await.unwrap();
    let link = observed(&root, "link");
    let destination = tempfile::tempdir().unwrap();
    let dest = RootedFs::open(destination.path().into()).await.unwrap();
    let journal = SourceRemovalJournal::default();
    journal
        .append_published(&link, &published(&dest, &link, "renamed-link"))
        .await
        .unwrap();
    assert_eq!(
        journal
            .replay(root, |proof| std::future::ready(validate(proof, &dest)))
            .await
            .unwrap(),
        1
    );
    assert!(std::fs::symlink_metadata(source.path().join("link")).is_err());
    assert_eq!(
        std::fs::read(source.path().join("target/keep")).unwrap(),
        b"keep"
    );
    assert_eq!(
        std::fs::read_link(destination.path().join("renamed-link")).unwrap(),
        std::path::Path::new("target")
    );
}

#[tokio::test]
async fn authenticated_but_ineligible_record_and_failed_append_prevent_all_unlinks() {
    let source = tempfile::tempdir().unwrap();
    std::fs::write(source.path().join("a"), b"same").unwrap();
    let root = SourceRoot::open(source.path().into()).await.unwrap();
    let entry = observed(&root, "a");
    let destination = tempfile::tempdir().unwrap();
    let dest = RootedFs::open(destination.path().into()).await.unwrap();
    let receipt = published(&dest, &entry, "out");
    let journal = SourceRemovalJournal::default();
    journal.append_published(&entry, &receipt).await.unwrap();
    // Authenticated intent is necessary but does not make a non-removal record eligible.
    journal
        .state
        .lock()
        .await
        .writer
        .as_mut()
        .unwrap()
        .append(&SyncOp::Create {
            source: entry.clone(),
            destination_path: rel("other"),
        })
        .await
        .unwrap();
    let error = journal
        .replay(root.clone(), |_| std::future::ready(Ok(())))
        .await
        .unwrap_err();
    assert_eq!(error.removed, 0);
    assert!(matches!(error.cause, RemovalCause::Invalid(_)));
    let journal = SourceRemovalJournal::default();
    journal.append_published(&entry, &receipt).await.unwrap();
    let mut oversized = entry.clone();
    oversized.symlink_target = Some(std::path::PathBuf::from(
        "x".repeat(super::super::plan_journal::MAX_RECORD_PAYLOAD),
    ));
    assert!(journal
        .append_published(&oversized, &receipt)
        .await
        .is_err());
    assert!(journal
        .replay(root, |_| std::future::ready(Ok(())))
        .await
        .is_err());
    assert!(source.path().join("a").exists());
}

#[tokio::test]
async fn cancelled_append_and_destination_gate_are_terminal() {
    let source = tempfile::tempdir().unwrap();
    std::fs::write(source.path().join("a"), b"same").unwrap();
    let root = SourceRoot::open(source.path().into()).await.unwrap();
    let entry = observed(&root, "a");
    let destination = tempfile::tempdir().unwrap();
    let dest = RootedFs::open(destination.path().into()).await.unwrap();
    let receipt = published(&dest, &entry, "out");
    let journal = SourceRemovalJournal::default();
    let mut append = Box::pin(journal.append_published(&entry, &receipt));
    assert!(futures::poll!(append.as_mut()).is_pending());
    drop(append);
    assert!(journal.append_published(&entry, &receipt).await.is_err());
    assert!(journal
        .replay(root.clone(), |_| std::future::ready(Ok(())))
        .await
        .is_err());
    let journal = SourceRemovalJournal::default();
    journal.append_published(&entry, &receipt).await.unwrap();
    let (entered, mut entering) = tokio::sync::mpsc::channel(1);
    let mut replay = Box::pin(journal.replay(root.clone(), |_| {
        entered.try_send(()).unwrap();
        std::future::pending::<io::Result<()>>()
    }));
    tokio::select! { result = &mut replay => panic!("unexpected result: {result:?}"), _ = entering.recv() => {} }
    drop(replay);
    assert_eq!(journal.close().await.unwrap(), 0);
    assert!(journal
        .replay(root, |_| std::future::ready(Ok(())))
        .await
        .is_err());
    assert!(source.path().join("a").exists());
}

#[tokio::test]
async fn cancelled_native_join_retains_work_and_reports_post_unlink_scratch_failure() {
    let source = tempfile::tempdir().unwrap();
    std::fs::write(source.path().join("a"), b"same").unwrap();
    let root = SourceRoot::open(source.path().into()).await.unwrap();
    let entry = observed(&root, "a");
    let mut native = NativeState::new(root).unwrap();
    let read_only = tempfile::NamedTempFile::new().unwrap();
    native.states.file = File::open(read_only.path()).unwrap();
    let journal = SourceRemovalJournal::default();
    let (release, wait) = std::sync::mpsc::channel();
    let mut state = journal.state.lock().await;
    state.terminal = true;
    let mut joining = Box::pin(state.native(true, move || {
        wait.recv().unwrap();
        native.remove(&entry).map(|native| (native, None))
    }));
    assert!(futures::poll!(joining.as_mut()).is_pending());
    drop(joining);
    drop(state);
    assert!(source.path().join("a").exists());
    release.send(()).unwrap();
    let error = journal.close().await.unwrap_err();
    assert_eq!(error.removed, 1);
    assert!(!error.removal_uncertain);
    assert!(matches!(error.cause, RemovalCause::Io(_)));
    assert!(!source.path().join("a").exists());
    assert_eq!(journal.close().await.unwrap(), 1);
}

#[test]
fn continuation_never_adopts_an_external_change_after_our_own_unlink() {
    let source = tempfile::tempdir().unwrap();
    std::fs::write(source.path().join("a"), b"same").unwrap();
    std::fs::hard_link(source.path().join("a"), source.path().join("b")).unwrap();
    let root = SourceRoot::open_blocking(source.path().into()).unwrap();
    let a = observed(&root, "a");
    let b = observed(&root, "b");
    assert_eq!(a.identity, b.identity);
    let native = NativeState::new(root)
        .unwrap()
        .validate(&a)
        .unwrap()
        .validate(&b)
        .unwrap();
    let native = native.remove(&a).unwrap();
    std::fs::write(source.path().join("b"), b"foreign change").unwrap();
    let error = match native.remove(&b) {
        Err(error) => error,
        Ok(_) => panic!("adopted a fresh source observation"),
    };
    assert_eq!(error.removed, 1);
    assert!(matches!(
        error.cause,
        RemovalCause::Rooted(RootedFsError::DestinationChanged(_))
    ));
    assert_eq!(
        std::fs::read(source.path().join("b")).unwrap(),
        b"foreign change"
    );
}

#[test]
fn continuation_state_corruption_is_terminal_even_for_other_and_missing_keys() {
    let source = tempfile::tempdir().unwrap();
    std::fs::write(source.path().join("a"), b"same").unwrap();
    let root = SourceRoot::open_blocking(source.path().into()).unwrap();
    let entry = observed(&root, "a");
    let state = root
        .rooted()
        .remove_hardlink_source_member_blocking(&entry.path, entry.identity.unwrap(), None)
        .unwrap();
    for corrupt_key in [false, true] {
        let mut index = StateIndex::new().unwrap();
        index.set([1; 32], state).unwrap();
        if corrupt_key {
            index.file.seek(SeekFrom::Start(0)).unwrap();
            index.file.write_all(&[2; 32]).unwrap();
        } else {
            index.file.set_len(STATE_RECORD_BYTES as u64 - 1).unwrap();
        }
        assert!(index.get([1; 32]).is_err());
        assert!(index.get([2; 32]).is_err());
        assert!(index.get([3; 32]).is_err());
        assert!(index.set([1; 32], state).is_err());
    }
}

#[cfg(unix)]
#[test]
fn non_h_aliases_replay_under_low_fd_limit_with_one_continuation_per_identity() {
    const CHILD: &str = "SY_STANDALONE_REMOVAL_RESOURCE_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let result = std::process::Command::new("sh").args([
            "-c", "ulimit -n 32; exec \"$1\" --exact engine::source_removals::tests::non_h_aliases_replay_under_low_fd_limit_with_one_continuation_per_identity --nocapture", "source-removal-resource-test",
        ]).arg(std::env::current_exe().unwrap()).env(CHILD, "1").output().unwrap();
        assert!(
            result.status.success(),
            "{}{}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
        return;
    }
    tokio::runtime::Builder::new_current_thread()
        .max_blocking_threads(1)
        .build()
        .unwrap()
        .block_on(async {
            const ALIASES: usize = 256;
            let source = tempfile::tempdir().unwrap();
            std::fs::write(source.path().join("member-0"), b"same").unwrap();
            for n in 1..ALIASES {
                std::fs::hard_link(
                    source.path().join("member-0"),
                    source.path().join(format!("member-{n}")),
                )
                .unwrap();
            }
            let root = SourceRoot::open(source.path().into()).await.unwrap();
            let destination = tempfile::tempdir().unwrap();
            let dest = RootedFs::open(destination.path().into()).await.unwrap();
            let journal = SourceRemovalJournal::default();
            for n in 0..ALIASES {
                let name = format!("member-{n}");
                let entry = observed(&root, &name);
                assert!(entry.hardlink_group.is_none());
                journal
                    .append_published(&entry, &published(&dest, &entry, &name))
                    .await
                    .unwrap();
            }
            // Exercise the full authenticated gates and actual standalone public API.
            assert_eq!(
                journal
                    .replay(root, |proof| std::future::ready(validate(proof, &dest)))
                    .await
                    .unwrap(),
                ALIASES as u64
            );
            assert_eq!(std::fs::read_dir(source.path()).unwrap().count(), 0);
            assert_eq!(
                std::fs::read_dir(destination.path()).unwrap().count(),
                ALIASES
            );

            // The native state file grows by identities, not by alias removals.
            std::fs::write(source.path().join("member-0"), b"same").unwrap();
            for n in 1..ALIASES {
                std::fs::hard_link(
                    source.path().join("member-0"),
                    source.path().join(format!("member-{n}")),
                )
                .unwrap();
            }
            let root = SourceRoot::open(source.path().into()).await.unwrap();
            let original = observed(&root, "member-0");
            let mut native = NativeState::new(root).unwrap();
            for n in 0..ALIASES {
                let mut entry = original.clone();
                entry.path = rel(&format!("member-{n}"));
                native = native.remove(&entry).unwrap();
                assert_eq!(
                    native.states.file.metadata().unwrap().len(),
                    STATE_RECORD_BYTES as u64
                );
            }
        });
}
