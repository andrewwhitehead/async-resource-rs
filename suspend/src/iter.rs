use std::fmt::{self, Debug, Formatter};
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use futures_core::stream::{BoxStream, FusedStream, Stream};

use super::helpers::{block_on, block_on_deadline};
use super::TimedOut;

pub type BoxFusedStream<'t, T> = Pin<Box<dyn FusedStream<Item = T> + Send + 't>>;

/// A convenience method to turn a `Stream` into an `Iterator` which parks the
/// current thread until items are available.
pub fn iter_stream<'s, S>(stream: S) -> Iter<'s, S::Item>
where
    S: Stream + Send + 's,
{
    Iter::from_stream(stream)
}

/// A polling function equivalent to `Stream::poll_next`.
pub type PollNextFn<'a, T> = Box<dyn FnMut(&mut Context) -> Poll<Option<T>> + Send + 'a>;

enum IterState<'s, T> {
    FusedStream(BoxFusedStream<'s, T>),
    Stream(BoxStream<'s, T>),
    PollNext(PollNextFn<'s, T>),
}

impl<'s, T> IterState<'s, T> {
    fn is_terminated(&self) -> bool {
        match self {
            IterState::FusedStream(stream) => stream.is_terminated(),
            _ => false,
        }
    }

    fn poll_next(&mut self, cx: &mut Context<'_>) -> Poll<Option<T>> {
        match self {
            IterState::FusedStream(stream) => stream.as_mut().poll_next(cx),
            IterState::Stream(stream) => stream.as_mut().poll_next(cx),
            IterState::PollNext(poll_next) => poll_next(cx),
        }
    }
}

/// A stream which may be polled asynchronously, or by using blocking
/// operations with an optional timeout.
#[must_use = "Iter must be awaited"]
pub struct Iter<'s, T> {
    state: IterState<'s, T>,
}

impl<'s, T> Iter<'s, T> {
    /// Create a new `Iter` from a function returning `Poll<Option<T>>`. If
    /// the function is already boxed, then it would be more efficent to use
    /// `Iter::from`.
    pub fn from_poll_next<F>(f: F) -> Self
    where
        F: FnMut(&mut Context) -> Poll<Option<T>> + Send + 's,
    {
        Self::from(Box::new(f) as PollNextFn<'s, T>)
    }

    /// Create a new `Iter` from a `Stream<T>`. If the stream is already boxed,
    /// then it would be more efficent to use `Iter::from`.
    pub fn from_iter<I>(iter: I) -> Self
    where
        I: IntoIterator<Item = T> + 's,
        <I as IntoIterator>::IntoIter: Send,
    {
        let mut iter = iter.into_iter();
        Self::from_poll_next(move |_cx| Poll::Ready(iter.next()))
    }

    /// Create a new `Iter` from a `FusedStream<T>`. If the stream is already boxed,
    /// then it would be more efficent to use `Iter::from`.
    pub fn from_fused_stream<S>(s: S) -> Self
    where
        S: FusedStream<Item = T> + Send + 's,
    {
        Self::from(Box::pin(s) as BoxFusedStream<'s, T>)
    }

    /// Create a new `Iter` from a `Stream<T>`. If the stream is already boxed,
    /// then it would be more efficent to use `Iter::from`.
    pub fn from_stream<S>(s: S) -> Self
    where
        S: Stream<Item = T> + Send + 's,
    {
        Self::from(Box::pin(s) as BoxStream<'s, T>)
    }

    /// If the `Iter` is wrapping a `FusedStream`, then this method will return
    /// `true` when the stream should no longer be polled.
    pub fn is_terminated(&self) -> bool {
        self.state.is_terminated()
    }

    /// Map the items of the `Iter` using a transformation function.
    pub fn map<'m, F, R>(self, f: F) -> Iter<'m, R>
    where
        F: Fn(T) -> R + Send + 'm,
        T: 'm,
        's: 'm,
    {
        let mut state = self.state;
        Iter::from_poll_next(move |cx| state.poll_next(cx).map(|opt| opt.map(&f)))
    }

    /// Create a new future which resolves to the next item in the stream.
    pub fn next(&mut self) -> Next<'_, 's, T> {
        Next { iter: self }
    }

    /// Resolve the next item in the `Iter` stream, parking the current thread
    /// until the result is available.
    pub fn wait_next(&mut self) -> Option<T> {
        block_on(self.next())
    }

    /// Resolve the next item in the `Iter` stream, parking the current thread
    /// until the result is available or the deadline is reached. If a timeout
    /// occurs then `Err(TimedOut)` is returned.
    pub fn wait_next_deadline(&mut self, expire: Instant) -> Result<Option<T>, TimedOut> {
        block_on_deadline(self.next(), expire).map_err(|_| TimedOut)
    }

    /// Resolve the next item in the `Iter` stream, parking the current thread
    /// until the result is available or the timeout expires. If a timeout does
    /// occur then `Err(TimedOut)` is returned.
    pub fn wait_next_timeout(&mut self, timeout: Duration) -> Result<Option<T>, TimedOut> {
        match Instant::now().checked_add(timeout) {
            Some(expire) => self.wait_next_deadline(expire),
            None => Err(TimedOut),
        }
    }
}

impl<'t, T, E> Iter<'t, Result<T, E>> {
    /// A helper method to map the `Ok(T)` item of the `Iter<Result<T, E>>`
    /// using a transformation function.
    pub fn map_ok<'m, F, R>(self, f: F) -> Iter<'m, Result<R, E>>
    where
        F: Fn(T) -> R + Send + 'm,
        T: 'm,
        E: 'm,
        't: 'm,
    {
        self.map(move |r| r.map(&f))
    }

    /// A helper method to map the `Err(E)` item of the `Iter<Result<T, E>>`
    /// using a transformation function.
    pub fn map_err<'m, F, R>(self, f: F) -> Iter<'m, Result<T, R>>
    where
        F: Fn(E) -> R + Send + 'm,
        T: 'm,
        E: 'm,
        't: 'm,
    {
        self.map(move |r| r.map_err(&f))
    }
}

impl<'s, T> From<BoxStream<'s, T>> for Iter<'s, T> {
    fn from(stream: BoxStream<'s, T>) -> Self {
        Self {
            state: IterState::Stream(stream),
        }
    }
}

impl<'s, T> From<BoxFusedStream<'s, T>> for Iter<'s, T> {
    fn from(stream: BoxFusedStream<'s, T>) -> Self {
        Self {
            state: IterState::FusedStream(stream),
        }
    }
}

impl<'s, T> From<PollNextFn<'s, T>> for Iter<'s, T> {
    fn from(poll: PollNextFn<'s, T>) -> Self {
        Self {
            state: IterState::PollNext(poll),
        }
    }
}

impl<T> Debug for Iter<'_, T> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "Iter({:p})", self)
    }
}

impl<'s, T> Iterator for Iter<'s, T> {
    type Item = T;

    fn next(&mut self) -> Option<Self::Item> {
        self.wait_next()
    }
}

impl<T> PartialEq for Iter<'_, T> {
    fn eq(&self, other: &Self) -> bool {
        std::ptr::eq(self, other)
    }
}

impl<T> Eq for Iter<'_, T> {}

impl<'s, T> FusedStream for Iter<'s, T> {
    fn is_terminated(&self) -> bool {
        self.is_terminated()
    }
}

impl<'s, T> Stream for Iter<'s, T> {
    type Item = T;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.state.poll_next(cx)
    }
}

/// A `Future` representing the next item in an [`Iter`] stream.
#[derive(Debug)]
pub struct Next<'n, 's, T> {
    iter: &'n mut Iter<'s, T>,
}

impl<'n, 's, T> Future for Next<'n, 's, T> {
    type Output = Option<T>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.iter.state.poll_next(cx)
    }
}
