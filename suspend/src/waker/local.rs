use std::marker::PhantomData;
use std::ops::Deref;
use std::pin::Pin;
use std::task::{RawWaker, RawWakerVTable, Waker};

use super::internal::{Wake, WakerImpl};

/// A struct providing a reference to a [`Waker`], where the target to be woken
/// is pinned on the stack and will be cloned to the heap only if necessary.
pub struct LocalWakerRef<'a, W: Wake> {
    waker: Waker,
    _pd: PhantomData<&'a W>,
}

impl<'w, W: Wake + 'w> LocalWakerRef<'w, W> {
    const WAKER_VTABLE: RawWakerVTable = RawWakerVTable::new(
        WakerImpl::<W>::clone_waker,
        Self::wake_waker,
        WakerImpl::<W>::wake_by_ref_waker,
        Self::drop_waker,
    );

    /// Create a new local waker reference from a pinned reference to a type
    /// implementing [`Wake`].
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
        // nothing to do: the instance will be dropped by the containing scope
    }
}

impl<W: Wake> Deref for LocalWakerRef<'_, W> {
    type Target = Waker;

    fn deref(&self) -> &Self::Target {
        &self.waker
    }
}

#[macro_export]
/// Pin an instance of a type implementing [`Wake`](crate::waker::Wake) to
/// the stack, and provide a [`LocalWakerRef`](crate::LocalWakerRef) which
/// dereferences to a [`Waker`](crate::waker::Waker) for the value. If and
/// when the Waker is cloned, the value will be cloned to the heap as well.
macro_rules! local_waker {
    ($x:ident, $y:expr) => {
        // borrowed from pin-project-lite:
        let mut $x = $y;
        // Shadow the original binding so that it can't be directly accessed
        // ever again.
        #[allow(unused_mut)]
        let mut $x = unsafe { $crate::re_export::Pin::new_unchecked(&mut $x) };

        let $x = $crate::waker::LocalWakerRef::new($x);
    };
}
