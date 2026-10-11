use super::*;
use crate::engine::domain::{Entry, EntryIdentity, Timestamp};
use crate::engine::plan_journal::PlanJournal;

fn create(name: &str, group: [u8; 32]) -> SyncOp {
    let mut source = Entry::file(RelativePath::new(name).unwrap(), 6, Timestamp::UNIX_EPOCH);
    source.identity = Some(EntryIdentity::from_bytes(group));
    source.hardlink_group = Some(EntryIdentity::from_bytes(group));
    SyncOp::Create {
        destination_path: source.path.clone(),
        source,
    }
}

async fn collect(
    membership: &mut HardlinkCommitments,
    journal: &mut PlanJournal,
    operation: SyncOp,
) -> Result<()> {
    let position = journal.append(&operation).await?;
    membership
        .collect(PlanRecord {
            position,
            operation,
        })
        .await
}

#[tokio::test]
async fn changed_or_missing_original_source_observation_is_terminal() {
    for missing in [false, true] {
        let mut membership = HardlinkCommitments::default();
        let mut journal = PlanJournal::new().await.unwrap();
        collect(&mut membership, &mut journal, create("a", [1; 32]))
            .await
            .unwrap();
        let mut changed = create("b", [1; 32]);
        if let SyncOp::Create { source, .. } = &mut changed {
            source.identity = if missing {
                None
            } else {
                Some(EntryIdentity::from_bytes([8; 32]))
            };
        }
        let error = collect(&mut membership, &mut journal, changed)
            .await
            .unwrap_err();
        assert!(matches!(
            (missing, error),
            (true, HardlinkPreflightError::MissingObservation(_))
                | (false, HardlinkPreflightError::SourceChanged { .. })
        ));
        assert!(collect(&mut membership, &mut journal, create("c", [1; 32]))
            .await
            .is_err());
        assert!(membership.seal().await.is_err());
    }
}

#[tokio::test]
async fn corrupt_record_lengths_fail_before_allocation_and_poison_storage() {
    for length in [0, u32::MAX] {
        let membership = HardlinkCommitments::default();
        membership
            .with_disk(|disk| disk.insert([1; 32], &create("a", [1; 32])))
            .await
            .unwrap();
        membership
            .with_disk(move |disk| {
                disk.file.seek(SeekFrom::Start(0))?;
                disk.file.write_all(&length.to_le_bytes())?;
                Ok(())
            })
            .await
            .unwrap();
        assert!(
            matches!(membership.with_disk(|disk| disk.get([1; 32])).await,
            Err(HardlinkPreflightError::Io(error)) if error.kind() == io::ErrorKind::InvalidData)
        );
        assert!(membership
            .with_disk(|disk| disk.get([1; 32]))
            .await
            .is_err());
    }
}

#[cfg(unix)]
#[test]
fn membership_remains_exact_with_bounded_memory_and_descriptors() {
    const CHILD: &str = "SY_HARDLINK_MEMBERSHIP_RESOURCE_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let result = std::process::Command::new("sh")
            .args(["-c", "ulimit -n 32; exec \"$1\" --exact engine::hardlink_preflight::tests::membership_remains_exact_with_bounded_memory_and_descriptors --nocapture", "hardlink-membership-test"])
            .arg(std::env::current_exe().unwrap()).env(CHILD, "1").output().unwrap();
        assert!(
            result.status.success(),
            "{}{}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
        return;
    }
    fn rss_kib() -> usize {
        let output = std::process::Command::new("ps")
            .args(["-o", "rss=", "-p", &std::process::id().to_string()])
            .output()
            .unwrap();
        assert!(output.status.success());
        String::from_utf8(output.stdout)
            .unwrap()
            .trim()
            .parse()
            .unwrap()
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .max_blocking_threads(1)
        .build()
        .unwrap();
    runtime.block_on(async {
        let mut membership = HardlinkCommitments::default();
        let mut journal = PlanJournal::new().await.unwrap();
        collect(&mut membership, &mut journal, create("a", [0; 32]))
            .await
            .unwrap();
        let before = rss_kib();
        let path = "p".repeat(4096);
        // 64 MiB of native path records, without a tree-sized resident map.
        for index in 1..=8192_u64 {
            let group = *blake3::hash(&index.to_le_bytes()).as_bytes();
            collect(&mut membership, &mut journal, create(&path, group))
                .await
                .unwrap();
        }
        let after = rss_kib();
        assert!(
            after < before + 8 * 1024,
            "RSS grew from {before} to {after} KiB"
        );
        for index in [1_u64, 4096, 8192] {
            let group = *blake3::hash(&index.to_le_bytes()).as_bytes();
            collect(&mut membership, &mut journal, create("later-member", group))
                .await
                .unwrap();
        }
        let mut catalogue = membership.seal().await.unwrap();
        let reader = journal.seal().await.unwrap();
        assert_eq!((catalogue.groups(), catalogue.member_count()), (8193, 8196));
        catalogue.revalidate(&reader).await.unwrap();
        assert!(rss_kib() < before + 8 * 1024);
    });
}
