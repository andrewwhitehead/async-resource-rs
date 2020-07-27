use std::fmt::{self, Debug, Formatter};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::thread;
use std::time::{Duration, Instant};

use futures_core::future::{BoxFuture, FusedFuture};

use super::core::{InnerSuspend, Notifier};
use super::error::{Incomplete, TimedOut};
use super::helpers::{block_on, block_on_deadline};
use super::local_waker;
use super::oneshot::Channel;

pub use super::oneshot::TaskSender;

/// A polling function equivalent to `Future::poll`.
pub type PollFn<'a, T> = Box<dyn FnMut(&mut Context) -> Poll<T> + Send + 'a>;

pub type BoxCustomTask<'t, T> = Box<dyn CustomTask<T> + Send + 't>;

pub type BoxFusedFuture<'t, T> = Pin<Box<dyn FusedFuture<Output = T> + Send + 't>>;

/// Create a new one-shot message pair consisting of a
/// [`TaskSender<T>`](TaskSender) and a
/// [`Task<Result<T, Incomplete>>`](Task). Once [`TaskSender::send`] is called
/// or the `TaskSender` is dropped, the `Task` will be resolved.
pub fn message_task<'t, T: Send + 'static>() -> (TaskSender<T>, Task<'t, Result<T, Incomplete>>) {
    let (sender, receiver) = Channel::pair();
    (sender, Task::from_custom(receiver))
}

/// Create a new single-use [`Notifier`] and a corresponding [`Task`]. Once
/// notified, the Task will resolve to `()`.
pub fn notify_once<'a>() -> (Notifier, Task<'a, ()>) {
    let inner = Arc::new(InnerSuspend::new_waiting());
    let notifier = Notifier::from(inner.clone());
    (notifier, Task::from_poll(move |cx| inner.poll(cx)))
}

/// A convenience method to create a new [`Task`] from a result value.
pub fn ready<'t, T: Send + 't>(result: T) -> Task<'t, T> {
    Task::from_value(result)
}

/// A trait allowing for custom [`Task`] implementations.
pub trait CustomTask<T> {
    /// Indicate that the consumer of the `Task` is going away. The return
    /// value should be `true` if a subsequent poll operation is guaranteed
    /// to return a `Ready` value.
    fn cancel(&mut self) -> bool {
        false
    }

    /// Equivalent to `FusedFuture::is_terminated`, this method must return
    /// `true` if polling should no longer be attempted.
    fn is_terminated(&self) -> bool {
        false
    }

    /// Poll the instance for a result.
    fn poll(&mut self, cx: &mut Context) -> Poll<T>;

    /// Block on a result.
    fn wait(&mut self) -> T {
        local_waker!(thread_waker, thread::current());
        let mut cx = Context::from_waker(&*thread_waker);
        loop {
            match self.poll(&mut cx) {
                Poll::Pending => thread::park(),
                Poll::Ready(result) => break result,
            }
        }
    }

    /// Block on a result with a deadline.
    fn wait_deadline(&mut self, expire: Instant) -> Result<T, TimedOut> {
        local_waker!(thread_waker, thread::current());
        let mut cx = Context::from_waker(&*thread_waker);
        loop {
            match self.poll(&mut cx) {
                Poll::Ready(result) => break Ok(result),
                Poll::Pending => {
                    if let Some(dur) = expire.checked_duration_since(Instant::now()) {
                        thread::park_timeout(dur);
                    } else {
                        break Err(TimedOut);
                    }
                }
            }
        }
    }
}

enum TaskState<'t, T> {
    Custom(BoxCustomTask<'t, T>),
    Future(BoxFuture<'t, T>),
    FusedFuture(BoxFusedFuture<'t, T>),
    Poll(PollFn<'t, T>),
    Terminated,
}

impl<'t, T> TaskState<'t, T> {
    fn cancel(&mut self) -> bool {
        match self {
            Self::Custom(inner) => inner.cancel(),
            _ => false,
        }
    }

    fn is_terminated(&self) -> bool {
        match self {
            Self::Custom(inner) => inner.is_terminated(),
            Self::FusedFuture(fut) => fut.is_terminated(),
            Self::Terminated => true,
            _ => false,
        }
    }

    fn poll_state(&mut self, cx: &mut Context) -> Poll<T> {
        match self {
            Self::Custom(inner) => inner.poll(cx),
            Self::Future(fut) => fut.as_mut().poll(cx),
            Self::FusedFuture(fut) => fut.as_mut().poll(cx),
            Self::Poll(poll) => poll(cx),
            Self::Terminated => return Poll::Pending,
        }
        .map(|r| {
            *self = Self::Terminated;
            r
        })
    }

    pub fn wait(self) -> T {
        match self {
            Self::Custom(mut inner) => inner.wait(),
            _ => block_on(self),
        }
    }

    pub fn wait_deadline(mut self, expire: Instant) -> Result<T, Self> {
        match &mut self {
            TaskState::Custom(inner) => inner.wait_deadline(expire).map_err(|_| self),
            _ => block_on_deadline(self, expire),
        }
    }
}

impl<T> Future for TaskState<'_, T> {
    type Output = T;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<T> {
        self.poll_state(cx)
    }
}

/// An asynchronous result which may be evaluated with `.await`, or by using
/// blocking operations with an optional timeout.
#[must_use = "Task must be awaited"]
pub struct Task<'t, T> {
    state: TaskState<'t, T>,
}

impl<'t, T> Task<'t, T> {
    /// Create a new `Task` from a `CustomTask<T>`. If the custom task is
    /// already boxed, then it would be more efficient to use `Task::from`.
    pub fn from_custom<C>(task: C) -> Self
    where
        C: CustomTask<T> + Send + 't,
    {
        Self::from(Box::new(task) as BoxCustomTask<'t, T>)
    }

    /// Create a new `Task` from a `Future<T> + FusedFuture`. If the future
    /// is already boxed, then it would be more efficient to use `Task::from`.
    pub fn from_fused_fut<F>(f: F) -> Self
    where
        F: FusedFuture<Output = T> + Send + 't,
    {
        Self::from(Box::pin(f) as BoxFusedFuture<'t, T>)
    }

    /// Create a new `Task` from a `Future<T>`. If the future is already boxed,
    /// then it would be more efficient to use `Task::from`.
    pub fn from_fut<F>(f: F) -> Self
    where
        F: Future<Output = T> + Send + 't,
    {
        Self::from(Box::pin(f) as BoxFuture<'t, T>)
    }

    /// Create a new `Task` from a function returning `Poll<T>`. If the
    /// function is already boxed, then it would be more efficient to use
    /// `Task::from`.
    pub fn from_poll<F>(f: F) -> Self
    where
        F: FnMut(&mut Context) -> Poll<T> + Send + 't,
    {
        Self::from(Box::new(f) as PollFn<'t, T>)
    }

    /// Create a new `Task` from a result value.
    pub fn from_value(result: T) -> Self
    where
        T: Send + 't,
    {
        let mut result = Some(result);
        Self::from_poll(move |_| {
            if let Some(result) = result.take() {
                Poll::Ready(result)
            } else {
                Poll::Pending
            }
        })
    }

    /// In the case of a `CustomTask` implementation, this method can be used
    /// to indicate to the `TaskSender` that the receiver will be dropped. The
    /// next poll or wait on the `Task` should not block if `true` is returned.
    pub fn cancel(&mut self) -> bool {
        self.state.cancel()
    }

    /// Equivalent to `FusedFuture::is_terminated`, this method will return
    /// `true` if polling should no longer be attempted.
    pub fn is_terminated(&mut self) -> bool {
        self.state.is_terminated()
    }

    /// Map the result of the `Task` using a transformation function.
    pub fn map<'m, F, R>(self, f: F) -> Task<'m, R>
    where
        F: Fn(T) -> R + Send + 'm,
        T: Send + 'm,
        't: 'm,
    {
        let mut state = self.state;
        Task::from_poll(move |cx| state.poll_state(cx).map(&f))
    }

    /// Resolve the `Task` to its result, parking the current thread until
    /// the result is available.
    pub fn wait(self) -> T {
        self.state.wait()
    }

    /// Resolve the `Task` to its result, parking the current thread until
    /// the result is available or the deadline is reached. If a timeout occurs
    /// then the `Err` result will contain the original `Task`.
    pub fn wait_deadline(self, expire: Instant) -> Result<T, Self> {
        self.state
            .wait_deadline(expire)
            .map_err(|state| Task { state })
    }

    /// Resolve the `Task` to its result, parking the current thread until
    /// the result is available or the timeout expires. If a timeout does
    /// occur then the `Err` result will contain the original `Task`.
    pub fn wait_timeout(self, timeout: Duration) -> Result<T, Self> {
        match Instant::now().checked_add(timeout) {
            Some(expire) => self.wait_deadline(expire),
            None => Err(self),
        }
    }
}

impl<'t, T, E> Task<'t, Result<T, E>> {
    /// A helper method to map the `Ok(T)` result of the `Task<Result<T, E>>`
    /// using a transformation function.
    pub fn map_ok<'m, F, R>(self, f: F) -> Task<'m, Result<R, E>>
    where
        F: Fn(T) -> R + Send + 'm,
        T: Send + 'm,
        E: Send + 'm,
        't: 'm,
    {
        self.map(move |r| r.map(&f))
    }

    /// A helper method to map the `Err(E)` result of the `Task<Result<T, E>>`
    /// using a transformation function.
    pub fn map_err<'m, F, R>(self, f: F) -> Task<'m, Result<T, R>>
    where
        F: Fn(E) -> R + Send + 'm,
        T: Send + 'm,
        E: Send + 'm,
        't: 'm,
    {
        self.map(move |r| r.map_err(&f))
    }
}

impl<T> Debug for Task<'_, T> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "Task({:p})", self)
    }
}

impl<T> PartialEq for Task<'_, T> {
    fn eq(&self, other: &Self) -> bool {
        std::ptr::eq(self, other)
    }
}

impl<T> Eq for Task<'_, T> {}

impl<'t, T> From<BoxCustomTask<'t, T>> for Task<'t, T> {
    fn from(inner: BoxCustomTask<'t, T>) -> Self {
        Task {
            state: TaskState::Custom(inner),
        }
    }
}

impl<'t, T> From<BoxFuture<'t, T>> for Task<'t, T> {
    fn from(fut: BoxFuture<'t, T>) -> Self {
        Self {
            state: TaskState::Future(fut),
        }
    }
}

impl<'t, T> From<BoxFusedFuture<'t, T>> for Task<'t, T> {
    fn from(fut: BoxFusedFuture<'t, T>) -> Self {
        Self {
            state: TaskState::FusedFuture(fut),
        }
    }
}

impl<'t, T> From<PollFn<'t, T>> for Task<'t, T> {
    fn from(poll: PollFn<'t, T>) -> Self {
        Self {
            state: TaskState::Poll(poll),
        }
    }
}

impl<'t, T> Future for Task<'t, T> {
    type Output = T;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.state.poll_state(cx)
    }
}

impl<'t, T> FusedFuture for Task<'t, T> {
    fn is_terminated(&self) -> bool {
        self.state.is_terminated()
    }
}
