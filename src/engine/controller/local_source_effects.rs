//! Effect declarations reuse the completed plan and its exact journals. No
//! independent reconcile pass or destination authority is created here.
use super::*;
use crate::engine::backup::{BackupAddress, BackupPolicy};
use crate::engine::hardlink_runtime::intent::{destination, GroupIntent};
use crate::rooted_fs::backup::BackupRoot;
use crate::rooted_fs::local_source_effects::preflight::{EffectPreflight, EffectRoot};
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
        let preflight = EffectPreflight::new(
            effects,
            destination_root,
            backup.and_then(|(_, root)| root.as_ref()),
        )
        .await
        .map_err(|error| ControllerError::backend("LOCAL SOURCE preflight", error))?;
        let declarations = &preflight;
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
                                declare_publication_and_backup(
                                    declarations,
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
                let kind = if (operation.source().is_file() || operation.source().is_symlink())
                    && matches!(
                        operation,
                        SyncOp::Create { .. } | SyncOp::Update { .. } | SyncOp::Replace { .. }
                    ) {
                    SourceEffectKind::LeafPublication
                } else {
                    match operation {
                        SyncOp::Create { .. } if operation.source().is_directory() => {
                            SourceEffectKind::DirectoryNamespaceChange
                        }
                        SyncOp::Metadata { .. } | SyncOp::Unchanged { .. }
                            if operation.source().is_directory() =>
                        {
                            SourceEffectKind::DirectoryPolicy
                        }
                        SyncOp::Metadata { .. } | SyncOp::Unchanged { .. }
                            if operation.source().is_symlink() =>
                        {
                            SourceEffectKind::SymlinkMetadata
                        }
                        SyncOp::Metadata { .. } | SyncOp::Unchanged { .. } => {
                            SourceEffectKind::InodeMetadata
                        }
                        _ => SourceEffectKind::Unknown,
                    }
                };
                declare_publication_and_backup(declarations, backup, &operation, kind).await?;
            }
        }
        self.reader.rewind()?;
        if let Some(delete) = &mut self.delete {
            delete
                .validate_actions(|action| async move {
                    declarations
                        .declare(
                            EffectRoot::Main,
                            action.path.clone(),
                            SourceEffectKind::NamespaceRetirement,
                        )
                        .await
                        .map_err(|error| {
                            ControllerError::backend("LOCAL SOURCE delete effects", error)
                        })?;
                    if action.kind == crate::engine::domain::EntryKind::File {
                        declare_backup(declarations, backup, &action.path).await?;
                    }
                    Ok::<(), ControllerError>(())
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
                declarations
                    .declare(
                        EffectRoot::Main,
                        metadata.path,
                        if metadata.target == DirectoryTarget::Create {
                            SourceEffectKind::DirectoryNamespaceChange
                        } else if !extended_preservation && metadata.unix_mode.is_none() {
                            SourceEffectKind::DirectoryTimestamp
                        } else {
                            SourceEffectKind::DirectoryPolicy
                        },
                    )
                    .await
                    .map_err(|error| {
                        ControllerError::backend("LOCAL SOURCE directory effects", error)
                    })
            })
            .await?;
        preflight
            .finish()
            .await
            .map_err(|error| ControllerError::backend("LOCAL SOURCE preflight", error))?;
        // No root can materialize on one declaration while a later member,
        // backup, delete or directory policy remains unqualified.
        effects
            .complete_root_declarations(destination_root)
            .map_err(|error| ControllerError::backend("LOCAL SOURCE root effects", error))?;
        if let Some((_, Some(root))) = backup {
            root.complete_local_source_effects(effects)
                .map_err(|error| {
                    ControllerError::backend("LOCAL SOURCE backup root effects", error)
                })?;
        }
        Ok(())
    }
}

async fn declare_publication_and_backup(
    declarations: &EffectPreflight,
    backup: Option<&(BackupPolicy, Option<BackupRoot>)>,
    operation: &SyncOp,
    kind: SourceEffectKind,
) -> Result<()> {
    declarations
        .declare(EffectRoot::Main, operation.path().clone(), kind)
        .await
        .map_err(|error| ControllerError::backend("LOCAL SOURCE effects", error))?;
    if destination(operation).is_some_and(|old| old.is_file()) {
        declare_backup(declarations, backup, operation.path()).await?;
    }
    Ok(())
}

async fn declare_backup(
    declarations: &EffectPreflight,
    backup: Option<&(BackupPolicy, Option<BackupRoot>)>,
    path: &RelativePath,
) -> Result<()> {
    if let Some((policy, root)) = backup {
        let address = policy
            .destination(path)
            .map_err(|error| ControllerError::backend("LOCAL SOURCE backup address", error))?;
        let result = match address {
            BackupAddress::Main(path) => {
                declarations
                    .declare(EffectRoot::Main, path, SourceEffectKind::LeafPublication)
                    .await
            }
            BackupAddress::FixedRoot(path) => {
                root.as_ref().ok_or_else(|| {
                    ControllerError::backend(
                        "LOCAL SOURCE backup root",
                        std::io::Error::other("missing original root"),
                    )
                })?;
                declarations
                    .declare(EffectRoot::Backup, path, SourceEffectKind::LeafPublication)
                    .await
            }
        };
        result.map_err(|error| ControllerError::backend("LOCAL SOURCE backup effects", error))?;
    }
    Ok(())
}
