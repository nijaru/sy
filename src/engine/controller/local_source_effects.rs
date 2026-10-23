//! Effect declarations reuse the completed plan and its exact journals. No
//! independent reconcile pass or destination authority is created here.
use super::*;
use crate::engine::backup::{BackupAddress, BackupPolicy};
use crate::engine::hardlink_runtime::intent::{destination, GroupIntent};
use crate::rooted_fs::backup::BackupRoot;
use crate::rooted_fs::local_source_effects::{LocalSourceEffects, SourceEffectKind};
use crate::rooted_fs::operand::DestinationRoot;

impl SyncPlan {
    /// LOCAL-only completed effect admission; declaration-free work retains
    /// observation authority. Unsupported native predicates remain unknown.
    /// A lowered publication includes staging/cleanup, OLD retirement, backup
    /// slots/parents and containing-directory effects, not just the visible leaf.
    pub async fn validate_local_source_effects<F>(
        &mut self,
        effects: &LocalSourceEffects,
        destination_root: &DestinationRoot,
        backup: Option<&(BackupPolicy, Option<BackupRoot>)>,
        extended_preservation: bool,
        mut lowers_effect: F,
    ) -> Result<()>
    where
        F: FnMut(SyncOp, ExecutionPolicy) -> Result<bool>,
    {
        self.hardlinks.revalidate(&self.reader).await?;
        while let Some(record) = self.reader.next_record().await? {
            let operation = record.operation;
            if !matches!(operation, SyncOp::Skip { .. }) {
                if let (Some(key), HardlinkPreflight::Ready(catalogue)) =
                    (operation.source().hardlink_group, &self.hardlinks)
                {
                    let group = crate::engine::hardlink_runtime::HardlinkGroupPlan::new(
                        catalogue,
                        &self.reader,
                        *key.as_bytes(),
                    )
                    .await?;
                    let first_position = group.members().await?.first_position().await?;
                    if record.position == first_position {
                        let mut members = group.members().await?;
                        let first = members.next().await?.ok_or(
                            crate::engine::hardlink_runtime::GroupError::Invalid("empty group"),
                        )?;
                        let mut intent = GroupIntent::new(&first, self.execution_policy);
                        while let Some(member) = members.next().await? {
                            intent.include(&member);
                        }
                        let matching = destination(&first).is_some_and(|old| {
                            intent
                                .unix_mode
                                .is_none_or(|mode| old.unix_mode == Some(mode))
                                && intent.modified.is_none_or(|time| old.modified == time)
                        });
                        if intent.canonical_source || extended_preservation || !matching {
                            // Even M/E members can publish for isolation or
                            // topology repair. Declare every OLD retirement and
                            // backup before any group-private creation. This
                            // does not grant native created-inode link authority.
                            let mut members = group.members().await?;
                            while let Some(member) = members.next().await? {
                                validate_publication_and_backup(
                                    effects,
                                    destination_root,
                                    backup,
                                    &member,
                                    SourceEffectKind::LeafPublication,
                                )
                                .await?;
                            }
                        }
                    }
                    continue;
                }
            }
            if lowers_effect(operation.clone(), self.execution_policy)? {
                let kind = if operation.source().is_file()
                    && matches!(operation, SyncOp::Create { .. } | SyncOp::Update { .. })
                {
                    SourceEffectKind::LeafPublication
                } else {
                    match operation {
                        SyncOp::Create { .. } if operation.source().is_directory() => {
                            SourceEffectKind::DirectoryNamespaceChange
                        }
                        SyncOp::Metadata { .. } | SyncOp::Unchanged { .. } => {
                            SourceEffectKind::InodeMetadata
                        }
                        _ => SourceEffectKind::Unknown,
                    }
                };
                validate_publication_and_backup(
                    effects,
                    destination_root,
                    backup,
                    &operation,
                    kind,
                )
                .await?;
            }
        }
        self.reader.rewind()?;
        if let Some(delete) = &mut self.delete {
            delete
                .validate_actions(|action| async move {
                    effects
                        .validate_effect(
                            destination_root,
                            action.path,
                            SourceEffectKind::NamespaceRetirement,
                        )
                        .await
                        .map_err(|error| {
                            ControllerError::backend("LOCAL SOURCE delete effects", error)
                        })
                })
                .await?;
        }
        self.finalize
            .validate_records(|metadata| async move {
                if !extended_preservation
                    && metadata.unix_mode.is_none()
                    && metadata.modified.is_none()
                {
                    // The journal also retains read-only SOURCE observation
                    // work. Its existence is not a directory setter.
                    return Ok(());
                }
                effects
                    .validate_effect(
                        destination_root,
                        metadata.path,
                        if !extended_preservation && metadata.unix_mode.is_none() {
                            SourceEffectKind::DirectoryTimestamp
                        } else {
                            SourceEffectKind::InodeMetadata
                        },
                    )
                    .await
                    .map_err(|error| {
                        ControllerError::backend("LOCAL SOURCE directory effects", error)
                    })
            })
            .await?;
        Ok(())
    }
}

async fn validate_publication_and_backup(
    effects: &LocalSourceEffects,
    destination_root: &DestinationRoot,
    backup: Option<&(BackupPolicy, Option<BackupRoot>)>,
    operation: &SyncOp,
    kind: SourceEffectKind,
) -> Result<()> {
    effects
        .validate_effect(destination_root, operation.path().clone(), kind)
        .await
        .map_err(|error| ControllerError::backend("LOCAL SOURCE effects", error))?;
    if let Some((policy, root)) = backup {
        if destination(operation).is_some_and(|old| old.is_file()) {
            let address = policy
                .destination(operation.path())
                .map_err(|error| ControllerError::backend("LOCAL SOURCE backup address", error))?;
            let result = match address {
                BackupAddress::Main(path) => {
                    effects
                        .validate_effect(destination_root, path, SourceEffectKind::LeafPublication)
                        .await
                }
                BackupAddress::FixedRoot(path) => {
                    root.as_ref()
                        .ok_or_else(|| {
                            ControllerError::backend(
                                "LOCAL SOURCE backup root",
                                std::io::Error::other("missing original root"),
                            )
                        })?
                        .validate_local_source_effect(effects, path)
                        .await
                }
            };
            result
                .map_err(|error| ControllerError::backend("LOCAL SOURCE backup effects", error))?;
        }
    }
    Ok(())
}
