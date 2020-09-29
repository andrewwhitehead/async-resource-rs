use std::fmt::{self, Debug, Formatter};
use std::future::Future;
use std::ops::Deref;
use std::pin::Pin;
use std::sync::atomic::{AtomicU8, Ordering};
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant};

use super::notify::{Listener, Notifier};
use super::thread::thread_suspend_deadline;
use super::util::{BoxPtr, Maybe};
use super::waker::{internal::WakeableState, WakeByRef};

const STATE_IDLE: u8 = 0b000;
const STATE_WAIT: u8 = 0b001;
const STATE_LISTEN: u8 = 0b011;

// synchronize with release writes to a specific atomic variable
macro_rules! acquire {
    ($x:expr) => {
        $x.load(Ordering::Acquire);
    };
}

pub(crate) struct SuspendState {
    state: AtomicU8,
    waker: Maybe<Waker>,
}

unsafe impl Send for SuspendState {}
unsafe impl Sync for SuspendState {}

impl SuspendState {
    #[inline]
    const fn new(state: u8) -> Self {
        Self {
            state: AtomicU8::new(state),
            waker: Maybe::empty(),
        }
    }

    pub const fn new_idle() -> Self {
        Self::new(STATE_IDLE)
    }

    pub const fn new_waiting() -> Self {
        Self::new(STATE_WAIT)
    }

    pub fn try_acquire(&self) -> bool {
        if self
            .state
            .compare_and_swap(STATE_IDLE, STATE_WAIT, Ordering::Release)
            == STATE_IDLE
        {
            acquire!(self.state);
            true
        } else {
            false
        }
    }

    pub fn clear_waiting(&mut self) -> bool {
        let prev = self.state.fetch_and(!STATE_LISTEN, Ordering::Release);
        match prev & STATE_LISTEN {
            STATE_IDLE => false,
            STATE_WAIT => true,
            STATE_LISTEN => {
                acquire!(self.state);
                self.waker.clear();
                true
            }
            _ => panic!("Invalid state"),
        }
    }

    pub fn check_idle(&self) -> bool {
        self.state.load(Ordering::Acquire) & STATE_LISTEN == STATE_IDLE
    }

    /// Try to start listening with a new waker
    pub fn poll(&mut self, cx: &mut Context) -> Poll<()> {
        match self.state.load(Ordering::Acquire) {
            STATE_IDLE => {
                // not currently acquired
                return Poll::Ready(());
            }
            STATE_WAIT => (),
            STATE_LISTEN => {
                // try to reacquire wait state
                match self.state.compare_exchange(
                    STATE_LISTEN,
                    STATE_WAIT,
                    Ordering::Release,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => {
                        acquire!(self.state);
                        self.waker.clear();
                    }
                    Err(STATE_WAIT) => {
                        // competition from another thread?
                        // should not happen with wrappers around this instance
                        panic!("Invalid state");
                    }
                    Err(STATE_IDLE) => {
                        // already notified, or being notified
                        acquire!(self.state);
                        return Poll::Ready(());
                    }
                    Err(_) => panic!("Invalid state"),
                }
            }
            _ => panic!("Invalid state"),
        }

        // must be in the WAIT state at this point, or LOCKED_WAIT if pre-empted
        // by lock()
        self.waker.store(cx.waker().clone());

        match self.state.compare_exchange(
            STATE_WAIT,
            STATE_LISTEN,
            Ordering::Release,
            Ordering::Relaxed,
        ) {
            Ok(_) => {
                // registered listener
                Poll::Pending
            }
            Err(STATE_IDLE) => {
                // notify() was called while storing
                acquire!(self.state);
                self.waker.clear();
                Poll::Ready(())
            }
            _ => panic!("Invalid state"),
        }
    }

    /// Clear the state if it is waiting, and call the notifier if any.
    pub fn notify(&self) {
        match self.state.swap(STATE_IDLE, Ordering::Release) & STATE_LISTEN {
            STATE_IDLE | STATE_WAIT => (),
            STATE_LISTEN => {
                acquire!(self.state);
                self.waker.load().wake();
            }
            _ => panic!("Invalid state"),
        }
    }

    #[inline]
    pub fn is_waiting(&self) -> bool {
        self.state.load(Ordering::Relaxed) != STATE_IDLE
    }

    #[inline]
    pub fn state(&self) -> u8 {
        self.state.load(Ordering::Relaxed)
    }

    pub fn state_str(&self) -> &'static str {
        match self.state() {
            STATE_IDLE => "Notified",
            STATE_WAIT => "Waiting",
            STATE_LISTEN => "Polled",
            _ => "<!>",
        }
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
    ptr: BoxPtr<WakeableState<SuspendState>>,
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
        &*self.ptr
    }

    pub unsafe fn acquire_unchecked(&self) -> &mut SuspendState {
        self.inner().get_mut()
    }

    pub fn try_acquire(&self) -> Option<&mut SuspendState> {
        if self.inner().as_ref().try_acquire() {
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
        self.inner().as_ref()
    }
}

impl Drop for SharedSuspend {
    fn drop(&mut self) {
        WakeableState::dec_count(self.ptr.as_ptr());
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
        Notifier::from(self.shared.clone())
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
    pub fn poll_future<F>(&mut self, fut: Pin<&mut F>) -> Result<F::Output, Listener<'_>>
    where
        F: Future,
    {
        let mut listener = self.listen();
        match listener.poll_future(fut) {
            Poll::Ready(r) => Ok(r),
            Poll::Pending => Err(listener),
        }
    }

    /// A convenience method to poll a `Future + Unpin`.
    pub fn poll_future_unpin<F>(&mut self, fut: &mut F) -> Result<F::Output, Listener<'_>>
    where
        F: Future + Unpin,
    {
        self.poll_future(Pin::new(fut))
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
        futures_lite::pin!(fut);
        loop {
            match self.poll_future(fut.as_mut()) {
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
            match self.poll_future_unpin(&mut fut) {
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

    /// Block on the result of a `Future`, aborting if and when the given
    /// predicate function returns `false`.
    pub fn wait_on_while<F, C>(&mut self, fut: F, mut cond: C) -> Option<F::Output>
    where
        F: Future,
        C: FnMut() -> bool,
    {
        futures_lite::pin!(fut);
        loop {
            match self.poll_future(fut.as_mut()) {
                Ok(result) => break Some(result),
                Err(listen) => listen.wait(),
            }
            if !(cond)() {
                break None;
            }
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
            STATE_IDLE => "Idle",
            STATE_WAIT => "Waiting",
            STATE_LISTEN => "Listening",
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
