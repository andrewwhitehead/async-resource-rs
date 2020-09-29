use std::fmt::{self, Debug, Formatter};
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant};

use super::core::{SharedSuspend, SuspendState};

/// Create a new single-use [`NotifyOnce`] and a corresponding [`ListenOnce`].
/// Once notified, the ListenOnce will resolve to `()`.
pub fn notify_once<'a>() -> (NotifyOnce, ListenOnce) {
    let shared = SharedSuspend::new_waiting();
    (NotifyOnce(shared.clone()), ListenOnce(shared))
}

pub struct NotifyOnce(SharedSuspend);

impl NotifyOnce {
    pub fn notify(self) {
        self.0.notify();
    }
}

impl Debug for NotifyOnce {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        let state = if self.0.is_waiting() {
            "Waiting"
        } else {
            "Notified"
        };
        write!(f, "NotifyOnce({})", state)
    }
}

pub struct ListenOnce(SharedSuspend);

impl Debug for ListenOnce {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        let state = if self.0.is_waiting() {
            "Waiting"
        } else {
            "Notified"
        };
        write!(f, "ListenOnce({})", state)
    }
}

impl Future for ListenOnce {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        unsafe { self.0.acquire_unchecked() }.poll(cx)
    }
}

/// The result of acquiring a [`Suspend`] using either [`Suspend::listen`] or
/// [`Suspend::try_listen`]. It may be used to wait for a notification with
/// `.await` or by parking the current thread.
pub struct Listener<'a> {
    pub(crate) shared: &'a mut SuspendState,
    pub(crate) waker: &'a Waker,
}

impl Listener<'_> {
    /// Check if the listener has already been notified.
    pub fn is_notified(&self) -> bool {
        self.shared.check_idle()
    }

    /// Poll a `Future`, which will then notify this listener when ready.
    pub fn poll_future<F>(&mut self, fut: Pin<&mut F>) -> Poll<F::Output>
    where
        F: Future,
    {
        let mut cx = Context::from_waker(self.waker);
        fut.poll(&mut cx)
    }

    /// A convenience method to poll a `Future + Unpin`.
    #[inline]
    pub fn poll_future_unpin<F>(&mut self, fut: &mut F) -> Poll<F::Output>
    where
        F: Future + Unpin,
    {
        self.poll_future(Pin::new(fut))
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
        write!(f, "Listener({})", self.shared.state_str())
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
