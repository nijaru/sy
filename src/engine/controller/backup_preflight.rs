use super::*;
use crate::engine::backup::BackupAddress;
use crate::engine::namespace::{NamespaceCollisionDetector, NamespacePreflight};
use futures::StreamExt;

struct Declarations<'a> {
    preflight: &'a NamespacePreflight<'a>,
    main: NamespaceCollisionDetector,
    external: Option<(
        NamespaceCollisionDetector,
        crate::rooted_fs::DestinationNamespace,
    )>,
}
impl Declarations<'_> {
    async fn record(&mut self, path: RelativePath) -> Result<()> {
        let Some(backup) = &self.preflight.backup else {
            return Ok(());
        };
        let address = backup
            .policy
            .destination(&path)
            .map_err(|error| ControllerError::backend("backup address", error))?;
        match address {
            BackupAddress::Main(path) => {
                self.main.record(&path, backup.qualification).await?;
                backup.qualification.validate_backup_address(path).await?;
            }
            BackupAddress::FixedRoot(path) => {
                if self.external.is_none() {
                    let root = backup.external.ok_or_else(|| {
                        ControllerError::backend(
                            "backup root",
                            std::io::Error::other("missing original backup authority"),
                        )
                    })?;
                    let (qualification, mut entries) = root
                        .prepare()
                        .await
                        .map_err(|error| ControllerError::backend("backup root", error))?;
                    let mut detector = NamespaceCollisionDetector::new();
                    let result = async {
                        while let Some(entry) = entries.next().await {
                            let entry = entry.map_err(|error| ControllerError::Backend {
                                phase: "backup scan",
                                source: error,
                            })?;
                            detector.record(&entry.path, &qualification).await?;
                        }
                        Ok::<_, ControllerError>(())
                    }
                    .await;
                    let drained = entries.close().await;
                    result?;
                    drained.map_err(|error| ControllerError::Backend {
                        phase: "backup scan drain",
                        source: error,
                    })?;
                    self.external = Some((detector, qualification));
                }
                let (detector, qualification) = self.external.as_mut().ok_or_else(|| {
                    ControllerError::backend(
                        "backup namespace",
                        std::io::Error::other("missing prepared backup namespace"),
                    )
                })?;
                detector.record(&path, qualification).await?;
                qualification.validate_backup_address(path).await?;
            }
        }
        Ok(())
    }
}

pub(super) async fn complete(
    mut plan: SyncPlan,
    detector: NamespaceCollisionDetector,
    preflight: &NamespacePreflight<'_>,
) -> Result<SyncPlan> {
    let declarations = tokio::sync::Mutex::new(Declarations {
        preflight,
        main: detector,
        external: None,
    });
    plan.validate_operations(|operation| {
        let declarations = &declarations;
        async move {
            let candidate = match operation {
                SyncOp::Update { destination, .. } | SyncOp::Replace { destination, .. }
                    if destination.is_file() =>
                {
                    Some(destination.path)
                }
                SyncOp::Metadata {
                    source,
                    destination,
                }
                | SyncOp::Unchanged {
                    source,
                    destination,
                    ..
                } if source.is_file()
                    && destination.is_file()
                    && preflight
                        .backup
                        .as_ref()
                        .is_some_and(|backup| backup.potential_hardlinks) =>
                {
                    Some(destination.path)
                }
                _ => None,
            };
            if let Some(path) = candidate {
                declarations.lock().await.record(path).await?;
            }
            Ok(())
        }
    })
    .await?;
    if let Some(delete) = &mut plan.delete {
        delete
            .validate_actions(|action| {
                let declarations = &declarations;
                async move {
                    if action.kind == crate::engine::domain::EntryKind::File {
                        declarations.lock().await.record(action.path).await?;
                    }
                    Ok::<_, ControllerError>(())
                }
            })
            .await?;
    }
    let declarations = declarations.into_inner();
    declarations.main.finish().await?;
    if let Some((detector, qualification)) = declarations.external {
        detector.finish().await?;
        // All collision proofs must complete before either native owner seals.
        preflight.destination.seal().await?;
        qualification.seal().await?;
    } else {
        preflight.destination.seal().await?;
    }
    Ok(plan)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::domain::{Entry, Timestamp};
    use crate::engine::namespace::{
        BackupNamespace, FixedNamespaceQualification, NamespaceSemantics,
    };

    fn entries(mut entries: Vec<Entry>) -> EntryStream {
        entries.sort_by(|a, b| a.path.cmp(&b.path));
        EntryStream::new(futures::stream::iter(entries.into_iter().map(Ok)))
    }
    fn file(path: &str, size: u64) -> Entry {
        Entry::file(
            RelativePath::new(path).unwrap(),
            size,
            Timestamp::UNIX_EPOCH,
        )
    }

    #[tokio::test]
    async fn regular_file_to_symlink_backup_cannot_alias_a_kept_name() {
        let qualification =
            FixedNamespaceQualification(NamespaceSemantics::CASE_AND_NORMALIZATION_FOLDED);
        let policy = crate::engine::backup::BackupPolicy::in_root(None, "~".into());
        let source = Entry::symlink(
            RelativePath::new("foo").unwrap(),
            "target".into(),
            Timestamp::UNIX_EPOCH,
        );
        let result = preflight_sync(
            entries(vec![source]),
            entries(vec![file("foo", 1), file("FOO~", 1)]),
            ComparisonPolicy::default(),
            None,
            NamespacePreflight::new(&qualification).with_backup(BackupNamespace {
                policy: &policy,
                qualification: &qualification,
                external: None,
                potential_hardlinks: false,
            }),
            |_| true,
        )
        .await;
        assert!(matches!(result, Err(ControllerError::Namespace(_))));
    }

    #[tokio::test]
    async fn backup_names_constrain_kept_planned_and_potential_group_names() {
        let qualification =
            FixedNamespaceQualification(NamespaceSemantics::CASE_AND_NORMALIZATION_FOLDED);
        let policy = crate::engine::backup::BackupPolicy::in_root(None, "~".into());
        // Ordinary unchanged work does not reserve a backup; -H may publish
        // that same member for topology repair and must reserve its old bytes.
        for (changed, hardlinks, planned_alias, fails) in [
            (true, false, false, true),
            (true, false, true, true),
            (false, false, false, false),
            (false, true, false, true),
        ] {
            let mut source = vec![file("a", if changed { 2 } else { 1 })];
            let mut destination = vec![file("a", 1)];
            if planned_alias {
                source.push(file("A~", 1));
            } else {
                destination.push(file("A~", 1));
            }
            let namespace = NamespacePreflight::new(&qualification).with_backup(BackupNamespace {
                policy: &policy,
                qualification: &qualification,
                external: None,
                potential_hardlinks: hardlinks,
            });
            let result = preflight_sync(
                entries(source),
                entries(destination),
                ComparisonPolicy::default(),
                None,
                namespace,
                |_| true,
            )
            .await;
            assert_eq!(
                result.is_err(),
                fails,
                "changed={changed} H={hardlinks} planned={planned_alias}"
            );
            if fails {
                assert!(matches!(result, Err(ControllerError::Namespace(_))));
            }
        }
    }
}
