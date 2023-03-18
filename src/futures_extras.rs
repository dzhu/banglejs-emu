// Copied from `futures::future::OptionFuture`, except when it's `None` it polls
// as `Pending` rather than `Ready(None)`.

use core::pin::Pin;
use futures_core::future::{FusedFuture, Future};
use futures_core::task::{Context, Poll};
use pin_project_lite::pin_project;

pin_project! {
    #[derive(Debug, Clone)]
    #[must_use = "futures do nothing unless you `.await` or poll them"]
    pub struct OptionFuture<F> {
        #[pin]
        inner: Option<F>,
    }
}

impl<F> Default for OptionFuture<F> {
    fn default() -> Self {
        Self { inner: None }
    }
}

impl<F: Future> Future for OptionFuture<F> {
    type Output = F::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match self.project().inner.as_pin_mut() {
            Some(x) => x.poll(cx),
            None => Poll::Pending,
        }
    }
}

impl<F: FusedFuture> FusedFuture for OptionFuture<F> {
    fn is_terminated(&self) -> bool {
        match &self.inner {
            Some(x) => x.is_terminated(),
            None => true,
        }
    }
}

impl<T> From<Option<T>> for OptionFuture<T> {
    fn from(option: Option<T>) -> Self {
        Self { inner: option }
    }
}
