use std::pin::Pin;
use std::task::{Context, Poll};

use futures_core::stream::{FusedStream, Stream};
use futures_lite::stream::{poll_fn as stream_poll_fn, PollFn};

use super::thread::thread_suspend;

/// A convenience method to turn a `Stream` into an `Iterator` which parks the
/// current thread until items are available.
pub fn iter_stream<S>(stream: S) -> IterStream<Pin<Box<S>>>
where
    S: Stream,
{
    IterStream(Some(Box::pin(stream)))
}

/// A convenience method to turn a `Stream` into an `Iterator` which parks the
/// current thread until items are available.
pub fn iter_stream_unpin<S>(stream: S) -> IterStream<S>
where
    S: Stream + Unpin,
{
    IterStream(Some(stream))
}

/// A convenience method to turn a `Stream` into an `Iterator` which parks the
/// current thread until items are available.
pub fn iter_poll_fn<T, F>(poll_fn: F) -> IterStream<PollFn<F>>
where
    F: FnMut(&mut Context) -> Poll<Option<T>>,
{
    IterStream(Some(stream_poll_fn(poll_fn)))
}

pub struct IterStream<S>(Option<S>);

impl<S> IterStream<S> {
    pub fn into_inner(self) -> Option<S> {
        self.0
    }
}

impl<S> Iterator for IterStream<S>
where
    S: Stream + Unpin,
{
    type Item = S::Item;

    fn next(&mut self) -> Option<Self::Item> {
        if let Some(mut stream) = self.0.as_mut() {
            thread_suspend(|cx| Pin::new(&mut stream).poll_next(cx))
        } else {
            None
        }
    }
}

impl<S> Stream for IterStream<S>
where
    S: Stream + Unpin,
{
    type Item = S::Item;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if let Some(mut stream) = self.0.as_mut() {
            Pin::new(&mut stream).poll_next(cx)
        } else {
            Poll::Ready(None)
        }
    }
}

impl<S> FusedStream for IterStream<S>
where
    S: FusedStream + Unpin,
{
    fn is_terminated(&self) -> bool {
        self.0
            .as_ref()
            .map(FusedStream::is_terminated)
            .unwrap_or(true)
    }
}
