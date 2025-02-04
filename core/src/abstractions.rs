//! This module contains very generic helpers that can be used codebase-wide

use crate::MetricsContext;
use futures::{stream, Stream, StreamExt};
use std::{
    fmt::{Debug, Formatter},
    future::Future,
    sync::Arc,
};
use tokio::sync::{AcquireError, Notify, OwnedSemaphorePermit, Semaphore, TryAcquireError};

/// Wraps a [Semaphore] with a function call that is fed the available permits any time a permit is
/// acquired or restored through the provided methods
#[derive(Clone)]
pub(crate) struct MeteredSemaphore {
    sem: Arc<Semaphore>,
    metrics_ctx: MetricsContext,
    record_fn: fn(&MetricsContext, usize),
}

impl MeteredSemaphore {
    pub fn new(
        inital_permits: usize,
        metrics_ctx: MetricsContext,
        record_fn: fn(&MetricsContext, usize),
    ) -> Self {
        Self {
            sem: Arc::new(Semaphore::new(inital_permits)),
            metrics_ctx,
            record_fn,
        }
    }

    pub fn available_permits(&self) -> usize {
        self.sem.available_permits()
    }

    pub async fn acquire_owned(&self) -> Result<OwnedMeteredSemPermit, AcquireError> {
        let res = self.sem.clone().acquire_owned().await?;
        self.record();
        Ok(OwnedMeteredSemPermit {
            inner: res,
            record_fn: self.record_drop_owned(),
        })
    }

    pub fn try_acquire_owned(&self) -> Result<OwnedMeteredSemPermit, TryAcquireError> {
        let res = self.sem.clone().try_acquire_owned()?;
        self.record();
        Ok(OwnedMeteredSemPermit {
            inner: res,
            record_fn: self.record_drop_owned(),
        })
    }

    fn record(&self) {
        (self.record_fn)(&self.metrics_ctx, self.sem.available_permits());
    }

    fn record_drop_owned(&self) -> Box<dyn Fn() + Send + Sync> {
        let rcf = self.record_fn;
        let mets = self.metrics_ctx.clone();
        let sem = self.sem.clone();
        Box::new(move || rcf(&mets, sem.available_permits() + 1))
    }
}

/// Wraps an [OwnedSemaphorePermit] to update metrics when it's dropped
pub(crate) struct OwnedMeteredSemPermit {
    inner: OwnedSemaphorePermit,
    record_fn: Box<dyn Fn() + Send + Sync>,
}
impl Drop for OwnedMeteredSemPermit {
    fn drop(&mut self) {
        (self.record_fn)()
    }
}
impl Debug for OwnedMeteredSemPermit {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        self.inner.fmt(f)
    }
}

/// From the input stream, create a new stream which only pulls from the input stream when allowed.
/// When allowed is determined by the passed in `proceeder` which must return a future every time
/// it's called. The input stream is only pulled from when that future resolves.
pub(crate) fn stream_when_allowed<S, F, FF>(
    input: S,
    proceeder: FF,
) -> impl Stream<Item = (S::Item, F::Output)>
where
    S: Stream + Send + 'static,
    F: Future,
    FF: FnMut() -> F,
{
    let acceptable_notify = Arc::new(Notify::new());
    acceptable_notify.notify_one();
    let stream = stream::unfold(
        (proceeder, input.boxed()),
        |(mut proceeder, mut input)| async {
            let v = proceeder().await;
            input.next().await.map(|i| ((i, v), (proceeder, input)))
        },
    );
    stream
}

macro_rules! dbg_panic {
  ($($arg:tt)*) => {
      error!($($arg)*);
      debug_assert!(true, $($arg)*);
  };
}
pub(crate) use dbg_panic;

#[cfg(test)]
mod tests {
    use super::*;
    use futures::pin_mut;
    use std::{cell::RefCell, task::Poll};
    use tokio::sync::mpsc::unbounded_channel;

    // This is fine. Test only / guaranteed to happen serially.
    #[allow(clippy::await_holding_refcell_ref)]
    #[test]
    fn stream_when_allowed_works() {
        let inputs = stream::iter([1, 2, 3]);
        let (allow_tx, allow_rx) = unbounded_channel();
        let allow_rx = RefCell::new(allow_rx);
        let when_allowed = stream_when_allowed(inputs, || async {
            allow_rx.borrow_mut().recv().await.unwrap()
        });

        let waker = futures::task::noop_waker_ref();
        let mut cx = std::task::Context::from_waker(waker);
        pin_mut!(when_allowed);

        allow_tx.send(()).unwrap();
        assert_eq!(
            when_allowed.poll_next_unpin(&mut cx),
            Poll::Ready(Some((1, ())))
        );
        // Now, it won't be ready
        for _ in 1..10 {
            assert_eq!(when_allowed.poll_next_unpin(&mut cx), Poll::Pending);
        }
        allow_tx.send(()).unwrap();
        assert_eq!(
            when_allowed.poll_next_unpin(&mut cx),
            Poll::Ready(Some((2, ())))
        );
        for _ in 1..10 {
            assert_eq!(when_allowed.poll_next_unpin(&mut cx), Poll::Pending);
        }
        allow_tx.send(()).unwrap();
        assert_eq!(
            when_allowed.poll_next_unpin(&mut cx),
            Poll::Ready(Some((3, ())))
        );
        for _ in 1..10 {
            assert_eq!(when_allowed.poll_next_unpin(&mut cx), Poll::Pending);
        }
        allow_tx.send(()).unwrap();
        assert_eq!(when_allowed.poll_next_unpin(&mut cx), Poll::Ready(None));
    }
}
