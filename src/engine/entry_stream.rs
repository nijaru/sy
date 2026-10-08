use super::domain::Entry;
use super::reconcile::BoxError;
use futures::{Stream, StreamExt};
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

type Entries = Pin<Box<dyn Stream<Item = Result<Entry, BoxError>> + Send>>;

/// Ordered entries and, for native scans, the worker that produces them.
/// Exhaustion joins the worker. Early-exit owners must explicitly close the
/// stream; Drop closes its queue but cannot await admitted filesystem work.
pub struct EntryStream {
    entries: Option<Entries>,
    worker: Option<JoinHandle<Result<(), BoxError>>>,
}

impl EntryStream {
    pub fn new(stream: impl Stream<Item = Result<Entry, BoxError>> + Send + 'static) -> Self {
        Self {
            entries: Some(Box::pin(stream)),
            worker: None,
        }
    }

    pub(crate) fn spawn_blocking(
        capacity: usize,
        produce: impl FnOnce(mpsc::Sender<Result<Entry, BoxError>>) -> Result<(), BoxError>
            + Send
            + 'static,
    ) -> Self {
        let (sender, receiver) = mpsc::channel(capacity);
        let worker = tokio::task::spawn_blocking(move || produce(sender));
        let stream = futures::stream::unfold(receiver, |mut receiver| async move {
            receiver.recv().await.map(|entry| (entry, receiver))
        });
        Self {
            entries: Some(Box::pin(stream)),
            worker: Some(worker),
        }
    }

    /// Retain worker ownership when selection wraps the underlying queue.
    pub(crate) fn filter_map<F, Fut>(mut self, filter: F) -> Self
    where
        F: FnMut(Result<Entry, BoxError>) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = Option<Result<Entry, BoxError>>> + Send + 'static,
    {
        if let Some(entries) = self.entries.take() {
            self.entries = Some(Box::pin(entries.filter_map(filter)));
        }
        self
    }

    pub(crate) fn stop(&mut self) {
        // Drop the receiver before joining so backpressured producers wake.
        self.entries.take();
    }

    pub async fn close(&mut self) -> Result<(), BoxError> {
        self.stop();
        let joined = match self.worker.as_mut() {
            Some(worker) => worker
                .await
                .map_err(|error| Box::new(error) as BoxError)
                .and_then(|result| result),
            None => Ok(()),
        };
        self.worker.take();
        joined
    }
}

impl Stream for EntryStream {
    type Item = Result<Entry, BoxError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        if let Some(entries) = this.entries.as_mut() {
            match entries.as_mut().poll_next(cx) {
                Poll::Ready(None) => {
                    this.stop();
                }
                other => return other,
            }
        }
        let Some(worker) = this.worker.as_mut() else {
            return Poll::Ready(None);
        };
        let result = std::task::ready!(std::future::Future::poll(Pin::new(worker), cx));
        this.worker.take();
        Poll::Ready(
            result
                .map_err(|error| Box::new(error) as BoxError)
                .and_then(|result| result)
                .err()
                .map(Err),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn cancelled_close_retains_worker_and_reports_its_result() {
        for fail in [false, true] {
            let scratch = tempfile::tempdir().unwrap();
            let scratch_path = scratch.path().to_path_buf();
            let (release, wait) = std::sync::mpsc::channel();
            let mut entries = EntryStream::spawn_blocking(1, move |_sender| {
                wait.recv().unwrap();
                drop(scratch);
                if fail {
                    Err(Box::new(std::io::Error::other("injected cleanup failure")) as BoxError)
                } else {
                    Ok(())
                }
            })
            .filter_map(|entry| std::future::ready(Some(entry)));
            let mut closing = Box::pin(entries.close());
            assert!(futures::poll!(closing.as_mut()).is_pending());
            drop(closing);
            assert!(scratch_path.exists());
            release.send(()).unwrap();
            assert_eq!(entries.close().await.is_err(), fail);
            assert!(!scratch_path.exists());
            assert!(entries.next().await.is_none());
            entries.close().await.unwrap();
        }
    }

    #[tokio::test]
    async fn worker_panic_is_an_error_not_successful_scan_completion() {
        let mut entries = EntryStream::spawn_blocking(1, |sender| {
            drop(sender);
            panic!("injected scanner failure");
        });
        assert!(entries.next().await.unwrap().is_err());
        assert!(entries.next().await.is_none());
        entries.close().await.unwrap();
    }
}
