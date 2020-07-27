use std::task::{RawWaker, RawWakerVTable, Waker};

/// Convert an instance of a type implementing `CloneWake` into a `Waker`.
pub fn waker_from<W: CloneWake>(inst: W) -> Waker {
    CloneWakerImpl::<W>::into_waker(inst)
}

pub trait CloneWake: Clone {
    fn wake(self) {
        self.wake_by_ref()
    }

    fn wake_by_ref(&self);
}

pub(crate) struct CloneWakerImpl<W: CloneWake>(W);

impl<W: CloneWake> CloneWakerImpl<W> {
    const WAKER_VTABLE: RawWakerVTable = RawWakerVTable::new(
        Self::clone_waker,
        Self::wake_waker,
        Self::wake_by_ref_waker,
        Self::drop_waker,
    );

    fn into_waker(inst: W) -> Waker {
        unsafe { Waker::from_raw(Self::raw_waker(Box::new(inst))) }
    }

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
