use std::task::Waker;
use std::thread::{self, Thread};

#[macro_use]
mod local;
#[doc(hidden)]
pub use local::LocalWaker;

mod internal;

pub use internal::{waker_from, CloneWake};

/// Create a new `Waker` which will unpark the current thread when woken.
pub fn thread_waker() -> Waker {
    waker_from(thread::current())
}

impl CloneWake for Thread {
    fn wake(self) {
        self.unpark()
    }

    fn wake_by_ref(&self) {
        self.unpark()
    }
}
