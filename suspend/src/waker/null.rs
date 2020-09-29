use option_lock::OnceCell;
use std::task::{RawWaker, RawWakerVTable, Waker};

static NULL_WAKER: OnceCell<Waker> = OnceCell::empty();

pub struct NullWaker;

impl NullWaker {
    const RAW_WAKER: RawWaker = RawWaker::new(std::ptr::null(), &Self::WAKER_VTABLE);

    const WAKER_VTABLE: RawWakerVTable = RawWakerVTable::new(
        Self::clone_waker,
        Self::wake_waker,
        Self::wake_by_ref_waker,
        Self::drop_waker,
    );

    pub fn get() -> &'static Waker {
        if let Some(waker) = NULL_WAKER.get() {
            waker
        } else {
            NULL_WAKER.set(unsafe { Waker::from_raw(Self::RAW_WAKER) });
            NULL_WAKER.get().unwrap()
        }
    }

    unsafe fn clone_waker(_: *const ()) -> RawWaker {
        Self::RAW_WAKER
    }

    unsafe fn wake_waker(_: *const ()) {}

    unsafe fn wake_by_ref_waker(_: *const ()) {}

    unsafe fn drop_waker(_: *const ()) {}
}
