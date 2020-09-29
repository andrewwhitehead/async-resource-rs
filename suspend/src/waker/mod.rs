use std::task::Waker;
use std::thread;

// #[macro_use]
// mod local;
// pub use local::LocalWakerRef;

pub(crate) mod internal;

pub use internal::{waker_ref, ArcWake, IntoWaker, WakeByRef, Wakeable, WakerRef};

pub(crate) mod null;

pub use null::NullWaker;

thread_local! {
    pub(crate) static THREAD_WAKER: Waker = thread_waker();
}

/// Convert an instance of a type implementing [`Wake`] or [`ArcWake`] into a
/// [`Waker`].
pub fn waker_from<W: IntoWaker>(inst: W) -> Waker {
    inst.into_waker()
}

/// Create a new [`Waker`] which will unpark the current thread when woken.
pub fn thread_waker() -> Waker {
    waker_from(thread::current())
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
