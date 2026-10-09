use super::domain::{Entry, RelativePath, SyncScope};
use futures::StreamExt;
use std::error::Error as StdError;
use std::fmt;

pub type BoxError = Box<dyn StdError + Send + Sync + 'static>;
pub use super::entry_stream::EntryStream;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Source,
    Destination,
}

impl fmt::Display for Side {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Source => formatter.write_str("source"),
            Self::Destination => formatter.write_str("destination"),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error("{side} entry stream failed")]
    Endpoint {
        side: Side,
        #[source]
        source: BoxError,
    },

    #[error("{side} entry stream is not strictly ordered: {current} followed {previous}")]
    EntryOrder {
        side: Side,
        previous: RelativePath,
        current: RelativePath,
    },

    #[error("both entry streams failed to drain (source: {source_error}; destination: {destination_error})")]
    DrainBoth {
        source_error: BoxError,
        destination_error: BoxError,
    },

    #[error("engine invariant violated: {0}")]
    Invariant(&'static str),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReconcileItem {
    SourceOnly {
        source: Entry,
        destination_path: RelativePath,
    },
    Matched {
        source: Entry,
        destination: Entry,
    },
    DestinationOnly(Entry),
}

struct OrderedInput {
    side: Side,
    stream: EntryStream,
    previous: Option<RelativePath>,
    selected: Option<RelativePath>,
}

impl OrderedInput {
    fn new(side: Side, stream: EntryStream, selected: Option<RelativePath>) -> Self {
        Self {
            side,
            stream,
            previous: None,
            selected,
        }
    }

    async fn next(&mut self) -> Result<Option<Entry>, EngineError> {
        let Some(entry) = self.stream.next().await else {
            return Ok(None);
        };
        let entry = entry.map_err(|source| EngineError::Endpoint {
            side: self.side,
            source,
        })?;

        if self
            .selected
            .as_ref()
            .is_some_and(|selected| *selected != entry.path)
        {
            return Err(EngineError::Invariant("entry outside selected leaf scope"));
        }
        if self.selected.is_some() && self.side == Side::Source && entry.is_directory() {
            return Err(EngineError::Invariant(
                "directory entry requires a tree scope",
            ));
        }
        if let Some(previous) = self.previous.as_ref() {
            if entry.path <= *previous {
                return Err(EngineError::EntryOrder {
                    side: self.side,
                    previous: previous.clone(),
                    current: entry.path,
                });
            }
        }
        self.previous = Some(entry.path.clone());
        Ok(Some(entry))
    }
}

/// Bounded-memory merge join over two strictly ordered entry streams.
///
/// At most one source and one destination entry are retained. Stream ordering is
/// treated as an endpoint/protocol invariant and validated at the trust boundary
/// instead of being assumed by reconciliation.
pub struct OrderedReconciler {
    source: OrderedInput,
    destination: OrderedInput,
    source_head: Option<Entry>,
    destination_head: Option<Entry>,
    source_finished: bool,
    destination_finished: bool,
    scope: SyncScope,
}

impl OrderedReconciler {
    pub fn new(source: EntryStream, destination: EntryStream) -> Self {
        Self::with_scope(source, destination, SyncScope::Tree)
    }

    pub fn with_scope(source: EntryStream, destination: EntryStream, scope: SyncScope) -> Self {
        let (source_path, destination_path) = match &scope {
            SyncScope::Tree => (None, None),
            SyncScope::SelectedLeaf {
                source,
                destination,
            } => (Some(source.clone()), Some(destination.clone())),
        };
        Self {
            source: OrderedInput::new(Side::Source, source, source_path),
            destination: OrderedInput::new(Side::Destination, destination, destination_path),
            source_head: None,
            destination_head: None,
            source_finished: false,
            destination_finished: false,
            scope,
        }
    }

    pub(crate) fn is_selected_leaf(&self) -> bool {
        matches!(self.scope, SyncScope::SelectedLeaf { .. })
    }

    pub async fn next(&mut self) -> Result<Option<ReconcileItem>, EngineError> {
        self.fill_heads().await?;

        match (self.source_head.as_ref(), self.destination_head.as_ref()) {
            (None, None) => Ok(None),
            (Some(_), None) => Ok(self.take_source_only()),
            (None, Some(_)) => Ok(self
                .destination_head
                .take()
                .map(ReconcileItem::DestinationOnly)),
            (Some(source), Some(destination)) => {
                match if matches!(self.scope, SyncScope::SelectedLeaf { .. }) {
                    std::cmp::Ordering::Equal
                } else {
                    source.path.cmp(&destination.path)
                } {
                    std::cmp::Ordering::Less => Ok(self.take_source_only()),
                    std::cmp::Ordering::Greater => Ok(self
                        .destination_head
                        .take()
                        .map(ReconcileItem::DestinationOnly)),
                    std::cmp::Ordering::Equal => {
                        let source = self.source_head.take().ok_or(EngineError::Invariant(
                            "matched source head disappeared during reconciliation",
                        ))?;
                        let destination =
                            self.destination_head.take().ok_or(EngineError::Invariant(
                                "matched destination head disappeared during reconciliation",
                            ))?;
                        Ok(Some(ReconcileItem::Matched {
                            source,
                            destination,
                        }))
                    }
                }
            }
        }
    }

    fn take_source_only(&mut self) -> Option<ReconcileItem> {
        self.source_head.take().map(|source| {
            let destination_path = match &self.scope {
                SyncScope::Tree => source.path.clone(),
                SyncScope::SelectedLeaf { destination, .. } => destination.clone(),
            };
            ReconcileItem::SourceOnly {
                source,
                destination_path,
            }
        })
    }

    /// Stop both queues before awaiting either worker, including on early errors.
    pub async fn close(&mut self) -> Result<(), EngineError> {
        self.source.stream.stop();
        self.destination.stream.stop();
        self.source_head.take();
        self.destination_head.take();
        let source = self.source.stream.close().await;
        let destination = self.destination.stream.close().await;
        match (source, destination) {
            (Ok(()), Ok(())) => Ok(()),
            (Err(source_error), Err(destination_error)) => Err(EngineError::DrainBoth {
                source_error,
                destination_error,
            }),
            (Err(source), _) => Err(EngineError::Endpoint {
                side: Side::Source,
                source,
            }),
            (_, Err(source)) => Err(EngineError::Endpoint {
                side: Side::Destination,
                source,
            }),
        }
    }

    async fn fill_heads(&mut self) -> Result<(), EngineError> {
        if self.source_head.is_none() && !self.source_finished {
            self.source_head = self.source.next().await?;
            self.source_finished = self.source_head.is_none();
        }
        if self.destination_head.is_none() && !self.destination_finished {
            self.destination_head = self.destination.next().await?;
            self.destination_finished = self.destination_head.is_none();
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::domain::Timestamp;
    use futures::stream;

    fn entry(path: &str) -> Entry {
        Entry::file(RelativePath::new(path).unwrap(), 1, Timestamp::UNIX_EPOCH)
    }

    fn entries(paths: &[&str]) -> EntryStream {
        let entries = paths
            .iter()
            .map(|path| Ok::<_, BoxError>(entry(path)))
            .collect::<Vec<_>>();
        EntryStream::new(stream::iter(entries))
    }

    #[tokio::test]
    async fn merge_join_emits_all_three_relationships() {
        let mut reconciler =
            OrderedReconciler::new(entries(&["a", "c", "d"]), entries(&["b", "c", "e"]));

        assert!(matches!(
            reconciler.next().await.unwrap(),
            Some(ReconcileItem::SourceOnly { source: value, .. }) if value.path.as_path() == std::path::Path::new("a")
        ));
        assert!(matches!(
            reconciler.next().await.unwrap(),
            Some(ReconcileItem::DestinationOnly(value)) if value.path.as_path() == std::path::Path::new("b")
        ));
        assert!(matches!(
            reconciler.next().await.unwrap(),
            Some(ReconcileItem::Matched { source, destination })
                if source.path.as_path() == std::path::Path::new("c")
                    && destination.path.as_path() == std::path::Path::new("c")
        ));
        assert!(matches!(
            reconciler.next().await.unwrap(),
            Some(ReconcileItem::SourceOnly { source: value, .. }) if value.path.as_path() == std::path::Path::new("d")
        ));
        assert!(matches!(
            reconciler.next().await.unwrap(),
            Some(ReconcileItem::DestinationOnly(value)) if value.path.as_path() == std::path::Path::new("e")
        ));
        assert!(reconciler.next().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn selected_leaf_matches_logical_keys_without_renaming_physical_entries() {
        let scope = SyncScope::SelectedLeaf {
            source: RelativePath::new("original").unwrap(),
            destination: RelativePath::new("renamed").unwrap(),
        };
        let mut matched = OrderedReconciler::with_scope(
            entries(&["original"]),
            entries(&["renamed"]),
            scope.clone(),
        );
        assert!(
            matches!(matched.next().await.unwrap(), Some(ReconcileItem::Matched { source, destination }) if source.path.as_path() == std::path::Path::new("original") && destination.path.as_path() == std::path::Path::new("renamed"))
        );
        assert!(matched.next().await.unwrap().is_none());
        let mut missing =
            OrderedReconciler::with_scope(entries(&["original"]), entries(&[]), scope.clone());
        assert!(
            matches!(missing.next().await.unwrap(), Some(ReconcileItem::SourceOnly { source, destination_path }) if source.path.as_path() == std::path::Path::new("original") && destination_path.as_path() == std::path::Path::new("renamed"))
        );
        let mut neighbor =
            OrderedReconciler::with_scope(entries(&["sibling"]), entries(&[]), scope.clone());
        assert!(matches!(
            neighbor.next().await,
            Err(EngineError::Invariant(_))
        ));
        let mut duplicates =
            OrderedReconciler::with_scope(entries(&["original", "original"]), entries(&[]), scope);
        duplicates.next().await.unwrap();
        assert!(matches!(
            duplicates.next().await,
            Err(EngineError::EntryOrder { .. })
        ));
    }

    #[tokio::test]
    async fn rejects_duplicate_source_paths() {
        let mut reconciler = OrderedReconciler::new(entries(&["a", "a"]), entries(&[]));
        assert!(matches!(
            reconciler.next().await,
            Ok(Some(ReconcileItem::SourceOnly { .. }))
        ));
        assert!(matches!(
            reconciler.next().await,
            Err(EngineError::EntryOrder {
                side: Side::Source,
                ..
            })
        ));
    }

    #[tokio::test]
    async fn rejects_out_of_order_destination_paths() {
        let mut reconciler = OrderedReconciler::new(entries(&[]), entries(&["b", "a"]));
        assert!(matches!(
            reconciler.next().await,
            Ok(Some(ReconcileItem::DestinationOnly(_)))
        ));
        assert!(matches!(
            reconciler.next().await,
            Err(EngineError::EntryOrder {
                side: Side::Destination,
                ..
            })
        ));
    }

    #[tokio::test]
    async fn attaches_side_to_endpoint_errors() {
        let source_error = std::io::Error::other("scan failed");
        let source = EntryStream::new(stream::iter([Err::<Entry, BoxError>(Box::new(
            source_error,
        ))]));
        let mut reconciler = OrderedReconciler::new(source, entries(&[]));
        assert!(matches!(
            reconciler.next().await,
            Err(EngineError::Endpoint {
                side: Side::Source,
                ..
            })
        ));
    }
}
