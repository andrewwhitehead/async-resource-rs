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
//! The [`Task`] structure allows a `Future` or a polling function to be
//! evaluated in a blocking manner, as well as transform the result to produce
//! a new Task:
//!
//! ```
//! let task = suspend::Task::from_fut(async { 100 }).map(|val| val * 2);
//! assert_eq!(task.wait(), 200);
//! ```
//!
//! Similarly, the [`Iter`] structure allows a `Stream` instance to be consumed
//! in an async or blocking manner:
//!
//! ```
//! let mut values = suspend::Iter::from_iter(1..);
//! assert_eq!(suspend::block_on(async { values.next().await }), Some(1));
//! assert_eq!(values.take(3).collect::<Vec<_>>(), vec![2, 3, 4]);
//! ```
//!
//! The [`Suspend`] structure may be used to coordinate between threads and
//! `Futures`, allowing either to act as a waiter or notifier:
//!
//! ```
//! use std::time::Duration;
//!
//! let mut susp = suspend::Suspend::new();
//! let notifier = susp.notifier();
//!
//! // start listening for notifications
//! let mut listener = susp.listen();
//! // send a notification (satisfies the current listener)
//! notifier.notify();
//! // wait for notification (already sent) with a timeout
//! assert_eq!(listener.wait_timeout(Duration::from_secs(1)), true);
//! drop(listener);
//!
//! let mut listener = susp.listen();
//! notifier.notify();
//! // the listener is also a Future
//! suspend::block_on(async { listener.await });
//! ```
//!
//! It can also be used to directly poll a `Future`:
//!
//! ```
//! let mut susp = suspend::Suspend::new();
//! let mut task = suspend::Task::from_fut(async { 99 });
//! assert_eq!(susp.poll_unpin(&mut task), Ok(99));
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
    future::{BoxFuture, FusedFuture},
    stream::{BoxStream, Stream},
};
use futures_task::{waker, waker_ref, ArcWake, WakerRef};
use pin_utils::pin_mut;

#[macro_use]
mod waker;
pub use waker::{current_thread_waker, thread_waker, LocalWaker};

#[cfg(feature = "test_clone_waker")]
pub use waker::{waker_from, CloneWake};

const IDLE: u8 = 0x0;
const WAIT: u8 = 0x1;
const LISTEN: u8 = 0x2;
const CLOSED: u8 = 0x3;

/// A convenience method to evaluate a `Future`, blocking the current thread
/// until it is resolved.
pub fn block_on<F>(fut: F) -> F::Output
where
    F: Future,
{
    pin_mut!(fut);
    local_waker!(thread_waker, thread::current());
    let mut cx = Context::from_waker(&*thread_waker);
    loop {
        match fut.as_mut().poll(&mut cx) {
            Poll::Ready(result) => break result,
            Poll::Pending => thread::park(),
        }
    }
}

/// A convenience method to evaluate a `Future`, blocking the current thread
/// until it is resolved or the timeout expires.
pub fn block_on_deadline<F>(mut fut: F, expire: Instant) -> Result<F::Output, F>
where
    F: Future + Unpin,
{
    let mut pin_fut = Pin::new(&mut fut);
    local_waker!(thread_waker, thread::current());
    let mut cx = Context::from_waker(&*thread_waker);
    loop {
        match pin_fut.as_mut().poll(&mut cx) {
            Poll::Ready(result) => break Ok(result),
            Poll::Pending => {
                if let Some(dur) = expire.checked_duration_since(Instant::now()) {
                    thread::park_timeout(dur);
                } else {
                    break Err(fut);
                }
            }
        }
    }
}

/// Create a new one-shot message pair consisting of a
/// [`TaskSender<T>`](TaskSender) and a
/// [`Task<Result<T, Incomplete>`](Task). Once [`TaskSender::send`] is called
/// or the `TaskSender` is dropped, the `Task` will be resolved.
pub fn channel<'t, T: Send + 'static>() -> (TaskSender<T>, Task<'t, Result<T, Incomplete>>) {
    let channel = Arc::new(Channel::new());
    (
        TaskSender {
            channel: channel.clone(),
        },
        Task::from_custom(TaskReceiver { channel }),
    )
}

/// A convenience method to turn a `Stream` into an `Iterator` which parks the
/// current thread until items are available.
pub fn iter_stream<'s, S>(stream: S) -> Iter<'s, S::Item>
where
    S: Stream + Send + 's,
{
    Iter::from_stream(stream)
}

/// Create a new single-use [`Notifier`] and a corresponding [`Task`]. Once
/// notified, the Task will resolve to `()`.
pub fn notify_once<'a>() -> (Notifier, Task<'a, ()>) {
    let inner = Arc::new(InnerSuspend::new(WAIT));
    let notifier = Notifier {
        inner: inner.clone(),
    };
    (notifier, Task::from_poll(move |cx| inner.poll(cx)))
}

/// A convenience method to create a new `Task` from a result value.
pub fn ready<'t, T: Send + 't>(result: T) -> Task<'t, T> {
    Task::from_value(result)
}

/// An error indicating that the [`TaskSender`] side of a one-shot channel was
/// dropped without sending a result.
#[derive(Debug, PartialEq, Eq, Hash)]
pub struct Incomplete;

impl Display for Incomplete {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "Incomplete")
    }
}

impl std::error::Error for Incomplete {}

/// A timeout error which may be returned when waiting for a [`Listener`],
/// [`Task`] or [`Iter`] with a given expiry time.
#[derive(Debug, PartialEq, Eq, Hash)]
pub struct TimedOut;

impl Display for TimedOut {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "Timed out")
    }
}

impl std::error::Error for TimedOut {}

enum ClearResult {
    Removed(Waker),
    NoChange,
    Updated,
}

impl ClearResult {
    pub fn is_none(&self) -> bool {
        matches!(self, Self::NoChange | Self::Updated)
    }
}

struct InnerSuspend {
    state: AtomicU8,
    waker: UnsafeCell<MaybeUninit<Waker>>,
}

impl InnerSuspend {
    pub const fn new(state: u8) -> Self {
        Self {
            state: AtomicU8::new(state),
            waker: UnsafeCell::new(MaybeUninit::uninit()),
        }
    }

    pub fn acquire(&self) -> bool {
        if self.state.compare_and_swap(IDLE, WAIT, Ordering::AcqRel) == IDLE {
            true
        } else {
            false
        }
    }

    pub fn clear(&self, newval: u8) -> ClearResult {
        match self.state.swap(newval, Ordering::Release) {
            LISTEN => ClearResult::Removed(unsafe { self.vacate_waker() }),
            state => {
                if state == newval {
                    ClearResult::NoChange
                } else {
                    ClearResult::Updated
                }
            }
        }
    }

    // pub fn close(&self) -> bool {
    //     match self.clear(CLOSED) {
    //         ClearResult::Removed(waker) => {
    //             waker.wake();
    //             true
    //         }
    //         ClearResult::Updated => true,
    //         ClearResult::NoChange => false,
    //     }
    // }

    pub fn is_waiting(&self) -> bool {
        let s = self.state();
        s == WAIT || s == LISTEN
    }

    /// Try to start listening with a new waker
    pub fn listen(&self, waker: &Waker) -> bool {
        match self.state() {
            IDLE | CLOSED => {
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
                                self.vacate_waker();
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
                        Err(IDLE) | Err(CLOSED) => {
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
            self.update_waker(waker.clone());
        }
        match self.state.compare_and_swap(WAIT, LISTEN, Ordering::AcqRel) {
            WAIT => true,
            IDLE => {
                // notify was called while storing
                unsafe {
                    self.vacate_waker();
                }
                false
            }
            CLOSED => false,
            _ => panic!("Invalid state"),
        }
    }

    /// Clear the state if it is waiting, and call the notifier if any.
    /// Returns true if there was an active listener.
    pub fn notify(&self) -> bool {
        match self.clear(IDLE) {
            ClearResult::Removed(waker) => {
                waker.wake();
                true
            }
            ClearResult::Updated => true,
            ClearResult::NoChange => false,
        }
    }

    pub fn poll(&self, cx: &mut Context) -> Poll<()> {
        match self.state() {
            IDLE | CLOSED => {
                return Poll::Ready(());
            }
            LISTEN => {
                // try to clear existing waker and move back to wait state
                if self.clear(WAIT).is_none() {
                    // already taken (thread was pre-empted)
                    return Poll::Ready(());
                }
            }
            _ => (),
        }

        if self.listen(cx.waker()) {
            Poll::Pending
        } else {
            // already notified
            Poll::Ready(())
        }
    }

    pub fn state(&self) -> u8 {
        self.state.load(Ordering::Acquire)
    }

    pub unsafe fn update_waker(&self, waker: Waker) {
        self.waker.get().write(MaybeUninit::new(waker))
    }

    pub unsafe fn vacate_waker(&self) -> Waker {
        self.waker.get().read().assume_init()
    }

    pub fn wait(&self) {
        local_waker!(thread_waker, thread::current());
        if !self.listen(&*thread_waker) {
            // already notified
            return;
        }
        loop {
            thread::park();
            let s = self.state();
            if s == IDLE || s == CLOSED {
                break;
            }
        }
    }

    pub fn wait_deadline(&self, expire: Instant) -> bool {
        local_waker!(thread_waker, thread::current());
        if !self.listen(&*thread_waker) {
            // already notified
            return true;
        }
        while let Some(dur) = expire.checked_duration_since(Instant::now()) {
            thread::park_timeout(dur);
            let s = self.state();
            if s == IDLE || s == CLOSED {
                // notified
                return true;
            }
        }
        // timer expired
        false
    }

    pub fn wait_timeout(&self, timeout: Duration) -> bool {
        Instant::now()
            .checked_add(timeout)
            .map(|expire| self.wait_deadline(expire))
            .unwrap_or(false)
    }

    pub fn waker_ref<'a>(self: &'a Arc<Self>) -> WakerRef<'a> {
        waker_ref(self)
    }
}

impl ArcWake for InnerSuspend {
    fn wake_by_ref(arc_self: &Arc<Self>) {
        arc_self.notify();
    }
}

impl Drop for InnerSuspend {
    fn drop(&mut self) {
        // vacate any registered listener
        self.clear(IDLE);
    }
}

unsafe impl Sync for InnerSuspend {}

/// A structure which may be used to suspend a thread or `Future` pending a
/// notification.
pub struct Suspend {
    inner: Arc<InnerSuspend>,
}

impl Suspend {
    /// Construct a new `Suspend` instance in the Idle state. To begin
    /// listening for notifications, use the `listen` or `try_listen` methods
    /// to construct a [`Listener`].
    pub fn new() -> Self {
        Self {
            inner: Arc::new(InnerSuspend::new(IDLE)),
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
        self.try_listen().expect("Invalid Suspend state")
    }

    /// Poll a `Future`, returning the result if ready, otherwise a `Listener`
    /// instance which will be notified when the future should be polled again.
    pub fn poll<F>(&mut self, fut: Pin<&mut F>) -> Result<F::Output, Listener<'_>>
    where
        F: Future,
    {
        let mut listener = self.listen();
        match listener.poll(fut) {
            Poll::Ready(r) => Ok(r),
            Poll::Pending => Err(listener),
        }
    }

    /// A convenience method to poll a `Future + Unpin`.
    pub fn poll_unpin<F>(&mut self, fut: &mut F) -> Result<F::Output, Listener<'_>>
    where
        F: Future + Unpin,
    {
        self.poll(Pin::new(fut))
    }

    /// Try to construct a `Listener` and start listening for notifications
    /// when only a read-only reference to the `Suspend` is available.
    pub fn try_listen(&self) -> Option<Listener<'_>> {
        if self.inner.acquire() {
            Some(Listener { inner: &self.inner })
        } else {
            None
        }
    }

    /// Block on the result of a `Future`. When evaluating multiple `Future`s,
    /// it is more efficient to create one `Suspend` and use this method than
    /// to create a separate `Task` for each one.
    pub fn wait_on<F>(&mut self, fut: F) -> F::Output
    where
        F: Future,
    {
        pin_mut!(fut);
        loop {
            match self.poll(fut.as_mut()) {
                Ok(result) => break result,
                Err(mut listen) => listen.wait(),
            }
        }
    }

    /// A convenience method to block on the result of a `Future` with a
    /// deadline. If the deadline is reached before it is resolved, the
    /// original `Future` is returned as the error value.
    pub fn wait_on_deadline<F>(&mut self, mut fut: F, expire: Instant) -> Result<F::Output, F>
    where
        F: Future + Unpin,
    {
        loop {
            match self.poll_unpin(&mut fut) {
                Ok(result) => break Ok(result),
                Err(mut listen) => {
                    if !listen.wait_deadline(expire) {
                        break Err(fut);
                    }
                }
            }
        }
    }

    /// A convenience method to block on the result of a `Future` with a
    /// timeout. If the timeout occurs before it is resolved, the original
    /// `Future` is returned as the error value.
    pub fn wait_on_timeout<F>(&mut self, fut: F, timeout: Duration) -> Result<F::Output, F>
    where
        F: Future + Unpin,
    {
        match Instant::now().checked_add(timeout) {
            Some(expire) => self.wait_on_deadline(fut, expire),
            None => Err(fut),
        }
    }

    /// Get a `WakerRef` associated with the `Suspend` instance.
    /// The resulting instance can be cloned to obtain an owned `Waker`.
    pub fn waker_ref(&self) -> WakerRef {
        self.inner.waker_ref()
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

impl Default for Suspend {
    fn default() -> Self {
        Self::new()
    }
}

/// The result of acquiring a [`Suspend`] using either [`Suspend::listen`] or
/// [`Suspend::try_listen`]. It may be used to wait for a notification with
/// `.await` or by parking the current thread.
pub struct Listener<'a> {
    inner: &'a Arc<InnerSuspend>,
}

impl Listener<'_> {
    /// Check if the listener has already been notified.
    pub fn is_notified(&self) -> bool {
        self.inner.state() == IDLE
    }

    /// Poll a `Future`, which will then notify this listener when ready.
    pub fn poll<F>(&mut self, fut: Pin<&mut F>) -> Poll<F::Output>
    where
        F: Future,
    {
        let waker = self.inner.waker_ref();
        let mut cx = Context::from_waker(&*waker);
        fut.poll(&mut cx)
    }

    /// A convenience method to poll a `Future + Unpin`.
    pub fn poll_unpin<F>(&mut self, fut: &mut F) -> Poll<F::Output>
    where
        F: Future + Unpin,
    {
        self.poll(Pin::new(fut))
    }

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

    /// Get a `WakerRef` associated with the `Listener`.
    /// The resulting instance can be cloned to obtain an owned `Waker`.
    pub fn waker_ref(&self) -> WakerRef {
        self.inner.waker_ref()
    }
}

impl Debug for Listener<'_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        let state = match self.inner.state() {
            IDLE => "Notified",
            WAIT => "Waiting",
            LISTEN => "Polled",
            _ => "<!>",
        };
        write!(f, "Listener({})", state)
    }
}

impl Drop for Listener<'_> {
    fn drop(&mut self) {
        self.inner.clear(IDLE);
    }
}

impl Future for Listener<'_> {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.inner.poll(cx)
    }
}

impl PartialEq for Listener<'_> {
    fn eq(&self, other: &Self) -> bool {
        std::ptr::eq(self, other)
    }
}

impl Eq for Listener<'_> {}

/// An instance of a notifier for a `Suspend` instance. When notified, the
/// associated thread or `Future` will be woken if currently suspended.
pub struct Notifier {
    inner: Arc<InnerSuspend>,
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

pub trait CustomTask<T> {
    fn cancel(&mut self) {}

    fn is_terminated(&self) -> bool {
        false
    }

    fn poll(&mut self, cx: &mut Context) -> Poll<T>;

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

pub type BoxCustomTask<'t, T> = Box<dyn CustomTask<T> + Send + 't>;

pub type BoxFusedFuture<'t, T> = Pin<Box<dyn FusedFuture<Output = T> + Send + 't>>;

enum TaskState<'t, T> {
    Custom(BoxCustomTask<'t, T>),
    Future(BoxFuture<'t, T>),
    FusedFuture(BoxFusedFuture<'t, T>),
    Poll(PollFn<'t, T>),
    Done,
}

impl<'t, T> TaskState<'t, T> {
    fn cancel(&mut self) {
        match self {
            Self::Custom(inner) => inner.cancel(),
            _ => (),
        }
    }

    fn is_terminated(&self) -> bool {
        match self {
            Self::Custom(inner) => inner.is_terminated(),
            Self::FusedFuture(fut) => fut.is_terminated(),
            Self::Done => true,
            _ => false,
        }
    }

    fn poll_state(&mut self, cx: &mut Context) -> Poll<T> {
        let result = match self {
            Self::Custom(inner) => inner.poll(cx),
            Self::Future(fut) => fut.as_mut().poll(cx),
            Self::FusedFuture(fut) => fut.as_mut().poll(cx),
            Self::Poll(poll) => poll(cx),
            Self::Done => return Poll::Pending,
        };
        result.map(|r| {
            *self = Self::Done;
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

    /// Create a new `Task` from a function returning `Poll<T>`. If the
    /// function is already boxed, then it would be more efficient to use
    /// `Task::from`.
    pub fn from_poll<F>(f: F) -> Self
    where
        F: FnMut(&mut Context) -> Poll<T> + Send + 't,
    {
        Self::from(Box::new(f) as PollFn<'t, T>)
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

    /// In the case of a one-shot message task, this method can be used to
    /// indicate to the `TaskSender` that the receiver will be dropped. The
    /// next poll or wait on the `Task` will not block.
    pub fn cancel(&mut self) {
        self.state.cancel();
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

struct Channel<T> {
    data: UnsafeCell<MaybeUninit<T>>,
    state: InnerSuspend,
}

unsafe impl<T: Send> Send for Channel<T> {}
unsafe impl<T: Send> Sync for Channel<T> {}

impl<T> Channel<T> {
    pub const fn new() -> Self {
        Self {
            data: UnsafeCell::new(MaybeUninit::uninit()),
            state: InnerSuspend::new(WAIT),
        }
    }

    pub fn close_recv(&self) -> bool {
        match self.state.clear(IDLE) {
            ClearResult::NoChange => false,
            _ => true,
        }
    }

    pub fn poll(&self, cx: &mut Context) -> Poll<T> {
        self.state.poll(cx).map(|_| unsafe { self.take(true) })
    }

    pub fn send(&self, value: T) -> Result<(), T> {
        if self.state.is_waiting() {
            unsafe {
                self.data.get().write(MaybeUninit::new(value));
            }
            if self.state.notify() {
                Ok(())
            } else {
                // receiver was dropped while sender was pre-empted
                Err(unsafe { self.take(true) })
            }
        } else {
            Err(value)
        }
    }

    pub unsafe fn take(&self, reset: bool) -> T {
        let result = self.data.get().read().assume_init();
        if reset {
            self.state.acquire();
        }
        result
    }

    pub fn wait(&self) -> T {
        self.state.wait();
        unsafe { self.take(true) }
    }

    pub fn wait_deadline(&self, expire: Instant) -> Result<T, TimedOut> {
        if self.state.wait_deadline(expire) {
            Ok(unsafe { self.take(true) })
        } else {
            Err(TimedOut)
        }
    }
}

/// Created by [`channel`] and used to dispatch a single message to a
/// receiving [`Task`].
pub struct TaskSender<T> {
    channel: Arc<Channel<Result<T, Incomplete>>>,
}

impl<T> TaskSender<T> {
    /// Dispatch the result and consume the `TaskSender`.
    pub fn send(self, value: T) -> Result<(), T> {
        let result = self.channel.send(Ok(value));
        std::mem::forget(self); // skip destructor
        result.map_err(|r| r.unwrap())
    }
}

impl<T> Drop for TaskSender<T> {
    fn drop(&mut self) {
        self.channel.send(Err(Incomplete)).unwrap_or(());
    }
}

struct TaskReceiver<T> {
    channel: Arc<Channel<T>>,
}

impl<T: Send> CustomTask<T> for TaskReceiver<T> {
    fn cancel(&mut self) {
        self.channel.close_recv();
    }

    fn poll(&mut self, cx: &mut Context) -> Poll<T> {
        self.channel.poll(cx)
    }

    fn wait(&mut self) -> T {
        self.channel.wait()
    }

    fn wait_deadline(&mut self, expire: Instant) -> Result<T, TimedOut> {
        self.channel.wait_deadline(expire)
    }
}

impl<T> Drop for TaskReceiver<T> {
    fn drop(&mut self) {
        if self.channel.state.is_waiting() {
            self.channel.close_recv();
        } else {
            unsafe {
                self.channel.take(false);
            }
        }
    }
}
