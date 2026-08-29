use super::delete_journal::{DeleteJournal, DeleteJournalReader, DeleteKind};
use super::domain::{Entry, InvalidRelativePath, RelativePath};
use std::io;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeletePolicy {
    pub threshold: u8,
    pub force: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeleteAction {
    pub path: RelativePath,
    pub is_directory: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum DeletePlanError {
    #[error(transparent)]
    Journal(#[from] io::Error),

    #[error(transparent)]
    InvalidPath(#[from] InvalidRelativePath),

    #[error(
        "deletion threshold exceeded: {delete_candidates} of {eligible_destination_entries} eligible destination entries exceeds {threshold}%"
    )]
    ThresholdExceeded {
        eligible_destination_entries: u64,
        delete_candidates: u64,
        threshold: u8,
    },

    #[error("delete preflight {0} counter overflow")]
    CounterOverflow(&'static str),

    #[error("delete preflight {0} counter underflow")]
    CounterUnderflow(&'static str),
}

pub type Result<T> = std::result::Result<T, DeletePlanError>;

#[derive(Debug)]
struct CandidateDirectory {
    path: RelativePath,
    protected: bool,
}

/// Exact, bounded-memory delete preflight state for one ordered merge.
///
/// The caller decides whether each destination entry is in deletion scope. An
/// out-of-scope directory implicitly protects its entire destination subtree,
/// matching filtered-directory semantics without requiring a source HashSet.
/// Candidate records are written directly to disk; memory grows only with the
/// current directory nesting depth.
pub struct DeleteTracker {
    policy: DeletePolicy,
    journal: DeleteJournal,
    eligible_destination_entries: u64,
    delete_candidates: u64,
    excluded_subtree: Option<RelativePath>,
    candidate_directories: Vec<CandidateDirectory>,
}

impl DeleteTracker {
    pub async fn new(policy: DeletePolicy) -> Result<Self> {
        Ok(Self {
            policy,
            journal: DeleteJournal::new().await?,
            eligible_destination_entries: 0,
            delete_candidates: 0,
            excluded_subtree: None,
            candidate_directories: Vec::new(),
        })
    }

    /// A source-backed descendant makes any still-open destination-only parent
    /// non-deletable even when the source-side directory entry itself was
    /// filtered before reconciliation.
    pub async fn observe_source_only(&mut self, source: &Entry) -> Result<()> {
        self.close_candidate_directories(&source.path);
        self.protect_candidate_directories().await
    }

    pub async fn observe_matched(&mut self, destination: &Entry, in_scope: bool) -> Result<()> {
        self.close_candidate_directories(&destination.path);
        if self.destination_is_eligible(destination, in_scope) {
            self.eligible_destination_entries =
                checked_add(self.eligible_destination_entries, 1, "eligible destination")?;
        }
        self.protect_candidate_directories().await
    }

    pub async fn observe_destination_only(
        &mut self,
        destination: &Entry,
        in_scope: bool,
    ) -> Result<()> {
        self.close_candidate_directories(&destination.path);
        if self.destination_is_eligible(destination, in_scope) {
            self.eligible_destination_entries =
                checked_add(self.eligible_destination_entries, 1, "eligible destination")?;
            self.delete_candidates = checked_add(self.delete_candidates, 1, "delete candidate")?;
            self.append_candidate(destination).await
        } else {
            self.protect_candidate_directories().await
        }
    }

    pub async fn finish(self) -> Result<DeletePlan> {
        if !self.policy.force
            && self.eligible_destination_entries != 0
            && u128::from(self.delete_candidates) * 100
                > u128::from(self.eligible_destination_entries) * u128::from(self.policy.threshold)
        {
            return Err(DeletePlanError::ThresholdExceeded {
                eligible_destination_entries: self.eligible_destination_entries,
                delete_candidates: self.delete_candidates,
                threshold: self.policy.threshold,
            });
        }

        Ok(DeletePlan {
            eligible_destination_entries: self.eligible_destination_entries,
            delete_candidates: self.delete_candidates,
            replay: DeleteReplay {
                journal: self.journal.seal().await?,
                protected_directories: Vec::new(),
            },
        })
    }

    fn close_candidate_directories(&mut self, current: &RelativePath) {
        while self
            .candidate_directories
            .last()
            .is_some_and(|candidate| !current.as_path().starts_with(candidate.path.as_path()))
        {
            self.candidate_directories.pop();
        }
    }

    fn destination_is_eligible(&mut self, entry: &Entry, in_scope: bool) -> bool {
        if let Some(directory) = self.excluded_subtree.as_ref() {
            if entry.path.as_path().starts_with(directory.as_path()) {
                return false;
            }
            self.excluded_subtree = None;
        }

        if !in_scope {
            if entry.is_directory() {
                self.excluded_subtree = Some(entry.path.clone());
            }
            return false;
        }

        true
    }

    async fn append_candidate(&mut self, entry: &Entry) -> Result<()> {
        if entry.is_directory() {
            self.journal
                .append(entry.path.as_path(), DeleteKind::Directory)
                .await?;
            self.candidate_directories.push(CandidateDirectory {
                path: entry.path.clone(),
                protected: false,
            });
        } else {
            self.journal
                .append(entry.path.as_path(), DeleteKind::FileLike)
                .await?;
        }
        Ok(())
    }

    async fn protect_candidate_directories(&mut self) -> Result<()> {
        for candidate in &mut self.candidate_directories {
            if candidate.protected {
                continue;
            }
            self.journal
                .append(candidate.path.as_path(), DeleteKind::ProtectDirectory)
                .await?;
            candidate.protected = true;
            self.delete_candidates = checked_sub(self.delete_candidates, 1, "delete candidate")?;
        }
        Ok(())
    }
}

pub struct DeletePlan {
    eligible_destination_entries: u64,
    delete_candidates: u64,
    replay: DeleteReplay,
}

impl DeletePlan {
    pub const fn eligible_destination_entries(&self) -> u64 {
        self.eligible_destination_entries
    }

    /// Number of entries that will actually be removed after protection markers
    /// are accounted for, not merely the number initially observed as
    /// destination-only.
    pub const fn delete_candidates(&self) -> u64 {
        self.delete_candidates
    }

    pub fn into_replay(self) -> DeleteReplay {
        self.replay
    }
}

/// Reverse/depth-safe replay of a completed delete plan.
///
/// Protection markers cancel candidate directory removal when a source-backed
/// or excluded descendant was observed during preflight. File-like descendants
/// remain independent candidates. The protection stack is bounded by active
/// directory nesting rather than total tree size.
pub struct DeleteReplay {
    journal: DeleteJournalReader,
    protected_directories: Vec<RelativePath>,
}

impl DeleteReplay {
    pub async fn next_action(&mut self) -> Result<Option<DeleteAction>> {
        while let Some(record) = self.journal.next().await? {
            let path = RelativePath::new(record.path)?;
            match record.kind {
                DeleteKind::ProtectDirectory => {
                    if !self.protected_directories.iter().any(|item| item == &path) {
                        self.protected_directories.push(path);
                    }
                }
                DeleteKind::Directory => {
                    if let Some(index) = self
                        .protected_directories
                        .iter()
                        .rposition(|item| item == &path)
                    {
                        self.protected_directories.swap_remove(index);
                        continue;
                    }
                    return Ok(Some(DeleteAction {
                        path,
                        is_directory: true,
                    }));
                }
                DeleteKind::FileLike => {
                    return Ok(Some(DeleteAction {
                        path,
                        is_directory: false,
                    }));
                }
            }
        }
        Ok(None)
    }
}

fn checked_add(value: u64, increment: u64, counter: &'static str) -> Result<u64> {
    value
        .checked_add(increment)
        .ok_or(DeletePlanError::CounterOverflow(counter))
}

fn checked_sub(value: u64, decrement: u64, counter: &'static str) -> Result<u64> {
    value
        .checked_sub(decrement)
        .ok_or(DeletePlanError::CounterUnderflow(counter))
}

#[cfg(test)]
mod tests {
    use super::super::domain::Timestamp;
    use super::*;
    use std::path::PathBuf;

    fn path(value: &str) -> RelativePath {
        RelativePath::new(PathBuf::from(value)).unwrap()
    }

    fn file(value: &str) -> Entry {
        Entry::file(path(value), 1, Timestamp::UNIX_EPOCH)
    }

    fn directory(value: &str) -> Entry {
        Entry::directory(path(value), Timestamp::UNIX_EPOCH)
    }

    fn policy() -> DeletePolicy {
        DeletePolicy {
            threshold: 100,
            force: false,
        }
    }

    #[tokio::test]
    async fn replays_destination_only_subtree_child_before_parent() {
        let mut tracker = DeleteTracker::new(policy()).await.unwrap();
        tracker
            .observe_destination_only(&directory("parent"), true)
            .await
            .unwrap();
        tracker
            .observe_destination_only(&file("parent/file"), true)
            .await
            .unwrap();
        let plan = tracker.finish().await.unwrap();
        assert_eq!(plan.eligible_destination_entries(), 2);
        assert_eq!(plan.delete_candidates(), 2);

        let mut replay = plan.into_replay();
        assert_eq!(
            replay.next_action().await.unwrap(),
            Some(DeleteAction {
                path: path("parent/file"),
                is_directory: false,
            })
        );
        assert_eq!(
            replay.next_action().await.unwrap(),
            Some(DeleteAction {
                path: path("parent"),
                is_directory: true,
            })
        );
        assert_eq!(replay.next_action().await.unwrap(), None);
    }

    #[tokio::test]
    async fn matched_descendant_protects_parent_and_candidate_count() {
        let mut tracker = DeleteTracker::new(DeletePolicy {
            threshold: 0,
            force: false,
        })
        .await
        .unwrap();
        tracker
            .observe_destination_only(&directory("parent"), true)
            .await
            .unwrap();
        tracker
            .observe_matched(&file("parent/keep"), true)
            .await
            .unwrap();
        let plan = tracker.finish().await.unwrap();
        assert_eq!(plan.delete_candidates(), 0);

        let mut replay = plan.into_replay();
        assert_eq!(replay.next_action().await.unwrap(), None);
    }

    #[tokio::test]
    async fn source_only_descendant_protects_candidate_parent() {
        let mut tracker = DeleteTracker::new(policy()).await.unwrap();
        tracker
            .observe_destination_only(&directory("parent"), true)
            .await
            .unwrap();
        tracker
            .observe_source_only(&file("parent/new"))
            .await
            .unwrap();
        let plan = tracker.finish().await.unwrap();
        assert_eq!(plan.delete_candidates(), 0);

        let mut replay = plan.into_replay();
        assert_eq!(replay.next_action().await.unwrap(), None);
    }

    #[tokio::test]
    async fn excluded_directory_protects_its_subtree_and_candidate_ancestor() {
        let mut tracker = DeleteTracker::new(policy()).await.unwrap();
        tracker
            .observe_destination_only(&directory("parent"), true)
            .await
            .unwrap();
        tracker
            .observe_destination_only(&directory("parent/excluded"), false)
            .await
            .unwrap();
        tracker
            .observe_destination_only(&file("parent/excluded/file"), true)
            .await
            .unwrap();
        let plan = tracker.finish().await.unwrap();
        assert_eq!(plan.eligible_destination_entries(), 1);
        assert_eq!(plan.delete_candidates(), 0);

        let mut replay = plan.into_replay();
        assert_eq!(replay.next_action().await.unwrap(), None);
    }

    #[tokio::test]
    async fn threshold_is_checked_before_replay() {
        let mut tracker = DeleteTracker::new(DeletePolicy {
            threshold: 49,
            force: false,
        })
        .await
        .unwrap();
        tracker
            .observe_destination_only(&file("remove"), true)
            .await
            .unwrap();
        tracker.observe_matched(&file("keep"), true).await.unwrap();

        assert!(matches!(
            tracker.finish().await,
            Err(DeletePlanError::ThresholdExceeded {
                eligible_destination_entries: 2,
                delete_candidates: 1,
                threshold: 49,
            })
        ));
    }

    #[tokio::test]
    async fn force_bypasses_threshold() {
        let mut tracker = DeleteTracker::new(DeletePolicy {
            threshold: 0,
            force: true,
        })
        .await
        .unwrap();
        tracker
            .observe_destination_only(&file("remove"), true)
            .await
            .unwrap();
        assert!(tracker.finish().await.is_ok());
    }
}
