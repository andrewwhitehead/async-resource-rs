use std::sync::Arc;
use std::task::{RawWaker, RawWakerVTable, Waker};

use futures_task::waker;
pub use futures_task::{waker_ref, ArcWake, WakerRef};

/// Convert an instance of a type implementing [`Wake`] or [`ArcWake`] into a
/// [`Waker`].
pub fn waker_from<W: IntoWaker>(inst: W) -> Waker {
    inst.into_waker()
}

/// Provide the ability to wake an instance of an owned type.
pub trait Wake: Clone + Send + Sync {
    fn wake(self) {
        self.wake_by_ref()
    }

    fn wake_by_ref(&self);
}

pub trait IntoWaker {
    fn into_waker(self) -> Waker;
}

impl<W: Wake> IntoWaker for W {
    fn into_waker(self) -> Waker {
        unsafe { Waker::from_raw(WakerImpl::<W>::raw_waker(Box::new(self))) }
    }
}

impl<W: ArcWake> IntoWaker for Arc<W> {
    fn into_waker(self) -> Waker {
        waker(self)
    }
}

impl IntoWaker for Waker {
    fn into_waker(self) -> Waker {
        self
    }
}

pub(crate) struct WakerImpl<W: Wake>(W);

impl<W: Wake> WakerImpl<W> {
    const WAKER_VTABLE: RawWakerVTable = RawWakerVTable::new(
        Self::clone_waker,
        Self::wake_waker,
        Self::wake_by_ref_waker,
        Self::drop_waker,
    );

    fn raw_waker(inst: Box<W>) -> RawWaker {
        let data = Box::into_raw(inst) as *const ();
        RawWaker::new(data, &Self::WAKER_VTABLE)
    }

    pub(crate) unsafe fn clone_waker(data: *const ()) -> RawWaker {
        let inst = &*(data as *const W);
        Self::raw_waker(Box::new(inst.clone()))
    }

    pub(crate) unsafe fn wake_waker(data: *const ()) {
        let inst = Box::from_raw(data as *mut W);
        inst.wake();
    }

    pub(crate) unsafe fn wake_by_ref_waker(data: *const ()) {
        let inst = &*(data as *const W);
        inst.wake_by_ref();
    }

    pub(crate) unsafe fn drop_waker(data: *const ()) {
        drop(Box::from_raw(data as *mut W))
    }
}
