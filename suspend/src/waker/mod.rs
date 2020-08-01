use std::task::Waker;
use std::thread::{self, Thread};

// #[macro_use]
// mod local;
// pub use local::LocalWakerRef;

pub(crate) mod internal;

pub use internal::{waker_from, waker_ref, ArcWake, WakeByRef, Wakeable, WakerRef};

thread_local! {
    pub(crate) static THREAD_WAKER: Waker = thread_waker();
}

/// Create a new [`Waker`] which will unpark the current thread when woken.
pub fn thread_waker() -> Waker {
    waker_from(thread::current())
}

// FIXME Wake for AtomicBool? boxed function?

impl WakeByRef for Thread {
    fn wake_by_ref(&self) {
        self.unpark()
    }
}

struct WakerFn<F: Fn() + Send + Sync>(F);

impl<F: Fn() + Send + Sync> WakeByRef for WakerFn<F> {
    fn wake_by_ref(&self) {
        (self.0)()
    }
}

pub fn waker_fn<F: Fn() + Send + Sync>(f: F) -> Waker {
    waker_from(WakerFn(f))
}
