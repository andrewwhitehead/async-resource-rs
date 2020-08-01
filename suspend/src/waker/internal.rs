use std::cell::UnsafeCell;
use std::mem::MaybeUninit;
use std::ops::Deref;
use std::ptr::{self, NonNull};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use std::task::{RawWaker, RawWakerVTable, Waker};

use futures_task::waker;
pub use futures_task::{waker_ref, ArcWake, WakerRef};

/// Convert an instance of a type implementing [`Wake`] or [`ArcWake`] into a
/// [`Waker`].
pub fn waker_from<W: IntoWaker>(inst: W) -> Waker {
    inst.into_waker()
}

/// A generic trait for types that can be woken by reference.
pub trait WakeByRef: Send + Sync {
    fn wake_by_ref(&self);
}

pub trait IntoWaker {
    fn into_waker(self) -> Waker;
}

impl<W: WakeByRef> IntoWaker for W {
    fn into_waker(self) -> Waker {
        WakeableState::<W>::new_waker(self)
    }
}

impl<W: ArcWake> IntoWaker for Arc<W> {
    fn into_waker(self) -> Waker {
        waker(self)
    }
}

impl<W: ArcWake> IntoWaker for &Arc<W> {
    fn into_waker(self) -> Waker {
        waker(self.clone())
    }
}

impl<W: WakeByRef> IntoWaker for Wakeable<W> {
    fn into_waker(self) -> Waker {
        self.waker().clone()
    }
}

impl<W: WakeByRef> IntoWaker for &Wakeable<W> {
    fn into_waker(self) -> Waker {
        self.waker().clone()
    }
}

impl IntoWaker for Waker {
    fn into_waker(self) -> Waker {
        self
    }
}

impl IntoWaker for &Waker {
    fn into_waker(self) -> Waker {
        self.clone()
    }
}

pub(crate) struct WakeableState<T: WakeByRef> {
    value: UnsafeCell<T>,
    waker: MaybeUninit<Waker>,
    count: AtomicUsize,
}

impl<T: WakeByRef> WakeableState<T> {
    const WAKER_VTABLE: RawWakerVTable = RawWakerVTable::new(
        Self::clone_waker,
        Self::wake_waker,
        Self::wake_by_ref_waker,
        Self::drop_waker,
    );

    #[inline]
    fn new_raw(value: T) -> *mut Self {
        Box::into_raw(Box::new(Self {
            value: UnsafeCell::new(value),
            waker: MaybeUninit::uninit(),
            count: AtomicUsize::new(1),
        }))
    }

    pub fn new(value: T) -> NonNull<Self> {
        unsafe {
            let slf = Self::new_raw(value);
            (&mut *slf).waker = MaybeUninit::new(Waker::from_raw(Self::raw_waker(slf)));
            NonNull::new_unchecked(slf)
        }
    }

    pub fn new_waker(value: T) -> Waker {
        unsafe {
            let slf = Self::new_raw(value);
            Waker::from_raw(Self::raw_waker(slf))
        }
    }

    #[inline]
    pub fn get(&self) -> &T {
        unsafe { &*self.value.get() }
    }

    #[inline]
    pub unsafe fn get_mut(&self) -> &mut T {
        &mut *self.value.get()
    }

    #[inline]
    pub fn inc_count(data: *mut Self) {
        unsafe { &*data }.count.fetch_add(1, Ordering::Relaxed);
    }

    #[inline]
    pub fn dec_count(data: *mut Self) {
        unsafe {
            if (&*data).count.fetch_sub(1, Ordering::Release) == 1 {
                // use an acquire load to synchronize specifically with release
                // writes to count on other threads
                (&*data).count.load(Ordering::Acquire);
                ptr::drop_in_place(data);
            }
        }
    }

    #[inline]
    pub fn waker(&self) -> &Waker {
        unsafe { &*self.waker.as_ptr() }
    }

    #[inline]
    fn raw_waker(data: *const Self) -> RawWaker {
        RawWaker::new(data as *const (), &Self::WAKER_VTABLE)
    }

    unsafe fn clone_waker(data: *const ()) -> RawWaker {
        let inst = &mut *(data as *mut Self);
        Self::inc_count(inst);
        Self::raw_waker(data as *const Self)
    }

    unsafe fn wake_waker(data: *const ()) {
        let inst = &mut *(data as *mut Self);
        inst.get().wake_by_ref();
        Self::dec_count(inst);
    }

    unsafe fn wake_by_ref_waker(data: *const ()) {
        let inst = &*(data as *const Self);
        inst.get().wake_by_ref();
    }

    unsafe fn drop_waker(data: *const ()) {
        Self::dec_count(data as *mut Self);
    }
}

pub struct Wakeable<T: WakeByRef> {
    ptr: NonNull<WakeableState<T>>,
}

impl<T: WakeByRef> Wakeable<T> {
    pub fn new(value: T) -> Self {
        Self {
            ptr: WakeableState::new(value),
        }
    }

    fn waker(&self) -> &Waker {
        unsafe { self.ptr.as_ref() }.waker()
    }
}

impl<T: WakeByRef> Clone for Wakeable<T> {
    fn clone(&self) -> Self {
        WakeableState::<T>::inc_count(self.ptr.as_ptr());
        Self { ptr: self.ptr }
    }
}

impl<T: WakeByRef> Deref for Wakeable<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        unsafe { self.ptr.as_ref().get() }
    }
}

impl<T: WakeByRef> Drop for Wakeable<T> {
    fn drop(&mut self) {
        WakeableState::<T>::dec_count(self.ptr.as_ptr());
    }
}
