use std::cell::Cell;
use std::mem::ManuallyDrop;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc, Weak,
};
use std::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};
use std::thread::{self, Thread};
use std::time::Instant;

thread_local! {
    static THREAD_SUSPEND: (Arc<ThreadWake>, Waker, Cell<usize>) = {
        let tw = Arc::new(ThreadWake::new(0, thread::current()));
        let waker = tw.clone().into_waker();
        (tw, waker, Cell::new(1))
    };
}

#[inline]
fn with_thread_suspend<F, R>(f: F) -> R
where
    F: FnOnce(&ThreadWake, &Waker, usize) -> R,
{
    THREAD_SUSPEND.with(|(thread_wake, waker, seqno_source)| {
        let seqno = {
            let mut seq = seqno_source.get().wrapping_add(1);
            // handle overflow - need seqno > 1 because 0 and 1 are special values
            seq = std::cmp::max(seq, 2);
            seqno_source.replace(seq);
            seq
        };
        if thread_wake.acquire(seqno) {
            f(&*thread_wake, waker, seqno)
        } else {
            // if thread_wake is not idle, then this function has been re-entered while waiting
            let thread_wake = Arc::new(ThreadWake::new(seqno, thread_wake.thread.clone()));
            let waker = thread_wake.clone().into_waker();
            f(&*thread_wake, &waker, seqno)
        }
    })
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
    with_thread_suspend(|thread_wake, waker, seqno| {
        let mut cx = Context::from_waker(waker);
        loop {
            if let Poll::Ready(result) = f(&mut cx) {
                thread_wake.release();
                break Poll::Ready(result);
            } else {
                if !thread_wake.waiting(seqno) {
                    // waker was called directly by the polling function.
                    // poll again immediately before going into wait loop
                    if let Poll::Ready(result) = f(&mut cx) {
                        break Poll::Ready(result);
                    }
                }
                loop {
                    if let Some(expire) = expire.as_ref() {
                        if let Some(dur) = expire.checked_duration_since(Instant::now()) {
                            thread::park_timeout(dur);
                        } else {
                            // free up instance for next caller
                            thread_wake.release();
                            return Poll::Pending;
                        }
                    } else {
                        thread::park();
                    }
                    if thread_wake.restart(seqno) {
                        break;
                    }
                }
            }
        }
    })
}

struct ThreadWake {
    seqno: AtomicUsize,
    thread: Thread,
}

impl ThreadWake {
    const WAKER_VTABLE: RawWakerVTable = RawWakerVTable::new(
        Self::waker_clone,
        Self::waker_wake,
        Self::waker_wake_by_ref,
        Self::waker_drop,
    );

    const fn new(seqno: usize, thread: Thread) -> Self {
        Self {
            seqno: AtomicUsize::new(seqno),
            thread,
        }
    }

    #[inline]
    fn acquire(&self, seqno: usize) -> bool {
        self.seqno
            .compare_exchange(0, seqno, Ordering::AcqRel, Ordering::Relaxed)
            .is_ok()
    }

    #[inline]
    fn restart(&self, seqno: usize) -> bool {
        self.seqno
            .compare_exchange(1, seqno, Ordering::Release, Ordering::Relaxed)
            .is_ok()
    }

    #[inline]
    fn waiting(&self, seqno: usize) -> bool {
        self.seqno.load(Ordering::Acquire) == seqno
    }

    #[inline]
    fn notify(&self, seqno: usize) -> bool {
        self.seqno
            .compare_exchange(seqno, 1, Ordering::AcqRel, Ordering::Relaxed)
            .is_ok()
    }

    #[inline]
    fn release(&self) {
        self.seqno.store(0, Ordering::Release);
    }

    #[inline]
    fn into_waker(self: Arc<Self>) -> Waker {
        let data = Arc::into_raw(self) as *const ();
        unsafe { Waker::from_raw(RawWaker::new(data, &Self::WAKER_VTABLE)) }
    }

    unsafe fn waker_clone(data: *const ()) -> RawWaker {
        let slf = ManuallyDrop::new(Arc::from_raw(data as *const Self));
        Box::new(ThreadWakeRef {
            seqno: slf.seqno.load(Ordering::Acquire),
            inst: Arc::downgrade(&*slf),
        })
        .into_raw_waker()
    }

    unsafe fn waker_wake(_data: *const ()) {
        // the waker itself is owned by this struct, so it should not be
        // possible to wake it directly, only by reference
        unimplemented!();
    }

    unsafe fn waker_wake_by_ref(data: *const ()) {
        // if this instance is available to be woken by reference, then the
        // sequence number is either seqno or 0, we don't need to worry about
        // comparing and swapping it
        (&*(data as *const Self)).seqno.store(0, Ordering::Release);
    }

    unsafe fn waker_drop(data: *const ()) {
        Arc::from_raw(data as *const Self);
    }
}

#[derive(Clone)]
struct ThreadWakeRef {
    seqno: usize,
    inst: Weak<ThreadWake>,
}

impl ThreadWakeRef {
    const WAKER_VTABLE: RawWakerVTable = RawWakerVTable::new(
        Self::waker_clone,
        Self::waker_wake,
        Self::waker_wake_by_ref,
        Self::waker_drop,
    );

    fn into_raw_waker(self: Box<Self>) -> RawWaker {
        let data = Box::into_raw(self) as *const ();
        RawWaker::new(data, &Self::WAKER_VTABLE)
    }

    unsafe fn waker_clone(data: *const ()) -> RawWaker {
        let slf = &*(data as *const Self);
        Box::new(slf.clone()).into_raw_waker()
    }

    unsafe fn waker_wake(data: *const ()) {
        Self::waker_wake_by_ref(data);
        Self::waker_drop(data);
    }

    unsafe fn waker_wake_by_ref(data: *const ()) {
        let slf = &*(data as *const Self);
        if slf.seqno == 0 {
            // waker was called before it was cloned
            return;
        }
        if let Some(source) = slf.inst.upgrade() {
            if source.notify(slf.seqno) {
                source.thread.unpark();
            }
        }
    }

    unsafe fn waker_drop(data: *const ()) {
        Box::from_raw(data as *mut Self);
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
                Some(Instant::now() + Duration::from_millis(50))
            ),
            Poll::Ready(())
        );
        assert_eq!(calls.get(), 2);
    }

    #[test]
    fn thread_suspend_wake_delayed() {
        let calls = Cell::new(0);
        assert_eq!(
            thread_suspend_deadline(
                |cx| {
                    if calls.replace(calls.get() + 1) == 0 {
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
        assert_eq!(calls.get(), 2);
    }

    #[test]
    fn thread_suspend_reenter() {
        let calls = Cell::new(0);
        let expire = Instant::now() + Duration::from_millis(500);
        assert_eq!(
            thread_suspend_deadline(
                |_cx| {
                    thread_suspend_deadline(
                        |cx| {
                            if calls.replace(calls.get() + 1) == 0 {
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
