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
use std::time::{Duration, Instant};

use pin_utils::pin_mut;

use super::thread::thread_suspend_deadline;
use super::waker::{waker_from, waker_ref, ArcWake, WakerRef};

const IDLE: u8 = 0b000;
const WAIT: u8 = 0b001;
const LISTEN: u8 = 0b011;
const PREPARE: u8 = 0b100;
const PREPARE_WAIT: u8 = 0b101;
const PREPARE_LISTEN: u8 = 0b111;

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

    // #[inline]
    pub fn acquire(&self) -> bool {
        self.state.compare_and_swap(IDLE, WAIT, Ordering::AcqRel) == IDLE
    }

    pub fn clear_waiting(&self) -> bool {
        let prev = self.state.fetch_and(!LISTEN, Ordering::Release);
        match prev & LISTEN {
            IDLE => false,
            WAIT => true,
            LISTEN => {
                unsafe { self.vacate_waker() };
                true
            }
            _ => panic!("Invalid state"),
        }
    }

    pub fn try_clear_waiting(&self) -> bool {
        // used to guarantee delivery in one-shot.
        // needs to do nothing and return false if the PREPARE flag is set
        let state = self.state();
        if state == IDLE {
            true
        } else if (state == WAIT || state == LISTEN)
            && self.state.compare_and_swap(state, IDLE, Ordering::Release) == state
        {
            if state == LISTEN {
                unsafe { self.vacate_waker() };
            }
            true
        } else {
            false
        }
    }

    // #[inline]
    pub fn is_idle(&self) -> bool {
        self.state() == IDLE
    }

    // #[inline]
    pub fn is_waiting(&self) -> bool {
        self.state() & LISTEN != IDLE
    }

    /// Try to start listening with a new waker
    pub fn poll(&self, cx: &mut Context) -> Poll<()> {
        match self.state() {
            IDLE | PREPARE => {
                // not currently acquired
                return Poll::Ready(());
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
                            break drop(unsafe { self.vacate_waker() });
                        }
                        Err(WAIT) => {
                            // competition from another thread?
                            // should not happen with wrappers around this instance
                            panic!("Invalid state");
                        }
                        Err(LISTEN) => {
                            // retry
                        }
                        Err(IDLE) | Err(PREPARE) => {
                            // already notified, or being notified
                            return Poll::Ready(());
                        }
                        Err(_) => panic!("Invalid state"),
                    }
                }
            }
            PREPARE_WAIT | PREPARE_LISTEN => {
                // about to be notified. call the new waker to poll again immediately
                cx.waker().wake_by_ref();
                return Poll::Pending;
            }
            _ => panic!("Invalid state"),
        }

        // must be in the WAIT state at this point, or PREPARE_WAIT if pre-empted
        // by prepare_notify()
        unsafe {
            self.update_waker(cx.waker().clone());
        }

        match self.state.compare_and_swap(WAIT, LISTEN, Ordering::AcqRel) {
            WAIT => {
                // registered listener
                Poll::Pending
            }
            IDLE => {
                // notify() was called while storing
                drop(unsafe { self.vacate_waker() });
                Poll::Ready(())
            }
            PREPARE_WAIT => {
                // prepare_notify() was called while storing. call new the waker now,
                // so the listener will poll again for the result
                unsafe { self.vacate_waker() }.wake();
                Poll::Pending
            }
            _ => panic!("Invalid state"),
        }
    }

    // #[inline]
    pub fn prepare_notify(&self) -> bool {
        match self.state.fetch_or(PREPARE, Ordering::Release) {
            IDLE => {
                self.state.store(IDLE, Ordering::Release);
                false
            }
            WAIT | LISTEN => true,
            _ => panic!("Invalid state"),
        }
    }

    /// Clear the state if it is waiting, and call the notifier if any.
    /// Returns true if there was an active listener.
    pub fn notify(&self) -> bool {
        match self.state.swap(IDLE, Ordering::Release) {
            IDLE | PREPARE => false,
            WAIT | PREPARE_WAIT => true,
            LISTEN | PREPARE_LISTEN => {
                unsafe { self.vacate_waker() }.wake();
                true
            }
            _ => panic!("Invalid state"),
        }
    }

    // #[inline]
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
        self.wait_deadline(None);
    }

    pub fn wait_deadline(&self, expire: Option<Instant>) -> bool {
        let mut first = true;
        thread_suspend_deadline(
            |cx| {
                if first {
                    first = false;
                    self.poll(cx)
                } else {
                    // no need to update the waker after the first poll
                    if self.is_idle() {
                        Poll::Ready(())
                    } else {
                        Poll::Pending
                    }
                }
            },
            expire,
        )
        .is_ready()
    }

    pub fn wait_timeout(&self, timeout: Duration) -> bool {
        Instant::now()
            .checked_add(timeout)
            .map(|expire| self.wait_deadline(Some(expire)))
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
        self.clear_waiting();
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
        if self.inner.wait_deadline(Some(expire)) {
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
        self.inner.clear_waiting();
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
