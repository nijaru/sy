use super::*;
use crate::engine::domain::{Entry, RelativePath, Timestamp};
use crate::engine::hardlink_preflight::HardlinkCommitments;
use crate::engine::plan_journal::PlanJournal;

async fn selection() -> (HardlinkCatalogue, PlanJournalReader, Vec<SyncOp>) {
    let mut journal = PlanJournal::new().await.unwrap();
    let mut operations = Vec::new();
    for name in ["a", "z/inner/b"] {
        let mut source = Entry::file(RelativePath::new(name).unwrap(), 4, Timestamp::UNIX_EPOCH);
        source.identity = Some(EntryIdentity::from_bytes([1; 32]));
        source.hardlink_group = Some(EntryIdentity::from_bytes([2; 32]));
        let operation = SyncOp::Create {
            destination_path: source.path.clone(),
            source,
        };
        journal.append(&operation).await.unwrap();
        operations.push(operation);
    }
    let mut reader = journal.seal().await.unwrap();
    let mut builder = HardlinkCommitments::default();
    while let Some(record) = reader.next_record().await.unwrap() {
        builder.collect(record).await.unwrap();
    }
    let mut catalogue = builder.seal().await.unwrap();
    catalogue.revalidate(&reader).await.unwrap();
    (catalogue, reader, operations)
}

#[tokio::test]
async fn completion_requires_each_original_selected_member_and_one_final_inode() {
    let (catalogue, reader, operations) = selection().await;
    let proof = |operation: &SyncOp, identity| PublishedFileProof {
        path: operation.path().clone(),
        identity: EntryIdentity::from_bytes([identity; 32]),
    };
    for failure in ["partial", "wrong-path", "wrong-inode", "duplicate"] {
        let mut group = HardlinkGroupPlan::new(&catalogue, &reader, [2; 32])
            .await
            .unwrap();
        group
            .confirm_member(&operations[0], &proof(&operations[0], 3))
            .await
            .unwrap();
        match failure {
            "partial" => assert!(group.finish().await.is_err()),
            "wrong-path" => assert!(group
                .confirm_member(&operations[1], &proof(&operations[0], 3))
                .await
                .is_err()),
            "wrong-inode" => assert!(group
                .confirm_member(&operations[1], &proof(&operations[1], 4))
                .await
                .is_err()),
            "duplicate" => assert!(group
                .confirm_member(&operations[0], &proof(&operations[0], 3))
                .await
                .is_err()),
            _ => unreachable!(),
        }
    }
    let mut group = HardlinkGroupPlan::new(&catalogue, &reader, [2; 32])
        .await
        .unwrap();
    for operation in &operations {
        group
            .confirm_member(operation, &proof(operation, 3))
            .await
            .unwrap();
    }
    group.finish().await.unwrap();
}

#[tokio::test]
async fn disk_completion_cannot_skip_another_original_group_or_first_record() {
    let (catalogue, reader, _) = selection().await;
    let group = HardlinkGroupPlan::new(&catalogue, &reader, [2; 32])
        .await
        .unwrap();
    let first = group
        .members()
        .await
        .unwrap()
        .first_position()
        .await
        .unwrap();
    let completed = CompletedGroups::default();
    assert!(!completed.contains([2; 32], first).await.unwrap());
    completed.insert([2; 32], first, [3; 32]).await.unwrap();
    assert!(completed.contains([2; 32], first).await.unwrap());
    assert!(!completed.contains([4; 32], first).await.unwrap());
    let mut changed = first;
    changed.digest[0] ^= 1;
    assert!(completed.contains([2; 32], changed).await.is_err());
    assert!(completed.contains([2; 32], first).await.is_err());
}
