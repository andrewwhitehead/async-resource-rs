use std::fmt::{self, Debug, Formatter};
use std::future::Future;
use std::mem::MaybeUninit;
use std::ops::Deref;
use std::pin::Pin;
use std::ptr::{self, NonNull};
use std::sync::atomic::{spin_loop_hint, AtomicU8, Ordering};
use std::task::{Context, Poll, Waker};
use std::thread;
use std::time::{Duration, Instant};

use pin_utils::pin_mut;

use super::thread::thread_suspend_deadline;
use super::waker::{internal::WakeableState, WakeByRef};

const IDLE: u8 = 0b000;
const WAIT: u8 = 0b001;
const LISTEN: u8 = 0b011;
const LOCKED: u8 = 0b100;
const LOCKED_WAIT: u8 = 0b101;
const LOCKED_LISTEN: u8 = 0b111;

// synchronize with release writes to a specific atomic variable
macro_rules! acquire {
    ($x:expr) => {
        $x.load(Ordering::Acquire);
    };
}

pub(crate) struct SuspendState {
    state: AtomicU8,
    waker: MaybeUninit<Waker>,
}

impl SuspendState {
    #[inline]
    const fn new(state: u8) -> Self {
        Self {
            state: AtomicU8::new(state),
            waker: MaybeUninit::uninit(),
        }
    }

    pub const fn new_idle() -> Self {
        Self::new(IDLE)
    }

    pub const fn new_waiting() -> Self {
        Self::new(WAIT)
    }

    pub fn try_acquire(&self) -> bool {
        if self.state.compare_and_swap(IDLE, WAIT, Ordering::Release) == IDLE {
            acquire!(self.state);
            true
        } else {
            false
        }
    }

    pub fn clear_waiting(&mut self) -> bool {
        let prev = self.state.fetch_and(!LISTEN, Ordering::Release);
        match prev & LISTEN {
            IDLE => false,
            WAIT => true,
            LISTEN => {
                acquire!(self.state);
                unsafe { self.drop_waker() };
                true
            }
            _ => panic!("Invalid state"),
        }
    }

    pub fn check_idle(&self) -> bool {
        self.state.load(Ordering::Acquire) & LISTEN == IDLE
    }

    /// Try to start listening with a new waker
    pub fn poll(&mut self, cx: &mut Context) -> Poll<()> {
        match self.state.load(Ordering::Acquire) {
            IDLE | LOCKED => {
                // not currently acquired
                return Poll::Ready(());
            }
            WAIT => (),
            LISTEN => {
                // try to reacquire wait state
                match self.state.compare_exchange(
                    LISTEN,
                    WAIT,
                    Ordering::Release,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => {
                        acquire!(self.state);
                        unsafe { self.drop_waker() };
                    }
                    Err(WAIT) => {
                        // competition from another thread?
                        // should not happen with wrappers around this instance
                        panic!("Invalid state");
                    }
                    Err(IDLE) | Err(LOCKED) => {
                        // already notified, or being notified
                        acquire!(self.state);
                        return Poll::Ready(());
                    }
                    Err(_) => panic!("Invalid state"),
                }
            }
            LOCKED_WAIT | LOCKED_LISTEN => {
                // about to be notified. call the new waker to poll again immediately
                cx.waker().wake_by_ref();
                return Poll::Pending;
            }
            _ => panic!("Invalid state"),
        }

        // must be in the WAIT state at this point, or LOCKED_WAIT if pre-empted
        // by lock()
        unsafe {
            self.store_waker(cx.waker().clone());
        }

        match self
            .state
            .compare_exchange(WAIT, LISTEN, Ordering::Release, Ordering::Relaxed)
        {
            Ok(_) => {
                // registered listener
                Poll::Pending
            }
            Err(IDLE) => {
                // notify() was called while storing
                acquire!(self.state);
                unsafe { self.drop_waker() };
                Poll::Ready(())
            }
            Err(LOCKED_WAIT) => {
                // lock() was called while storing. call new the waker now,
                // so the listener will poll again for the result
                acquire!(self.state);
                unsafe { self.wake_waker() };
                Poll::Pending
            }
            _ => panic!("Invalid state"),
        }
    }

    pub fn lock(&mut self) -> (bool, bool) {
        let prev = self.state.fetch_or(LOCKED, Ordering::AcqRel);
        (prev & LOCKED == 0, prev & LISTEN != 0)
    }

    /// Clear the waiting state and set the LOCKED bit. This is used to
    /// indicate when the listener is cancelling a notification, as in the
    /// one-shot Task. Returns a pair of (acquired, cleared) where acquired
    /// indicates that the LOCKED bit was not previously set, and cleared
    /// indicates that the state was either WAIT or LISTEN.
    pub fn lock_and_clear_waiting(&mut self) -> (bool, bool) {
        let prev = self.state.swap(LOCKED, Ordering::AcqRel);
        let cleared = match prev & LISTEN {
            IDLE => false,
            WAIT => true,
            LISTEN => {
                unsafe { self.drop_waker() };
                true
            }
            _ => panic!("Invalid state"),
        };
        (prev & LOCKED == 0, cleared)
    }

    /// Clear the state if it is waiting, and call the notifier if any.
    /// Returns a pair of (held, notified) where held indicates that the
    /// lock was still held when the notify occurred.
    pub fn locked_notify(&self) -> (bool, bool) {
        let prev = self.state.swap(IDLE, Ordering::AcqRel);
        (
            prev & LOCKED == LOCKED,
            match prev & LISTEN {
                IDLE => false,
                WAIT => true,
                LISTEN => {
                    unsafe { self.wake_waker() };
                    true
                }
                _ => panic!("Invalid state"),
            },
        )
    }

    pub fn unlock(&mut self) -> bool {
        self.state.fetch_and(!LOCKED, Ordering::AcqRel) & LOCKED == LOCKED
    }

    pub fn unlock_and_clear_waiting(&mut self) -> bool {
        match self.state.swap(IDLE, Ordering::AcqRel) {
            IDLE => false,
            LISTEN | LOCKED_LISTEN => {
                unsafe { self.drop_waker() };
                true
            }
            _ => true,
        }
    }

    pub fn wait_unlock(&self) {
        let mut i = 1;
        loop {
            if self.state.load(Ordering::Acquire) & LOCKED == 0 {
                return;
            }
            if i % 100 == 0 {
                thread::yield_now();
            } else {
                spin_loop_hint();
            }
            i += 1;
        }
    }

    /// Clear the state if it is waiting, and call the notifier if any.
    /// Returns true if there was an active listener.
    pub fn notify(&self) {
        match self.state.swap(IDLE, Ordering::Release) & LISTEN {
            IDLE | WAIT => (),
            LISTEN => {
                acquire!(self.state);
                unsafe { self.wake_waker() };
            }
            _ => panic!("Invalid state"),
        }
    }

    #[inline]
    pub fn state(&self) -> u8 {
        self.state.load(Ordering::SeqCst)
    }

    pub unsafe fn drop_waker(&mut self) {
        ptr::drop_in_place(self.waker.as_mut_ptr());
    }

    pub unsafe fn store_waker(&mut self, waker: Waker) {
        self.waker.as_mut_ptr().write(waker)
    }

    pub unsafe fn wake_waker(&self) {
        ptr::read(&self.waker).assume_init().wake();
    }

    pub fn wait(&mut self) {
        self.wait_deadline(None);
    }

    pub fn wait_deadline(&mut self, expire: Option<Instant>) -> bool {
        let mut first = true;
        thread_suspend_deadline(
            |cx| {
                if first {
                    first = false;
                    self.poll(cx)
                } else {
                    // no need to update the waker after the first poll
                    if self.check_idle() {
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

    pub fn wait_timeout(&mut self, timeout: Duration) -> bool {
        Instant::now()
            .checked_add(timeout)
            .map(|expire| self.wait_deadline(Some(expire)))
            .unwrap_or(false)
    }
}

impl Drop for SuspendState {
    fn drop(&mut self) {
        // vacate any registered listener
        self.clear_waiting();
    }
}

impl WakeByRef for SuspendState {
    fn wake_by_ref(&self) {
        self.notify();
    }
}

pub(crate) struct SharedSuspend {
    ptr: NonNull<WakeableState<SuspendState>>,
}

unsafe impl Send for SharedSuspend {}
unsafe impl Sync for SharedSuspend {}

impl SharedSuspend {
    pub fn new_idle() -> Self {
        Self {
            ptr: WakeableState::new(SuspendState::new_idle()),
        }
    }

    pub fn new_waiting() -> Self {
        Self {
            ptr: WakeableState::new(SuspendState::new_waiting()),
        }
    }

    #[inline]
    fn inner(&self) -> &WakeableState<SuspendState> {
        unsafe { self.ptr.as_ref() }
    }

    pub unsafe fn acquire_unchecked(&self) -> &mut SuspendState {
        self.inner().get_mut()
    }

    pub fn try_acquire(&self) -> Option<&mut SuspendState> {
        if self.inner().get().try_acquire() {
            Some(unsafe { self.acquire_unchecked() })
        } else {
            None
        }
    }

    pub fn waker(&self) -> &Waker {
        self.inner().waker()
    }
}

impl Clone for SharedSuspend {
    fn clone(&self) -> Self {
        WakeableState::inc_count(self.ptr.as_ptr());
        Self { ptr: self.ptr }
    }
}

impl Deref for SharedSuspend {
    type Target = SuspendState;

    fn deref(&self) -> &Self::Target {
        self.inner().get()
    }
}

impl Drop for SharedSuspend {
    fn drop(&mut self) {
        WakeableState::dec_count(unsafe { self.ptr.as_mut() });
    }
}

/// A structure which may be used to suspend a thread or `Future` pending a
/// notification.
pub struct Suspend {
    shared: SharedSuspend,
}

impl Suspend {
    /// Construct a new `Suspend` instance in the Idle state. To begin
    /// listening for notifications, use the `listen` or `try_listen` methods
    /// to construct a [`Listener`].
    pub fn new() -> Self {
        Self {
            shared: SharedSuspend::new_idle(),
        }
    }

    /// Construct a new `Notifier` instance associated with this `Suspend`.
    pub fn notifier(&self) -> Notifier {
        Notifier {
            shared: self.shared.clone(),
        }
    }

    /// Directly notify the `Suspend` instance and call any
    /// currently-associated waker.
    pub fn notify(&self) {
        self.shared.notify();
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
        self.shared.try_acquire().map(|shared| Listener {
            shared,
            waker: self.shared.waker(),
        })
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

    /// Get a reference to a `Waker` associated with the `Suspend` instance.
    /// The reference can be cloned to obtain an owned `Waker`.
    pub fn waker(&self) -> &Waker {
        self.shared.waker()
    }
}

impl Debug for Suspend {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        let state = match self.shared.state() {
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
    shared: &'a mut SuspendState,
    waker: &'a Waker,
}

impl Listener<'_> {
    /// Check if the listener has already been notified.
    pub fn is_notified(&self) -> bool {
        self.shared.check_idle()
    }

    /// Poll a `Future`, which will then notify this listener when ready.
    pub fn poll<F>(&mut self, fut: Pin<&mut F>) -> Poll<F::Output>
    where
        F: Future,
    {
        let mut cx = Context::from_waker(self.waker);
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
        self.shared.wait()
    }

    /// Wait for a notification on the associated `Suspend` instance, parking
    /// the current thread until the result is available or the deadline is
    /// reached. If a timeout occurs then `false` is returned, otherwise
    /// `true`.
    pub fn wait_deadline(self, expire: Instant) -> Result<(), Self> {
        if self.shared.wait_deadline(Some(expire)) {
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
        if self.shared.wait_timeout(timeout) {
            Ok(())
        } else {
            Err(self)
        }
    }
}

impl Debug for Listener<'_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        let state = match self.shared.state() {
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
        self.shared.clear_waiting();
    }
}

impl Future for Listener<'_> {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.shared.poll(cx)
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
    shared: SharedSuspend,
}

impl Notifier {
    /// Notify the associated [`Suspend`] instance, calling its currently
    /// associated waker, if any.
    pub fn notify(&self) {
        self.shared.notify();
    }

    /// Obtain a `Waker` corresponding to the associated [`Suspend`] instance.
    pub fn waker(&self) -> &Waker {
        self.shared.waker()
    }
}

impl Clone for Notifier {
    fn clone(&self) -> Self {
        Self::from(self.shared.clone())
    }
}

impl From<SharedSuspend> for Notifier {
    fn from(shared: SharedSuspend) -> Self {
        Self { shared }
    }
}
