use super::domain::{Entry, RelativePath};
use futures::Stream;
use futures::StreamExt;
use std::error::Error as StdError;
use std::fmt;
use std::pin::Pin;

pub type BoxError = Box<dyn StdError + Send + Sync + 'static>;
pub type EntryStream = Pin<Box<dyn Stream<Item = Result<Entry, EngineError>> + Send>>;

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

    #[error("engine invariant violated: {0}")]
    Invariant(&'static str),
}

impl EngineError {
    pub fn endpoint(side: Side, source: impl StdError + Send + Sync + 'static) -> Self {
        Self::Endpoint {
            side,
            source: Box::new(source),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReconcileItem {
    SourceOnly(Entry),
    Matched { source: Entry, destination: Entry },
    DestinationOnly(Entry),
}

struct OrderedInput {
    side: Side,
    stream: EntryStream,
    previous: Option<RelativePath>,
}

impl OrderedInput {
    fn new(side: Side, stream: EntryStream) -> Self {
        Self {
            side,
            stream,
            previous: None,
        }
    }

    async fn next(&mut self) -> Result<Option<Entry>, EngineError> {
        let Some(entry) = self.stream.next().await else {
            return Ok(None);
        };
        let entry = entry?;

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
}

impl OrderedReconciler {
    pub fn new(source: EntryStream, destination: EntryStream) -> Self {
        Self {
            source: OrderedInput::new(Side::Source, source),
            destination: OrderedInput::new(Side::Destination, destination),
            source_head: None,
            destination_head: None,
            source_finished: false,
            destination_finished: false,
        }
    }

    pub async fn next(&mut self) -> Result<Option<ReconcileItem>, EngineError> {
        self.fill_heads().await?;

        match (self.source_head.as_ref(), self.destination_head.as_ref()) {
            (None, None) => Ok(None),
            (Some(_), None) => Ok(self.source_head.take().map(ReconcileItem::SourceOnly)),
            (None, Some(_)) => Ok(self
                .destination_head
                .take()
                .map(ReconcileItem::DestinationOnly)),
            (Some(source), Some(destination)) => match source.path.cmp(&destination.path) {
                std::cmp::Ordering::Less => {
                    Ok(self.source_head.take().map(ReconcileItem::SourceOnly))
                }
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
            },
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
        let entries = paths.iter().map(|path| Ok(entry(path))).collect::<Vec<_>>();
        Box::pin(stream::iter(entries))
    }

    #[tokio::test]
    async fn merge_join_emits_all_three_relationships() {
        let mut reconciler =
            OrderedReconciler::new(entries(&["a", "c", "d"]), entries(&["b", "c", "e"]));

        assert!(matches!(
            reconciler.next().await.unwrap(),
            Some(ReconcileItem::SourceOnly(value)) if value.path.as_path() == std::path::Path::new("a")
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
            Some(ReconcileItem::SourceOnly(value)) if value.path.as_path() == std::path::Path::new("d")
        ));
        assert!(matches!(
            reconciler.next().await.unwrap(),
            Some(ReconcileItem::DestinationOnly(value)) if value.path.as_path() == std::path::Path::new("e")
        ));
        assert!(reconciler.next().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn rejects_duplicate_source_paths() {
        let mut reconciler = OrderedReconciler::new(entries(&["a", "a"]), entries(&[]));
        assert!(matches!(
            reconciler.next().await,
            Ok(Some(ReconcileItem::SourceOnly(_)))
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
}
