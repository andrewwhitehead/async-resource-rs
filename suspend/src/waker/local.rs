use std::marker::PhantomData;
use std::ops::Deref;
use std::pin::Pin;
use std::task::{RawWaker, RawWakerVTable, Waker};

use super::internal::{CloneWake, CloneWakerImpl};

/// A struct providing a reference to a `Waker`, where the target of the Waker
/// is pinned on the stack and will be cloned to the heap on first use. This
/// is used to avoid allocating unless the Waker is cloned to be woken later,
/// particularly for a Thread instance.
pub struct LocalWaker<'a, W: CloneWake> {
    waker: Waker,
    _pd: PhantomData<&'a W>,
}

impl<'w, W: CloneWake + 'w> LocalWaker<'w, W> {
    const WAKER_VTABLE: RawWakerVTable = RawWakerVTable::new(
        CloneWakerImpl::<W>::clone_waker,
        Self::wake_waker,
        CloneWakerImpl::<W>::wake_by_ref_waker,
        Self::drop_waker,
    );

    pub fn new(inst: Pin<&mut W>) -> Self {
        Self {
            waker: unsafe { Waker::from_raw(Self::raw_waker(inst)) },
            _pd: PhantomData,
        }
    }

    fn raw_waker(mut inst: Pin<&mut W>) -> RawWaker {
        let data = &*inst.as_mut() as *const W as *const ();
        RawWaker::new(data, &Self::WAKER_VTABLE)
    }

    unsafe fn wake_waker(_data: *const ()) {
        // the waker itself is owned by this struct, so it should not be
        // possible to wake it directly, only by reference
        unimplemented!();
    }

    unsafe fn drop_waker(_data: *const ()) {
        // nothing to do: the instance will be dropped when the lifetime ends
    }
}

impl<W: CloneWake> Deref for LocalWaker<'_, W> {
    type Target = Waker;

    fn deref(&self) -> &Self::Target {
        &self.waker
    }
}

#[cfg_attr(feature = "test_clone_waker", macro_export)]
#[doc(hidden)]
macro_rules! local_waker {
    ($x:ident, $y:expr) => {
        // borrowed from pin-project-lite:
        let mut $x = $y;
        // Shadow the original binding so that it can't be directly accessed
        // ever again.
        #[allow(unused_mut)]
        let mut $x = unsafe { $crate::re_export::Pin::new_unchecked(&mut $x) };

        let $x = $crate::waker::LocalWaker::new($x);
    };
}
