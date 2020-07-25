// Copyright 2020 Province of British Columbia
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// 	http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! This crate provides various utilities for moving between async and
//! synchronous contexts.
//!
//! # Examples
//!
//! The [Task] structure allows a `Future` or a polling function to be
//! evaluated in a blocking manner:
//!
//! ```
//! use suspend::Task;
//! use std::time::Duration;
//!
//! let task = Task::from_future(async { 100 }).map(|val| val * 2);
//! assert_eq!(task.wait_timeout(Duration::from_secs(1)), Ok(200));
//! ```
//!
//! Similarly, the [Iter] structure allows a `Stream` instance to be consumed
//! in an async or blocking manner:
//!
//! ```
//! use suspend::{Iter, block_on};
//!
//! let mut values = Iter::from_iterator(1..);
//! assert_eq!(block_on(async { values.next().await }), Some(1));
//! assert_eq!(values.take(3).collect::<Vec<_>>(), vec![2, 3, 4]);
//! ```
//!
//! The [Suspend] structure may be used to coordinate between threads and
//! `Futures`, allowing either to act as a waiter or notifier:
//!
//! ```
//! use std::time::Duration;
//! use suspend::{Suspend, block_on};
//!
//! let mut susp = Suspend::new();
//! let notifier = susp.notifier();
//!
//! // start listening for notifications
//! let mut listener = susp.listen();
//! // send a notification (satisfies the current listener)
//! notifier.notify();
//! assert_eq!(listener.wait_timeout(Duration::from_secs(1)), true);
//! drop(listener);
//!
//! let mut listener = susp.listen();
//! notifier.notify();
//! block_on(async { listener.await });
//! ```

use std::cell::UnsafeCell;
use std::fmt::{self, Debug, Display, Formatter};
use std::future::Future;
use std::mem::MaybeUninit;
use std::pin::Pin;
use std::sync::{
    atomic::{AtomicU8, Ordering},
    Arc,
};
use std::task::{Context, Poll, Waker};
use std::thread;
use std::time::{Duration, Instant};

use futures_core::{
    future::BoxFuture,
    stream::{BoxStream, Stream},
    // FutureExt, StreamExt,
};
use futures_task::{waker, ArcWake};

#[cfg(feature = "oneshot")]
pub use oneshot_rs as oneshot;

#[cfg(feature = "oneshot")]
/// A `oneshot::Sender` used to provide a single asynchronous result.
/// Requires the `oneshot` feature.
pub use oneshot::Sender;

#[cfg(feature = "oneshot")]
/// An error returned if the associated `oneshot::Sender` is dropped.
/// Requires the `oneshot` feature.
pub use oneshot::RecvError;

const IDLE: u8 = 0x0;
const WAIT: u8 = 0x1;
const LISTEN: u8 = 0x2;

/// A convenience method to evaluate a `Future`, blocking the current thread
/// until it is resolved.
pub fn block_on<F>(fut: F) -> F::Output
where
    F: Future + Send,
{
    Task::from_future(fut).wait()
}

/// A convenience method to turn a `Stream` into an `Iterator` which parks the
/// current thread until items are available.
pub fn iter_stream<'s, S>(stream: S) -> Iter<'s, S::Item>
where
    S: Stream + Send + 's,
{
    Iter::from_stream(stream)
}

/// Create a new single-use `Notifier` and a corresponding `Task`. Once
/// notified, the `Task` will resolve to `()`.
pub fn notify_once<'a>() -> (Notifier, Task<'a, ()>) {
    let inner = Arc::new(Inner::new(WAIT));
    let notifier = Notifier {
        inner: inner.clone(),
    };
    (notifier, Task::from_poll(move |cx| inner.poll(cx.waker())))
}

/// A convenience method to create a new `Task` from a result value.
pub fn ready<'t, T: Send + 't>(result: T) -> Task<'t, T> {
    Task::from_value(result)
}

#[cfg(feature = "oneshot")]
/// Create a new oneshot message pair consisting of a `Sender` and a
/// `Task<T>`. Once `Sender::send` is called or the `Sender` is dropped,
/// the `Task` will resolve to the sent message or a `RecvError`.
/// Requires the `oneshot` feature.
pub fn sender_task<'t, T: Send + 't>() -> (oneshot::Sender<T>, Task<'t, Result<T, RecvError>>) {
    let (sender, receiver) = oneshot::channel();
    (sender, Task::from(receiver))
}

enum InnerNotify {
    Thread(thread::Thread),
    Waker(Waker),
}

enum ClearResult {
    Removed(InnerNotify),
    NoChange,
    Updated,
}

/// A timeout error which may be returned when waiting for a `Suspend` or
/// `Iter` with a given timeout.
#[derive(Debug, PartialEq, Eq)]
pub struct TimeoutError;

impl Display for TimeoutError {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "Timed out")
    }
}

impl std::error::Error for TimeoutError {}

impl ClearResult {
    pub fn is_none(&self) -> bool {
        matches!(self, Self::NoChange | Self::Updated)
    }
}

struct Inner {
    notify: UnsafeCell<MaybeUninit<InnerNotify>>,
    state: AtomicU8,
}

impl Inner {
    pub const fn new(state: u8) -> Self {
        Self {
            notify: UnsafeCell::new(MaybeUninit::uninit()),
            state: AtomicU8::new(state),
        }
    }

    pub fn acquire<'a>(self: &'a Arc<Self>) -> Option<Listener<'a>> {
        if self.state.compare_and_swap(IDLE, WAIT, Ordering::AcqRel) == IDLE {
            Some(Listener { inner: self })
        } else {
            None
        }
    }

    pub fn clear(&self, wait: bool) -> ClearResult {
        let newval = if wait { WAIT } else { IDLE };
        match self.state.swap(newval, Ordering::Release) {
            LISTEN => ClearResult::Removed(unsafe { self.vacate() }),
            state => {
                if state == newval {
                    ClearResult::NoChange
                } else {
                    ClearResult::Updated
                }
            }
        }
    }

    pub fn listen(&self, notify: InnerNotify) -> bool {
        match self.state() {
            IDLE => {
                // not currently acquired
                return false;
            }
            WAIT => (),
            LISTEN => {
                // try to reacquire wait state
                loop {
                    match self.state.compare_exchange_weak(
                        LISTEN,
                        WAIT,
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    ) {
                        Ok(_) => {
                            break unsafe {
                                self.vacate();
                            }
                        }
                        Err(WAIT) => {
                            // competition from another thread?
                            // should not happen with wrappers around this instance
                            panic!("Invalid state");
                        }
                        Err(LISTEN) => {
                            // retry
                        }
                        Err(IDLE) => {
                            // already notified
                            return false;
                        }
                        Err(_) => panic!("Invalid state"),
                    }
                }
            }
            _ => panic!("Invalid state"),
        }
        unsafe {
            self.update(notify);
        }
        match self.state.compare_and_swap(WAIT, LISTEN, Ordering::AcqRel) {
            WAIT => true,
            IDLE => {
                // notify was called while storing
                unsafe {
                    self.vacate();
                }
                false
            }
            _ => panic!("Invalid state"),
        }
    }

    pub fn notify(&self) -> bool {
        match self.clear(false) {
            ClearResult::Removed(InnerNotify::Thread(thread)) => {
                thread.unpark();
                true
            }
            ClearResult::Removed(InnerNotify::Waker(waker)) => {
                waker.wake();
                true
            }
            ClearResult::Updated => true,
            ClearResult::NoChange => false,
        }
    }

    pub fn poll(&self, waker: &Waker) -> Poll<()> {
        match self.state() {
            IDLE => {
                return Poll::Ready(());
            }
            LISTEN => {
                // try to clear existing waker and move back to wait state
                if self.clear(true).is_none() {
                    // already taken (thread was pre-empted)
                    return Poll::Ready(());
                }
            }
            _ => (),
        }

        if self.listen(InnerNotify::Waker(waker.clone())) {
            Poll::Pending
        } else {
            // already notified
            Poll::Ready(())
        }
    }

    pub fn state(&self) -> u8 {
        self.state.load(Ordering::Acquire)
    }

    pub unsafe fn update(&self, notify: InnerNotify) {
        self.notify.get().write(MaybeUninit::new(notify))
    }

    pub unsafe fn vacate(&self) -> InnerNotify {
        self.notify.get().read().assume_init()
    }

    pub fn wait(&self) {
        if !self.listen(InnerNotify::Thread(thread::current())) {
            // already notified
            return;
        }
        loop {
            thread::park();
            if self.state() == IDLE {
                break;
            }
        }
    }

    pub fn wait_deadline(&self, expire: Instant) -> bool {
        if !self.listen(InnerNotify::Thread(thread::current())) {
            // already notified
            return true;
        }
        while let Some(dur) = expire.checked_duration_since(Instant::now()) {
            thread::park_timeout(dur);
            if self.state() == IDLE {
                // notified
                return true;
            }
        }
        // timer expired
        false
    }

    pub fn wait_timeout(&self, timeout: Duration) -> bool {
        self.wait_deadline(Instant::now() + timeout)
    }
}

impl ArcWake for Inner {
    fn wake_by_ref(arc_self: &Arc<Self>) {
        arc_self.notify();
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        // vacate any registered listener
        self.clear(false);
    }
}

unsafe impl Sync for Inner {}

// pub struct NotifyOnce {
//     inner: Inner,
// }

// impl NotifyOnce {
//     pub const fn new() -> Self {
//         Self {
//             inner: Inner {
//                 notify: UnsafeCell::new(MaybeUninit::uninit()),
//                 state: AtomicU8::new(WAIT),
//             },
//         }
//     }

//     pub fn cancel(&self) -> bool {
//         match self.inner.clear(false) {
//             ClearResult::NoChange => false,
//             _ => true,
//         }
//     }

//     pub fn complete(&self) -> bool {
//         self.inner.notify()
//     }

//     pub fn is_complete(&self) -> bool {
//         self.inner.state() == IDLE
//     }

//     pub fn poll(&self, waker: &Waker) -> Poll<()> {
//         self.inner.poll(waker)
//     }

//     pub fn wait(&self) {
//         self.inner.wait()
//     }

//     pub fn wait_deadline(&self, expire: Instant) -> bool {
//         self.inner.wait_deadline(expire)
//     }

//     pub fn wait_timeout(&self, timeout: Duration) -> bool {
//         self.inner.wait_timeout(timeout)
//     }

//     pub fn waker(self: &Arc<Self>) -> Waker {
//         waker(self.clone())
//     }
// }

// impl ArcWake for NotifyOnce {
//     fn wake_by_ref(arc_self: &Arc<Self>) {
//         arc_self.complete();
//     }
// }

// impl Future for NotifyOnce {
//     type Output = ();

//     fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
//         self.inner.poll(cx.waker())
//     }
// }

/// A structure which may be used to suspend a thread or `Future` pending a
/// notification.
pub struct Suspend {
    inner: Arc<Inner>,
}

impl Suspend {
    /// Construct a new `Suspend` instance in the Idle state. To begin
    /// listening for notifications, use the `listen` or `try_listen` methods
    /// to construct a [`Listener`].
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Inner::new(IDLE)),
        }
    }

    /// Construct a new `Notifier` instance associated with this `Suspend`.
    pub fn notifier(&self) -> Notifier {
        Notifier {
            inner: self.inner.clone(),
        }
    }

    /// Directly notify the `Suspend` instance and call any
    /// currently-associated waker.
    pub fn notify(&self) {
        self.inner.notify();
    }

    /// Given a mutable reference to the `Suspend`, start listening for
    /// a notification.
    pub fn listen(&mut self) -> Listener<'_> {
        self.inner.acquire().expect("Invalid Suspend state")
    }

    /// Try to construct a `Listener` and start listening for notifications
    /// when only a read-only reference to the `Suspend` is available.
    pub fn try_listen(&self) -> Option<Listener<'_>> {
        self.inner.acquire()
    }

    /// Construct a new `Waker` associated with the `Suspend` instance.
    pub fn waker(&self) -> Waker {
        waker(self.inner.clone())
    }
}

impl Debug for Suspend {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        let state = match self.inner.state() {
            IDLE => "Idle",
            WAIT => "Waiting",
            LISTEN => "Listening",
            _ => "<!>",
        };
        write!(f, "Suspend({})", state)
    }
}

/// The result of acquiring a `Suspend` using either [`Suspend::listen`] or
/// [`Suspend::try_listen`]. It may be used to wait for a notification with
/// `.await` or by parking the current thread.
pub struct Listener<'a> {
    inner: &'a Arc<Inner>,
}

impl Listener<'_> {
    /// Wait for a notification on the associated `Suspend` instance, parking
    /// the current thread until that time.
    pub fn wait(&mut self) {
        self.inner.wait()
    }

    /// Wait for a notification on the associated `Suspend` instance, parking
    /// the current thread until the result is available or the deadline is
    /// reached. If a timeout occurs then `false` is returned, otherwise
    /// `true`.
    pub fn wait_deadline(&mut self, expire: Instant) -> bool {
        self.inner.wait_deadline(expire)
    }

    /// Wait for a notification on the associated `Suspend` instance, parking
    /// the current thread until the result is available or the timeout
    /// expires. If a timeout does occur then `false` is returned, otherwise
    /// `true`.
    pub fn wait_timeout(&self, timeout: Duration) -> bool {
        self.inner.wait_timeout(timeout)
    }
}

impl Drop for Listener<'_> {
    fn drop(&mut self) {
        self.inner.clear(false);
    }
}

impl<'a> Future for Listener<'a> {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.inner.poll(cx.waker())
    }
}

/// An instance of a notifier for a `Suspend` instance. When notified, the
/// associated thread or `Future` will be woken if currently suspended.
pub struct Notifier {
    inner: Arc<Inner>,
}

impl Notifier {
    /// Notify the associated `Suspend` instance, calling its currently
    /// associated waker, if any.
    pub fn notify(&self) {
        self.inner.notify();
    }

    /// Convert the `Notifier` into a `Waker`.
    pub fn into_waker(self) -> Waker {
        waker(self.inner)
    }
}

impl Clone for Notifier {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

/// A polling function equivalent to `Future::poll`.
pub type PollFn<'a, T> = Box<dyn FnMut(&mut Context) -> Poll<T> + Send + 'a>;

enum TaskState<'t, T> {
    Future(BoxFuture<'t, T>),
    Poll(PollFn<'t, T>),
}

impl<T> TaskState<'_, T> {
    fn poll(&mut self, cx: &mut Context) -> Poll<T> {
        match self {
            Self::Future(fut) => fut.as_mut().poll(cx),
            Self::Poll(poll) => poll(cx),
        }
    }
}

/// An asynchronous result which may be evaluated with `.await`, or by using
/// blocking operations with an optional timeout.
#[must_use = "Task must be awaited"]
pub struct Task<'t, T> {
    state: TaskState<'t, T>,
}

impl<'t, T> Task<'t, T> {
    /// Create a new `Task` from a function returning `Poll<T>`. If the
    /// function is already boxed, then it would be more efficient to use
    /// `Task::from`.
    pub fn from_poll<F>(f: F) -> Self
    where
        F: FnMut(&mut Context) -> Poll<T> + Send + 't,
    {
        Self::from(Box::new(f) as PollFn<'t, T>)
    }

    /// Create a new `Task` from a `Future<T>`. If the future is already boxed,
    /// then it would be more efficient to use `Task::from`.
    pub fn from_future<F>(f: F) -> Self
    where
        F: Future<Output = T> + Send + 't,
    {
        Self {
            state: TaskState::Future(Box::pin(f) as BoxFuture<'t, T>),
        }
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

    /// Map the result of the `Task` using a transformation function.
    pub fn map<'m, F, R>(self, f: F) -> Task<'m, R>
    where
        F: Fn(T) -> R + Send + 'm,
        T: 'm,
        't: 'm,
    {
        let mut state = self.state;
        Task::from_poll(move |cx| state.poll(cx).map(&f))
    }

    /// Resolve the `Task` to its result, parking the current thread until
    /// the result is available.
    pub fn wait(self) -> T {
        if let Ok(result) = self.wait_while(|listen| {
            listen.wait();
            true
        }) {
            result
        } else {
            // should not be possible
            panic!("wait_while returned error");
        }
    }

    /// Resolve the `Task` to its result, parking the current thread until
    /// the result is available or the deadline is reached. If a timeout occurs
    /// then the `Err` result will contain the original `Task`.
    pub fn wait_deadline(self, expire: Instant) -> Result<T, Self> {
        self.wait_while(|listen| listen.wait_deadline(expire))
    }

    /// Resolve the `Task` to its result, parking the current thread until
    /// the result is available or the timeout expires. If a timeout does
    /// occur then the `Err` result will contain the original `Task`.
    pub fn wait_timeout(self, timeout: Duration) -> Result<T, Self> {
        let expire = Instant::now() + timeout;
        self.wait_while(|listen| listen.wait_deadline(expire))
    }

    fn wait_while<F>(mut self, test: F) -> Result<T, Self>
    where
        F: Fn(&mut Listener) -> bool,
    {
        let mut sus = Suspend::new();
        let waker = sus.waker();
        let mut cx = Context::from_waker(&waker);
        let mut listen = sus.listen();
        loop {
            match self.state.poll(&mut cx) {
                Poll::Ready(result) => break Ok(result),
                Poll::Pending => {
                    if !test(&mut listen) {
                        break Err(self);
                    }
                }
            }
        }
    }
}

impl<'t, T, E> Task<'t, Result<T, E>> {
    /// A helper method to map the `Ok(T)` result of the `Task<Result<T, E>>`
    /// using a transformation function.
    pub fn map_ok<'m, F, R>(self, f: F) -> Task<'m, Result<R, E>>
    where
        F: Fn(T) -> R + Send + 'm,
        T: 'm,
        E: 'm,
        't: 'm,
    {
        self.map(move |r| r.map(&f))
    }

    /// A helper method to map the `Err(E)` result of the `Task<Result<T, E>>`
    /// using a transformation function.
    pub fn map_err<'m, F, R>(self, f: F) -> Task<'m, Result<T, R>>
    where
        F: Fn(E) -> R + Send + 'm,
        T: 'm,
        E: 'm,
        't: 'm,
    {
        self.map(move |r| r.map_err(&f))
    }
}

impl<T> Debug for Task<'_, T> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "Task({:p}", self)
    }
}

impl<T> PartialEq for Task<'_, T> {
    fn eq(&self, other: &Self) -> bool {
        std::ptr::eq(self, other)
    }
}

impl<T> Eq for Task<'_, T> {}

impl<'t, T> From<BoxFuture<'t, T>> for Task<'t, T> {
    fn from(fut: BoxFuture<'t, T>) -> Self {
        Self {
            state: TaskState::Future(fut),
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
        self.state.poll(cx)
    }
}

#[cfg(feature = "oneshot")]
impl<'t, T: Send + 't> From<oneshot::Receiver<T>> for Task<'t, Result<T, RecvError>> {
    fn from(mut receiver: oneshot::Receiver<T>) -> Self {
        Task::from_poll(move |cx| Pin::new(&mut receiver).poll(cx))
    }
}

/// A polling function equivalent to `Stream::poll_next`.
pub type PollNextFn<'a, T> = Box<dyn FnMut(&mut Context) -> Poll<Option<T>> + Send + 'a>;

enum IterState<'s, T> {
    Stream(BoxStream<'s, T>),
    PollNext(PollNextFn<'s, T>),
}

impl<'s, T> IterState<'s, T> {
    fn poll_next(&mut self, cx: &mut Context<'_>) -> Poll<Option<T>> {
        match self {
            IterState::Stream(stream) => stream.as_mut().poll_next(cx),
            IterState::PollNext(poll_next) => poll_next(cx),
        }
    }
}

/// A stream which may be polled asynchronously, or by using blocking
/// operations with an optional timeout.
#[must_use = "Iter must be awaited"]
pub struct Iter<'s, T> {
    suspend: Option<Suspend>,
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
    pub fn from_iterator<I>(iter: I) -> Self
    where
        I: IntoIterator<Item = T> + 's,
        <I as IntoIterator>::IntoIter: Send,
    {
        let mut iter = iter.into_iter();
        Self::from_poll_next(move |_cx| Poll::Ready(iter.next()))
    }

    /// Create a new `Iter` from a `Stream<T>`. If the stream is already boxed,
    /// then it would be more efficent to use `Iter::from`.
    pub fn from_stream<S>(s: S) -> Self
    where
        S: Stream<Item = T> + Send + 's,
    {
        Self::from(Box::pin(s) as BoxStream<'s, T>)
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
        self.wait_while(|listen| {
            listen.wait();
            true
        })
        .unwrap()
    }

    /// Resolve the next item in the `Iter` stream, parking the current thread
    /// until the result is available or the deadline is reached. If a timeout
    /// occurs then `Err(TimeoutError)` is returned.
    pub fn wait_next_deadline(&mut self, expire: Instant) -> Result<Option<T>, TimeoutError> {
        self.wait_while(|listen| listen.wait_deadline(expire))
    }

    /// Resolve the next item in the `Iter` stream, parking the current thread
    /// until the result is available or the timeout expires. If a timeout does
    /// occur then `Err(TimeoutError)` is returned.
    pub fn wait_next_timeout(&mut self, timeout: Duration) -> Result<Option<T>, TimeoutError> {
        let expire = Instant::now() + timeout;
        self.wait_while(|listen| listen.wait_deadline(expire))
    }

    fn wait_while<F>(&mut self, test: F) -> Result<Option<T>, TimeoutError>
    where
        F: Fn(&mut Listener) -> bool,
    {
        if self.suspend.is_none() {
            self.suspend.replace(Suspend::new());
        }
        let sus = self.suspend.as_mut().unwrap();
        let waker = sus.waker();
        let mut listener = sus.listen();
        let mut cx = Context::from_waker(&waker);
        loop {
            match self.state.poll_next(&mut cx) {
                Poll::Ready(ret) => break Ok(ret),
                Poll::Pending => {
                    if !test(&mut listener) {
                        break Err(TimeoutError);
                    }
                }
            }
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
            suspend: None,
            state: IterState::Stream(stream),
        }
    }
}

impl<'s, T> From<PollNextFn<'s, T>> for Iter<'s, T> {
    fn from(poll: PollNextFn<'s, T>) -> Self {
        Self {
            suspend: None,
            state: IterState::PollNext(poll),
        }
    }
}

impl<T> Debug for Iter<'_, T> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "Iter({:p}", self)
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

impl<'s, T> Stream for Iter<'s, T> {
    type Item = T;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.state.poll_next(cx)
    }
}

/// A `Future` representing the next item in an [Iter] stream.
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
