use std::cell::UnsafeCell;
use std::fmt::{self, Debug, Formatter};
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

use pin_utils::pin_mut;

use super::waker::{waker_from, waker_ref, ArcWake, WakerRef, THREAD_WAKER};

const IDLE: u8 = 0x0;
const WAIT: u8 = 0x1;
const LISTEN: u8 = 0x2;
const CLOSED: u8 = 0x3;

pub(crate) enum ClearResult {
    Removed(Waker),
    NoChange,
    Updated(u8),
}

impl ClearResult {
    pub fn is_removed(&self) -> bool {
        matches!(self, Self::Removed(_))
    }

    pub fn is_updated(&self) -> bool {
        matches!(self, Self::Updated(_) | Self::Removed(_))
    }
}

pub(crate) struct InnerSuspend {
    state: AtomicU8,
    waker: UnsafeCell<MaybeUninit<Waker>>,
}

impl InnerSuspend {
    const fn new(state: u8) -> Self {
        Self {
            state: AtomicU8::new(state),
            waker: UnsafeCell::new(MaybeUninit::uninit()),
        }
    }

    pub const fn new_waiting() -> Self {
        Self::new(WAIT)
    }

    pub fn acquire(&self) -> bool {
        if self.state.compare_and_swap(IDLE, WAIT, Ordering::AcqRel) == IDLE {
            true
        } else {
            false
        }
    }

    fn clear(&self, newval: u8) -> ClearResult {
        match self.state.swap(newval, Ordering::Release) {
            LISTEN => ClearResult::Removed(unsafe { self.vacate_waker() }),
            state => {
                if state == newval {
                    ClearResult::NoChange
                } else {
                    ClearResult::Updated(state)
                }
            }
        }
    }

    pub fn close(&self) -> ClearResult {
        self.clear(CLOSED)
    }

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
        match self.set_idle() {
            ClearResult::Removed(waker) => {
                waker.wake();
                true
            }
            ClearResult::Updated(_) => true,
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
                if !self.set_waiting().is_removed() {
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

    pub fn set_idle(&self) -> ClearResult {
        self.clear(IDLE)
    }

    pub fn set_waiting(&self) -> ClearResult {
        self.clear(WAIT)
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
        if !THREAD_WAKER.with(|waker| self.listen(waker)) {
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
        if !THREAD_WAKER.with(|waker| self.listen(waker)) {
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
        self.set_idle();
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
                Err(listen) => listen.wait(),
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
                Err(listen) => {
                    if listen.wait_deadline(expire).is_err() {
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
    pub fn wait(self) {
        self.inner.wait()
    }

    /// Wait for a notification on the associated `Suspend` instance, parking
    /// the current thread until the result is available or the deadline is
    /// reached. If a timeout occurs then `false` is returned, otherwise
    /// `true`.
    pub fn wait_deadline(self, expire: Instant) -> Result<(), Self> {
        if self.inner.wait_deadline(expire) {
            Ok(())
        } else {
            Err(self)
        }
    }

    /// Wait for a notification on the associated `Suspend` instance, parking
    /// the current thread until the result is available or the timeout
    /// expires. If a timeout does occur then `false` is returned, otherwise
    /// `true`.
    pub fn wait_timeout(self, timeout: Duration) -> Result<(), Self> {
        if self.inner.wait_timeout(timeout) {
            Ok(())
        } else {
            Err(self)
        }
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
        self.inner.set_idle();
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

/// An instance of a notifier for a [`Suspend`] instance. When notified, the
/// associated thread or `Future` will be woken if currently suspended.
pub struct Notifier {
    inner: Arc<InnerSuspend>,
}

impl Notifier {
    /// Notify the associated [`Suspend`] instance, calling its currently
    /// associated waker, if any.
    pub fn notify(&self) {
        self.inner.notify();
    }

    /// Convert this instance into a [`Waker`].
    pub fn into_waker(self) -> Waker {
        waker_from(self.inner)
    }
}

impl Clone for Notifier {
    fn clone(&self) -> Self {
        Self::from(self.inner.clone())
    }
}

impl From<Arc<InnerSuspend>> for Notifier {
    fn from(inner: Arc<InnerSuspend>) -> Self {
        Self { inner }
    }
}
