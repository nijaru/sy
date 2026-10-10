use super::*;
use crate::engine::domain::{
    ContentComparison, Entry, EntryIdentity, RelativePath, SkipReason, SyncOp, Timestamp,
};
use crate::engine::plan_journal::{PlanJournal, PlanRecord};
use crate::engine::scheduler::Scheduler;

fn source(name: &str, group: u8) -> Entry {
    let mut source = Entry::file(RelativePath::new(name).unwrap(), 6, Timestamp::UNIX_EPOCH);
    source.identity = Some(EntryIdentity::from_bytes([group; 32]));
    source.hardlink_group = Some(EntryIdentity::from_bytes([group; 32]));
    source
}

fn create(name: &str, group: u8) -> SyncOp {
    let source = source(name, group);
    SyncOp::Create {
        destination_path: source.path.clone(),
        source,
    }
}

async fn build(operations: &[SyncOp]) -> (HardlinkCatalogue, PlanJournalReader) {
    let mut journal = PlanJournal::new().await.unwrap();
    let mut commitments = HardlinkCommitments::default();
    let scheduler = Scheduler::new(Default::default()).unwrap();
    for operation in operations {
        let position = journal.append(operation).await.unwrap();
        commitments
            .check(
                PlanRecord {
                    position,
                    operation: operation.clone(),
                },
                &scheduler,
                &mut |_| std::future::ready(Ok::<_, HardlinkPreflightError>([7; 32])),
            )
            .await
            .unwrap();
    }
    (
        commitments.seal().await.unwrap(),
        journal.seal().await.unwrap(),
    )
}

#[tokio::test]
async fn interleaved_complete_membership_keeps_retained_operations_and_first_seen_order() {
    let pair = |name| {
        let source = source(name, 1);
        let mut destination = source.clone();
        destination.identity = Some(EntryIdentity::from_bytes([9; 32]));
        destination.hardlink_group = None;
        (source, destination)
    };
    let (metadata_source, metadata_destination) = pair("c");
    let (unchanged_source, unchanged_destination) = pair("e");
    let (update_source, update_destination) = pair("g");
    let (replace_source, replace_destination) = pair("h");
    let operations = [
        create("a", 1),
        create("b", 2),
        SyncOp::Metadata {
            source: metadata_source,
            destination: metadata_destination,
        },
        create("d", 2),
        SyncOp::Unchanged {
            source: unchanged_source,
            destination: unchanged_destination,
            comparison: ContentComparison::Unverified,
        },
        SyncOp::Skip {
            source: source("f", 1),
            reason: SkipReason::Filtered,
        },
        SyncOp::Update {
            source: update_source,
            destination: update_destination,
        },
        SyncOp::Replace {
            source: replace_source,
            destination: replace_destination,
        },
    ];
    let (mut catalogue, mut plan) = build(&operations).await;
    assert_eq!((catalogue.groups(), catalogue.member_count()), (2, 7));
    catalogue.revalidate(&plan).await.unwrap();
    let mut group_offset = 0;
    let mut got = Vec::new();
    for _ in 0..catalogue.groups() {
        let (baseline, next) = catalogue
            .with_disk(move |disk| disk.get_at(group_offset))
            .await
            .unwrap();
        let group = *baseline
            .operation
            .source()
            .hardlink_group
            .unwrap()
            .as_bytes();
        let mut cursor = catalogue.members(group).await.unwrap();
        let mut ordinals = Vec::new();
        while let Some(member) = cursor.next().await.unwrap() {
            assert_eq!(
                plan.read_at(member.position).await.unwrap(),
                operations[member.position.ordinal as usize]
            );
            ordinals.push(member.position.ordinal);
        }
        got.push((group[0], ordinals));
        group_offset = next;
    }
    assert_eq!(got, [(1, vec![0, 2, 4, 6, 7]), (2, vec![1, 3])]);
    // Random membership replay did not disturb ordinary sequential preview.
    for operation in operations {
        assert_eq!(plan.next().await.unwrap(), Some(operation));
    }
    assert_eq!(plan.next().await.unwrap(), None);
}

#[tokio::test]
async fn corrupt_membership_or_valid_looking_baseline_change_permanently_poison_catalogue() {
    for fault in [
        "cycle",
        "missing",
        "group",
        "ordinal",
        "baseline",
        "truncation",
    ] {
        let (mut catalogue, plan) = build(&[create("a", 1), create("b", 1), create("c", 1)]).await;
        catalogue.revalidate(&plan).await.unwrap();
        let restored = catalogue
            .with_disk(move |disk| {
                let (file, offset, replacement) = match fault {
                    "cycle" => (&mut disk.members, 84, 0_u64.to_le_bytes().to_vec()),
                    "missing" => (&mut disk.members, 84, NO_MEMBER.to_le_bytes().to_vec()),
                    "group" => (&mut disk.members, MEMBER_BYTES, vec![2; 32]),
                    "ordinal" => (
                        &mut disk.members,
                        MEMBER_BYTES + 40,
                        0_u64.to_le_bytes().to_vec(),
                    ),
                    // Digest bytes with a valid flag/header remain parseable: the
                    // immutable aggregate seal must still reject their replacement.
                    "baseline" => (&mut disk.file, 5, vec![42; 32]),
                    "truncation" => {
                        disk.members.set_len(disk.member_end - 1)?;
                        return Ok(None);
                    }
                    _ => unreachable!(),
                };
                let mut original = vec![0; replacement.len()];
                file.seek(SeekFrom::Start(offset))?;
                file.read_exact(&mut original)?;
                file.seek(SeekFrom::Start(offset))?;
                file.write_all(&replacement)?;
                Ok(Some((offset, original)))
            })
            .await
            .unwrap();
        assert!(catalogue.revalidate(&plan).await.is_err(), "{fault}");
        // Even repairing the bytes must not revive failed authority.
        if let Some((offset, original)) = restored {
            let mut guard = catalogue.disk.lock().unwrap();
            let disk = guard.as_mut().unwrap();
            let file = if fault == "baseline" {
                &mut disk.file
            } else {
                &mut disk.members
            };
            file.seek(SeekFrom::Start(offset)).unwrap();
            file.write_all(&original).unwrap();
        }
        assert!(
            catalogue.revalidate(&plan).await.is_err(),
            "{fault} revived after repair"
        );
    }
}

#[tokio::test]
async fn failed_or_cancelled_byte_check_cannot_seal_partial_membership() {
    for cancel in [false, true] {
        let scheduler = Scheduler::new(Default::default()).unwrap();
        let mut journal = PlanJournal::new().await.unwrap();
        let mut commitments = HardlinkCommitments::default();
        let first = create("a", 1);
        let position = journal.append(&first).await.unwrap();
        commitments
            .check::<_, _, HardlinkPreflightError>(
                PlanRecord {
                    position,
                    operation: first,
                },
                &scheduler,
                &mut |_| async { panic!("one source commitment needs no bytes") },
            )
            .await
            .unwrap();
        let source = source("b", 1);
        let mut destination = source.clone();
        destination.identity = Some(EntryIdentity::from_bytes([9; 32]));
        let second = SyncOp::Metadata {
            source,
            destination,
        };
        let position = journal.append(&second).await.unwrap();
        if cancel {
            let (entered, mut receiving) = tokio::sync::mpsc::channel(1);
            let mut hash = move |_| {
                let entered = entered.clone();
                async move {
                    entered.send(()).await.unwrap();
                    std::future::pending::<Result<[u8; 32]>>().await
                }
            };
            {
                let checking = commitments.check(
                    PlanRecord {
                        position,
                        operation: second,
                    },
                    &scheduler,
                    &mut hash,
                );
                tokio::pin!(checking);
                tokio::select! {
                    result = &mut checking => panic!("check unexpectedly completed: {result:?}"),
                    _ = receiving.recv() => {},
                }
            }
        } else {
            let result = commitments
                .check(
                    PlanRecord {
                        position,
                        operation: second,
                    },
                    &scheduler,
                    &mut |_| async {
                        Err::<[u8; 32], _>(HardlinkPreflightError::Io(std::io::Error::other(
                            "fingerprint failed",
                        )))
                    },
                )
                .await;
            assert!(result.is_err());
        }
        assert!(commitments.seal().await.is_err());
    }
}
