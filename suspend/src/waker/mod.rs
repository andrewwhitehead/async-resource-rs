use std::task::Waker;
use std::thread::{self, Thread};

#[macro_use]
mod local;
pub use local::LocalWakerRef;

mod internal;

pub use internal::{waker_from, waker_ref, ArcWake, Wake, WakerRef};

thread_local! {
    pub(crate) static THREAD_WAKER: Waker = thread_waker();
}

/// Create a new [`Waker`] which will unpark the current thread when woken.
pub fn thread_waker() -> Waker {
    waker_from(thread::current())
}

impl Wake for Thread {
    fn wake(self) {
        self.unpark()
    }

    fn wake_by_ref(&self) {
        self.unpark()
    }
}
