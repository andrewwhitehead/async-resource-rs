use std::cell::Cell;
use std::ptr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};
use std::thread::{self, Thread};
use std::time::Instant;

use super::util::BoxPtr;

const WAKE_STATE_IDLE: usize = 0b000;
const WAKE_STATE_WOKEN: usize = 0b001;
const MIN_SEQNO: usize = WAKE_STATE_WOKEN + 1;

const REF_STATE_IDLE: u8 = 0b000;
const REF_STATE_WOKEN: u8 = 0b001;
const REF_STATE_ACQUIRED: u8 = 0b010;
const REF_STATE_CREATED: u8 = 0b100;

thread_local! {
    static THREAD_WAKE_HANDLE: ThreadWakeHandle = ThreadWakeHandle::new(thread::current());
}

#[inline]
pub fn thread_suspend<F, R>(f: F) -> R
where
    F: FnMut(&mut Context) -> Poll<R>,
{
    if let Poll::Ready(result) = thread_suspend_deadline(f, None) {
        result
    } else {
        unreachable!()
    }
}

#[inline]
pub fn thread_suspend_deadline<F, R>(mut f: F, expire: Option<Instant>) -> Poll<R>
where
    F: FnMut(&mut Context) -> Poll<R>,
{
    THREAD_WAKE_HANDLE.with(|thread_wake| {
        let mut wake_ref = thread_wake.wake_ref();
        let waker = wake_ref.waker();
        let mut cx = Context::from_waker(&waker);
        let mut next_park_duration = None;
        loop {
            loop {
                if let Poll::Ready(result) = f(&mut cx) {
                    return Poll::Ready(result);
                }
                if expire.is_some() {
                    next_park_duration = expire
                        .as_ref()
                        .unwrap()
                        .checked_duration_since(Instant::now());
                    if next_park_duration.is_none() {
                        return Poll::Pending;
                    }
                }
                if !wake_ref.restart() {
                    break;
                }
            }
            loop {
                if let Some(dur) = next_park_duration.take() {
                    thread::park_timeout(dur);
                } else {
                    thread::park();
                }
                if expire.is_some() {
                    next_park_duration = expire
                        .as_ref()
                        .unwrap()
                        .checked_duration_since(Instant::now());
                    if next_park_duration.is_none() {
                        return Poll::Pending;
                    }
                }
                if wake_ref.restart() {
                    break;
                }
            }
        }
    })
}

struct ThreadWakeHandle(BoxPtr<ThreadWake>, Cell<usize>);

impl ThreadWakeHandle {
    pub fn new(thread: Thread) -> Self {
        Self(
            ThreadWake::new(thread, WAKE_STATE_IDLE, 1),
            Cell::new(MIN_SEQNO - 1),
        )
    }

    #[cfg(test)]
    pub fn load_seqno(&self) -> usize {
        self.0.seqno.load(Ordering::Relaxed)
    }

    #[inline]
    pub fn wake_ref(&self) -> ThreadWakeRef {
        let seqno = {
            let mut seq = self.1.get().wrapping_add(1);
            // handle overflow
            seq = std::cmp::max(seq, MIN_SEQNO);
            self.1.replace(seq);
            seq
        };
        ThreadWakeRef::new(self.0, seqno)
    }
}

impl Drop for ThreadWakeHandle {
    fn drop(&mut self) {
        ThreadWake::dec_count(self.0);
    }
}

pub(crate) struct ThreadWake {
    thread: Thread,
    seqno: AtomicUsize,
    count: AtomicUsize,
}

impl ThreadWake {
    #[inline]
    pub fn new(thread: Thread, seqno: usize, count: usize) -> BoxPtr<Self> {
        BoxPtr::new(Box::new(Self {
            thread,
            seqno: AtomicUsize::new(seqno),
            count: AtomicUsize::new(count),
        }))
    }

    #[inline]
    pub fn acquire(&self, seqno: usize) -> bool {
        let prev = self
            .seqno
            .compare_and_swap(WAKE_STATE_IDLE, seqno, Ordering::Acquire);
        prev == WAKE_STATE_IDLE || prev == seqno
    }

    #[inline]
    pub fn release(&self) {
        self.seqno.store(WAKE_STATE_IDLE, Ordering::Release);
    }

    #[inline]
    pub fn restart(&self, seqno: usize) -> bool {
        self.seqno.swap(seqno, Ordering::Acquire) == WAKE_STATE_WOKEN
    }

    pub fn unpark(&self, seqno: usize) {
        if self
            .seqno
            .compare_exchange(
                seqno,
                WAKE_STATE_WOKEN,
                Ordering::Release,
                Ordering::Relaxed,
            )
            .is_ok()
        {
            self.thread.unpark();
        }
    }

    #[inline]
    pub fn inc_count(data: BoxPtr<Self>) {
        data.count.fetch_add(1, Ordering::Relaxed);
    }

    pub fn dec_count(data: BoxPtr<Self>) {
        if data.count.fetch_sub(1, Ordering::Release) == 1 {
            // perform an acquire load to synchronize specifically with release
            // writes to 'count' on other threads
            data.count.load(Ordering::Acquire);
            drop(data.into_box())
        }
    }
}

pub(crate) struct ThreadWakeRef {
    seqno: usize,
    ptr: BoxPtr<ThreadWake>,
    state: u8,
}

impl ThreadWakeRef {
    const WAKER_VTABLE: RawWakerVTable = RawWakerVTable::new(
        Self::clone_waker,
        Self::wake_waker,
        Self::wake_by_ref_waker,
        Self::drop_waker,
    );

    #[inline]
    pub fn new(ptr: BoxPtr<ThreadWake>, seqno: usize) -> Self {
        Self {
            ptr,
            state: REF_STATE_IDLE,
            seqno,
        }
    }

    #[inline]
    pub fn waker(&mut self) -> Waker {
        let data = self as *const Self as *const ();
        unsafe { Waker::from_raw(RawWaker::new(data, &Self::WAKER_VTABLE)) }
    }

    #[inline]
    pub fn restart(&mut self) -> bool {
        let woken = self.state & REF_STATE_WOKEN != 0;
        self.state &= !REF_STATE_WOKEN;
        woken
            || if self.state & REF_STATE_ACQUIRED != 0 {
                self.ptr.restart(self.seqno)
            } else {
                false
            }
    }

    unsafe fn clone_waker(data: *const ()) -> RawWaker {
        let inst = &mut *(data as *const Self as *mut Self);
        // it should not be possible to interact with the Waker simultaneously from
        // multiple threads, so we aren't using an atomic for the state
        if inst.state & REF_STATE_ACQUIRED == 0 {
            if inst.ptr.acquire(inst.seqno) {
                // acquired the thread local instance. increase the count to
                // correspond with the waker we are creating
                inst.state |= REF_STATE_ACQUIRED;
                ThreadWake::inc_count(inst.ptr);
            } else {
                inst.state |= REF_STATE_ACQUIRED | REF_STATE_CREATED;
                // init with a count of 2, corresponding to this reference and the waker
                inst.ptr = ThreadWake::new(inst.ptr.thread.clone(), inst.seqno, 2);
            }
        } else {
            ThreadWake::inc_count(inst.ptr);
        }
        ThreadWakeClone::raw_waker(inst.ptr, inst.seqno)
    }

    unsafe fn wake_waker(_data: *const ()) {
        // this should not be possible, as only a reference to the Waker is shared
        unreachable!();
    }

    unsafe fn wake_by_ref_waker(data: *const ()) {
        // the poll-ee called cx.waker().wake_by_ref() while owning a reference to the
        // context. the flag will be checked before attempting to park the thread, so
        // there is no reason to unpark it here
        let inst = &mut *(data as *const Self as *mut Self);
        inst.state |= REF_STATE_WOKEN;
    }

    unsafe fn drop_waker(_data: *const ()) {
        // no-op
        // there is no cleanup to perform
    }
}

impl Drop for ThreadWakeRef {
    fn drop(&mut self) {
        if self.state & REF_STATE_CREATED != 0 {
            // reduce the count of the instance we created (re-entry use case)
            ThreadWake::dec_count(self.ptr);
        } else if self.state & REF_STATE_ACQUIRED != 0 {
            // the count for the thread local instance is not increased when it is acquired
            self.ptr.release();
        }
    }
}

pub(crate) struct ThreadWakeClone {
    seqno: usize,
    ptr: BoxPtr<ThreadWake>,
}

impl ThreadWakeClone {
    const WAKER_VTABLE: RawWakerVTable = RawWakerVTable::new(
        Self::clone_waker,
        Self::wake_waker,
        Self::wake_by_ref_waker,
        Self::drop_waker,
    );

    #[inline]
    pub fn raw_waker(ptr: BoxPtr<ThreadWake>, seqno: usize) -> RawWaker {
        let data = Box::into_raw(Box::new(Self { ptr, seqno }));
        RawWaker::new(data as *const (), &Self::WAKER_VTABLE)
    }

    unsafe fn clone_waker(data: *const ()) -> RawWaker {
        let inst = ptr::read(data as *const Self);
        ThreadWake::inc_count(inst.ptr);
        Self::raw_waker(inst.ptr, inst.seqno)
    }

    unsafe fn wake_waker(data: *const ()) {
        let inst = Box::from_raw(data as *mut Self);
        inst.ptr.unpark(inst.seqno);
        ThreadWake::dec_count(inst.ptr);
    }

    unsafe fn wake_by_ref_waker(data: *const ()) {
        let inst = ptr::read(data as *const Self);
        inst.ptr.unpark(inst.seqno);
    }

    unsafe fn drop_waker(data: *const ()) {
        let inst = Box::from_raw(data as *mut Self);
        ThreadWake::dec_count(inst.ptr);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn thread_suspend_ready() {
        let calls = Cell::new(0);
        assert_eq!(
            thread_suspend_deadline(
                |_cx| {
                    calls.replace(calls.get() + 1);
                    Poll::Ready(())
                },
                Some(Instant::now() + Duration::from_millis(50))
            ),
            Poll::Ready(())
        );
        assert_eq!(calls.get(), 1);
    }

    #[test]
    fn thread_suspend_timeout() {
        let calls = Cell::new(0);
        assert_eq!(
            thread_suspend_deadline(
                |_cx| {
                    if calls.replace(calls.get() + 1) == 0 {
                        Poll::Pending
                    } else {
                        Poll::Ready(())
                    }
                },
                Some(Instant::now() + Duration::from_millis(50))
            ),
            Poll::Pending
        );
        assert_eq!(calls.get(), 1);
    }

    #[test]
    fn thread_suspend_wake_immed() {
        let calls = Cell::new(0);
        assert_eq!(
            thread_suspend_deadline(
                |cx| {
                    if calls.replace(calls.get() + 1) == 0 {
                        cx.waker().wake_by_ref();
                        Poll::Pending
                    } else {
                        Poll::Ready(())
                    }
                },
                Some(Instant::now() + Duration::from_millis(50))
            ),
            Poll::Ready(())
        );
        assert_eq!(calls.get(), 2);
    }

    #[test]
    fn thread_suspend_wake_clone_immed() {
        let calls = Cell::new(0);
        assert_eq!(
            thread_suspend_deadline(
                |cx| {
                    if calls.replace(calls.get() + 1) == 0 {
                        cx.waker().clone().wake();
                        Poll::Pending
                    } else {
                        Poll::Ready(())
                    }
                },
                Some(Instant::now() + Duration::from_millis(500))
            ),
            Poll::Ready(())
        );
        assert_eq!(calls.get(), 2);
    }

    #[test]
    fn thread_suspend_wake_delayed() {
        let count = 2;
        let calls = Cell::new(0);
        assert_eq!(
            thread_suspend_deadline(
                |cx| {
                    if calls.replace(calls.get() + 1) + 1 < count {
                        let waker = cx.waker().clone();
                        thread::spawn(move || {
                            thread::sleep(Duration::from_millis(10));
                            waker.wake();
                        });
                        Poll::Pending
                    } else {
                        Poll::Ready(())
                    }
                },
                Some(Instant::now() + Duration::from_millis(500))
            ),
            Poll::Ready(())
        );
        assert_eq!(calls.get(), count);
    }

    #[test]
    fn thread_suspend_reenter() {
        let calls = Cell::new(0);
        let expire = Instant::now() + Duration::from_millis(500);
        assert_eq!(
            thread_suspend_deadline(
                |cx1| {
                    let seqno0 = THREAD_WAKE_HANDLE.with(|w| w.load_seqno());
                    assert_eq!(seqno0, WAKE_STATE_IDLE);

                    // acquire thread local instance
                    let _waker = cx1.waker().clone();
                    let seqno1 = THREAD_WAKE_HANDLE.with(|w| w.load_seqno());
                    assert!(seqno1 >= MIN_SEQNO);

                    thread_suspend_deadline(
                        |cx2| {
                            // should have acquired a new instance, leaving thread-local
                            // instance in the same state
                            let seqno2 = THREAD_WAKE_HANDLE.with(|w| w.load_seqno());
                            assert_eq!(seqno2, seqno1);

                            if calls.replace(calls.get() + 1) == 0 {
                                let waker = cx2.waker().clone();
                                thread::spawn(move || {
                                    thread::sleep(Duration::from_millis(10));
                                    waker.wake();
                                });
                                Poll::Pending
                            } else {
                                Poll::Ready(())
                            }
                        },
                        Some(expire),
                    )
                },
                Some(expire)
            ),
            Poll::Ready(())
        );
        assert_eq!(calls.get(), 2);
    }
}
