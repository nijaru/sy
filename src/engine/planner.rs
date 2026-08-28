use super::domain::{Entry, EntryKind, SkipReason, SyncOp};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ComparisonMode {
    /// Size and modification timestamp, matching the fast default sync model.
    #[default]
    Quick,
    /// Size only.
    SizeOnly,
    /// BLAKE3/content comparison when sizes match.
    Checksum,
    /// Treat every existing non-directory entry as changed.
    Always,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ComparisonPolicy {
    pub mode: ComparisonMode,
    pub ignore_existing: bool,
    pub update_only: bool,
    pub preserve_permissions: bool,
    pub preserve_times: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanDecision {
    Ready(SyncOp),
    NeedContentComparison {
        source: Entry,
        destination: Entry,
    },
}

/// Plans one source-backed path without choosing a byte-transfer strategy.
///
/// `NeedContentComparison` is deliberately explicit: the engine controller can
/// request local hashes or remote signatures/hashes only for entries that
/// actually require them, keeping reconciliation itself free of endpoint I/O.
pub fn plan_entry(
    source: Entry,
    destination: Option<Entry>,
    policy: ComparisonPolicy,
) -> PlanDecision {
    let Some(destination) = destination else {
        return PlanDecision::Ready(SyncOp::Create { source });
    };

    if policy.ignore_existing {
        return PlanDecision::Ready(SyncOp::Skip {
            path: source.path,
            reason: SkipReason::ExistingOnly,
        });
    }

    if policy.update_only && destination.modified > source.modified {
        return PlanDecision::Ready(SyncOp::Skip {
            path: source.path,
            reason: SkipReason::DestinationNewer,
        });
    }

    if source.kind != destination.kind {
        return PlanDecision::Ready(SyncOp::Replace {
            source,
            destination,
        });
    }

    match source.kind {
        EntryKind::Directory => {
            PlanDecision::Ready(metadata_or_skip(source, destination, policy))
        }
        EntryKind::Symlink => {
            if source.symlink_target != destination.symlink_target {
                PlanDecision::Ready(SyncOp::Update {
                    source,
                    destination,
                })
            } else {
                PlanDecision::Ready(metadata_or_skip(source, destination, policy))
            }
        }
        EntryKind::File => plan_file(source, destination, policy),
    }
}

pub fn finish_content_comparison(
    source: Entry,
    destination: Entry,
    contents_equal: bool,
    policy: ComparisonPolicy,
) -> SyncOp {
    debug_assert_eq!(source.kind, EntryKind::File);
    debug_assert_eq!(destination.kind, EntryKind::File);

    if contents_equal {
        metadata_or_skip(source, destination, policy)
    } else {
        SyncOp::Update {
            source,
            destination,
        }
    }
}

fn plan_file(source: Entry, destination: Entry, policy: ComparisonPolicy) -> PlanDecision {
    match policy.mode {
        ComparisonMode::Always => PlanDecision::Ready(SyncOp::Update {
            source,
            destination,
        }),
        ComparisonMode::SizeOnly => {
            if source.size == destination.size {
                PlanDecision::Ready(metadata_or_skip(source, destination, policy))
            } else {
                PlanDecision::Ready(SyncOp::Update {
                    source,
                    destination,
                })
            }
        }
        ComparisonMode::Quick => {
            if source.size == destination.size && source.modified == destination.modified {
                PlanDecision::Ready(metadata_or_skip(source, destination, policy))
            } else {
                PlanDecision::Ready(SyncOp::Update {
                    source,
                    destination,
                })
            }
        }
        ComparisonMode::Checksum => {
            if source.size != destination.size {
                PlanDecision::Ready(SyncOp::Update {
                    source,
                    destination,
                })
            } else {
                PlanDecision::NeedContentComparison {
                    source,
                    destination,
                }
            }
        }
    }
}

fn metadata_or_skip(source: Entry, destination: Entry, policy: ComparisonPolicy) -> SyncOp {
    let permission_change = policy.preserve_permissions && source.unix_mode != destination.unix_mode;
    let time_change = policy.preserve_times && source.modified != destination.modified;

    if permission_change || time_change {
        SyncOp::Metadata {
            source,
            destination,
        }
    } else {
        SyncOp::Skip {
            path: source.path,
            reason: SkipReason::Unchanged,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::domain::{RelativePath, Timestamp};
    use std::path::PathBuf;

    fn timestamp(seconds: i64) -> Timestamp {
        Timestamp::new(seconds, 0).unwrap()
    }

    fn file(path: &str, size: u64, modified: i64) -> Entry {
        Entry::file(RelativePath::new(path).unwrap(), size, timestamp(modified))
    }

    #[test]
    fn missing_destination_creates_without_extra_io() {
        assert!(matches!(
            plan_entry(file("a", 3, 1), None, ComparisonPolicy::default()),
            PlanDecision::Ready(SyncOp::Create { .. })
        ));
    }

    #[test]
    fn type_change_is_semantic_replace() {
        let source = Entry::symlink(
            RelativePath::new("entry").unwrap(),
            PathBuf::from("target"),
            timestamp(1),
        );
        let destination = file("entry", 6, 1);
        assert!(matches!(
            plan_entry(source, Some(destination), ComparisonPolicy::default()),
            PlanDecision::Ready(SyncOp::Replace { .. })
        ));
    }

    #[test]
    fn checksum_mode_requests_content_only_when_sizes_match() {
        let policy = ComparisonPolicy {
            mode: ComparisonMode::Checksum,
            ..ComparisonPolicy::default()
        };
        assert!(matches!(
            plan_entry(file("a", 8, 1), Some(file("a", 8, 2)), policy),
            PlanDecision::NeedContentComparison { .. }
        ));
        assert!(matches!(
            plan_entry(file("a", 8, 1), Some(file("a", 9, 2)), policy),
            PlanDecision::Ready(SyncOp::Update { .. })
        ));
    }

    #[test]
    fn equal_content_can_still_require_metadata() {
        let mut source = file("a", 8, 1);
        source.unix_mode = Some(0o755);
        let mut destination = file("a", 8, 2);
        destination.unix_mode = Some(0o644);
        let policy = ComparisonPolicy {
            mode: ComparisonMode::Checksum,
            preserve_permissions: true,
            ..ComparisonPolicy::default()
        };

        assert!(matches!(
            finish_content_comparison(source, destination, true, policy),
            SyncOp::Metadata { .. }
        ));
    }

    #[test]
    fn update_only_short_circuits_newer_destination() {
        let policy = ComparisonPolicy {
            update_only: true,
            mode: ComparisonMode::Always,
            ..ComparisonPolicy::default()
        };
        assert!(matches!(
            plan_entry(file("a", 8, 1), Some(file("a", 9, 2)), policy),
            PlanDecision::Ready(SyncOp::Skip {
                reason: SkipReason::DestinationNewer,
                ..
            })
        ));
    }
}
