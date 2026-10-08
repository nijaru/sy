use super::{scan_worker, RootedFs, ScanRequest, CHANNEL_CAPACITY};
use crate::engine::domain::Entry;
use crate::engine::reconcile::BoxError;
use futures::Stream;
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

/// Own both the bounded metadata queue and its blocking traversal worker.
/// Exhaustion joins the worker; handlers must also close and join on early exit.
pub(crate) struct RootedEntryStream {
    receiver: mpsc::Receiver<Result<Entry, BoxError>>,
    worker: Option<JoinHandle<()>>,
}

impl RootedEntryStream {
    pub(super) fn spawn(rooted: RootedFs, request: ScanRequest) -> Self {
        let (sender, receiver) = mpsc::channel(CHANNEL_CAPACITY);
        let worker = tokio::task::spawn_blocking(move || scan_worker(rooted, request, sender));
        Self {
            receiver,
            worker: Some(worker),
        }
    }

    /// Wake a backpressured traversal and wait for its scratch/FD owners to drop.
    pub(crate) async fn close(&mut self) -> Result<(), BoxError> {
        self.receiver.close();
        let joined = match self.worker.as_mut() {
            Some(worker) => worker.await.map_err(|error| Box::new(error) as BoxError),
            None => Ok(()),
        };
        self.worker.take();
        // A permit reserved before close can still deliver an entry. Joining
        // first ensures no producer can refill the queue after this drain.
        while self.receiver.try_recv().is_ok() {}
        joined
    }
}

impl Stream for RootedEntryStream {
    type Item = Result<Entry, BoxError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        match this.receiver.poll_recv(cx) {
            Poll::Ready(None) => {}
            other => return other,
        }
        let Some(worker) = this.worker.as_mut() else {
            return Poll::Ready(None);
        };
        let result = std::task::ready!(std::future::Future::poll(Pin::new(worker), cx));
        this.worker.take();
        Poll::Ready(result.err().map(|error| Err(Box::new(error) as BoxError)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;

    #[tokio::test]
    async fn worker_panic_is_an_error_not_successful_scan_completion() {
        let (sender, receiver) = mpsc::channel(1);
        let worker = tokio::task::spawn_blocking(move || {
            drop(sender);
            panic!("injected scanner failure");
        });
        let mut entries = RootedEntryStream {
            receiver,
            worker: Some(worker),
        };
        assert!(entries.next().await.unwrap().is_err());
        assert!(entries.next().await.is_none());
        entries.close().await.unwrap();
    }
}
