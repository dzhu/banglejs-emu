use core::pin::Pin;
use futures_core::future::{FusedFuture, Future};
use futures_core::task::{Context, Poll};
use pin_project_lite::pin_project;
use tokio::task::{JoinError, JoinHandle};

// Copied from `futures::future::OptionFuture`, except when it's `None` it polls
// as `Pending` rather than `Ready(None)`.
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

pin_project! {
    /// A wrapper for a task join handle that separates waiting for the task to
    /// finish and extracting the result.
    #[derive(Debug)]
    #[project = TaskProj]
    #[must_use = "futures do nothing unless you `.await` or poll them"]
    pub enum Task<R> {
        Running {
            #[pin]
            handle: JoinHandle<R>,
        },
        Done { output: Result<R, JoinError> },
    }
}

impl<R> Task<R> {
    pub fn spawn<T>(future: T) -> Self
    where
        T: Future<Output = R> + Send + 'static,
        T::Output: Send + 'static,
    {
        Self::Running {
            handle: tokio::spawn(future),
        }
    }

    pub async fn output(self) -> Result<R, JoinError> {
        match self {
            Task::Running { handle } => handle.await,
            Task::Done { output } => output,
        }
    }
}

impl<R> Future for Task<R> {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match self.as_mut().project() {
            TaskProj::Running { handle } => match handle.poll(cx) {
                Poll::Ready(output) => {
                    *self = Self::Done { output };
                    Poll::Ready(())
                }
                Poll::Pending => Poll::Pending,
            },
            TaskProj::Done { .. } => Poll::Ready(()),
        }
    }
}
