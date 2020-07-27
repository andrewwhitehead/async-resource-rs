use std::marker::PhantomData;
use std::mem;
use std::ops::Deref;
use std::pin::Pin;
use std::task::{RawWaker, RawWakerVTable, Waker};
use std::thread::{self, Thread};

pub fn waker_from<W: CloneWake>(inst: W) -> Waker {
    unsafe { Waker::from_raw(raw_waker(Box::new(inst))) }
}

pub fn current_thread_waker() -> Waker {
    thread_waker(thread::current())
}

pub fn thread_waker(thread: Thread) -> Waker {
    waker_from(thread)
}

pub trait CloneWake: Clone {
    fn wake(&self);
}

impl CloneWake for Thread {
    fn wake(&self) {
        self.unpark()
    }
}

fn raw_waker<W: CloneWake>(inst: Box<W>) -> RawWaker {
    let data = Box::into_raw(inst) as *const ();
    RawWaker::new(
        data,
        &RawWakerVTable::new(
            clone_waker_raw::<W>,
            wake_waker_raw::<W>,
            wake_by_ref_waker_raw::<W>,
            drop_waker_raw::<W>,
        ),
    )
}

unsafe fn clone_waker_raw<W: CloneWake>(data: *const ()) -> RawWaker {
    let inst = Box::from_raw(data as *mut W);
    let result = raw_waker(inst.clone());
    mem::forget(inst);
    result
}

unsafe fn wake_waker_raw<W: CloneWake>(data: *const ()) {
    let inst = Box::from_raw(data as *mut W);
    inst.wake();
}

unsafe fn wake_by_ref_waker_raw<W: CloneWake>(data: *const ()) {
    let inst = Box::from_raw(data as *mut W);
    inst.wake();
    mem::forget(inst);
}

unsafe fn drop_waker_raw<W: CloneWake>(data: *const ()) {
    drop(Box::from_raw(data as *mut W))
}

pub struct LocalWaker<'a, T: CloneWake> {
    waker: Waker,
    _pd: PhantomData<&'a T>,
}

impl<T: CloneWake> LocalWaker<'_, T> {
    pub fn new(inst: Pin<&mut T>) -> Self {
        Self {
            waker: unsafe { Waker::from_raw(raw_local_waker(inst)) },
            _pd: PhantomData,
        }
    }
}

impl<T: CloneWake> Deref for LocalWaker<'_, T> {
    type Target = Waker;

    fn deref(&self) -> &Self::Target {
        &self.waker
    }
}

fn raw_local_waker<W: CloneWake>(mut inst: Pin<&mut W>) -> RawWaker {
    let data = &*inst.as_mut() as *const W as *const ();
    RawWaker::new(
        data,
        &RawWakerVTable::new(
            clone_local_waker_raw::<W>,
            wake_by_local_waker_raw::<W>,
            wake_by_ref_local_waker_raw::<W>,
            noop_drop,
        ),
    )
}

unsafe fn clone_local_waker_raw<W: CloneWake>(data: *const ()) -> RawWaker {
    let inst = &*(data as *const W);
    raw_waker(Box::new(inst.clone()))
}

unsafe fn wake_by_local_waker_raw<W: CloneWake>(_data: *const ()) {
    unimplemented!();
}

unsafe fn wake_by_ref_local_waker_raw<W: CloneWake>(data: *const ()) {
    let inst = &*(data as *const W);
    inst.wake();
}

unsafe fn noop_drop(_data: *const ()) {}

#[cfg_attr(feature = "test_clone_waker", macro_export)]
macro_rules! local_waker {
    ($x:ident, $y:expr) => {
        // borrowed from pin-project-lite:
        let mut $x = $y;
        // Shadow the original binding so that it can't be directly accessed
        // ever again.
        #[allow(unused_mut)]
        let mut $x = unsafe { std::pin::Pin::new_unchecked(&mut $x) };

        let $x = LocalWaker::new($x);
    };
}
