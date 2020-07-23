use std::cell::UnsafeCell;
use std::future::Future;
use std::mem::MaybeUninit;
use std::pin::Pin;
use std::sync::{
    atomic::{AtomicU8, Ordering},
    Arc,
};
use std::task::{Context, Poll, Waker};
use std::thread;
use std::time::{Duration, Instant};

use futures_util::task::{waker, ArcWake};

const IDLE: u8 = 0x0;
const BUSY: u8 = 0x1;
const LISTEN: u8 = 0x2;

enum InnerNotify {
    Thread(thread::Thread),
    Waker(Waker),
}

enum ClearResult {
    Removed(InnerNotify),
    NoChange,
    Updated,
}

impl ClearResult {
    pub fn is_none(&self) -> bool {
        matches!(self, Self::NoChange | Self::Updated)
    }
}

struct Inner {
    notify: UnsafeCell<MaybeUninit<InnerNotify>>,
    state: AtomicU8,
}

impl Inner {
    pub fn acquire<'a>(self: &'a Arc<Self>) -> Option<Listener<'a>> {
        if self.state.compare_and_swap(BUSY, IDLE, Ordering::AcqRel) == BUSY {
            Some(Listener { inner: self })
        } else {
            None
        }
    }

    pub fn clear(&self, idle: bool) -> ClearResult {
        let newval = if idle { IDLE } else { BUSY };
        match self.state.swap(newval, Ordering::Release) {
            LISTEN => ClearResult::Removed(unsafe { self.vacate() }),
            state => {
                if state == newval {
                    ClearResult::NoChange
                } else {
                    ClearResult::Updated
                }
            }
        }
    }

    pub fn listen(&self, notify: InnerNotify) -> bool {
        match self.state() {
            IDLE => (),
            BUSY => {
                return false;
            }
            _ => panic!("Invalid state"),
        }
        unsafe {
            self.update(notify);
        }
        match self.state.compare_and_swap(IDLE, LISTEN, Ordering::AcqRel) {
            IDLE => true,
            BUSY => {
                // notify was called while storing
                unsafe {
                    self.vacate();
                }
                false
            }
            _ => panic!("Invalid state"),
        }
    }

    pub fn notify(&self) -> bool {
        match self.clear(false) {
            ClearResult::Removed(InnerNotify::Thread(thread)) => {
                thread.unpark();
                true
            }
            ClearResult::Removed(InnerNotify::Waker(waker)) => {
                waker.wake();
                true
            }
            ClearResult::Updated => true,
            ClearResult::NoChange => false,
        }
    }

    pub fn park(&self) {
        if !self.listen(InnerNotify::Thread(thread::current())) {
            // already notified
            return;
        }
        loop {
            thread::park();
            if self.state() == BUSY {
                break;
            }
        }
    }

    pub fn park_timeout(&self, timeout: Duration) -> bool {
        self.park_until(Instant::now() + timeout)
    }

    pub fn park_until(&self, expire: Instant) -> bool {
        if !self.listen(InnerNotify::Thread(thread::current())) {
            // already notified
            return false;
        }
        while let Some(dur) = expire.checked_duration_since(Instant::now()) {
            thread::park_timeout(dur);
            if self.state() == BUSY {
                // notified
                return false;
            }
        }
        // timer expired
        true
    }

    pub fn poll(&self, waker: &Waker) -> Poll<()> {
        match self.state() {
            BUSY => {
                return Poll::Ready(());
            }
            LISTEN => {
                // try to clear existing waker and move back to idle state
                if self.clear(true).is_none() {
                    // already taken (thread was pre-empted)
                    return Poll::Ready(());
                }
            }
            _ => (),
        }

        if self.listen(InnerNotify::Waker(waker.clone())) {
            Poll::Pending
        } else {
            // already notified
            Poll::Ready(())
        }
    }

    pub fn state(&self) -> u8 {
        self.state.load(Ordering::Acquire)
    }

    pub unsafe fn update(&self, notify: InnerNotify) {
        self.notify.get().write(MaybeUninit::new(notify))
    }

    pub unsafe fn vacate(&self) -> InnerNotify {
        self.notify.get().read().assume_init()
    }
}

impl ArcWake for Inner {
    fn wake_by_ref(arc_self: &Arc<Self>) {
        arc_self.notify();
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        // vacate any registered listener
        self.clear(false);
    }
}

unsafe impl Sync for Inner {}

pub struct NotifyOnce {
    inner: Inner,
}

impl NotifyOnce {
    pub const fn new() -> Self {
        Self {
            inner: Inner {
                notify: UnsafeCell::new(MaybeUninit::uninit()),
                state: AtomicU8::new(IDLE),
            },
        }
    }

    pub fn cancel(&self) -> bool {
        match self.inner.clear(false) {
            ClearResult::NoChange => false,
            _ => true,
        }
    }

    pub fn complete(&self) -> bool {
        self.inner.notify()
    }

    pub fn is_complete(&self) -> bool {
        self.inner.state() == BUSY
    }

    pub fn park(&self) {
        self.inner.park()
    }

    pub fn park_timeout(&self, timeout: Duration) -> bool {
        self.inner.park_timeout(timeout)
    }

    pub fn park_until(&self, expire: Instant) -> bool {
        self.inner.park_until(expire)
    }

    pub fn poll(&self, waker: &Waker) -> Poll<()> {
        self.inner.poll(waker)
    }
}

impl Future for NotifyOnce {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.inner.poll(cx.waker())
    }
}

pub struct Suspend {
    inner: Arc<Inner>,
}

impl Suspend {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Inner {
                notify: UnsafeCell::new(MaybeUninit::uninit()),
                state: AtomicU8::new(BUSY),
            }),
        }
    }

    pub fn notifier(&self) -> Notifier {
        Notifier {
            inner: self.inner.clone(),
        }
    }

    pub fn notify(&self) {
        self.inner.notify();
    }

    pub fn listen(&mut self) -> Listener<'_> {
        self.inner.acquire().expect("Invalid Suspend state")
    }

    pub fn try_listen(&self) -> Option<Listener<'_>> {
        self.inner.acquire()
    }

    pub fn waker(&self) -> Waker {
        waker(self.inner.clone())
    }
}

pub struct Listener<'a> {
    inner: &'a Arc<Inner>,
}

impl Listener<'_> {
    pub fn park(&mut self) {
        self.inner.park()
    }

    pub fn park_timeout(&self, timeout: Duration) -> bool {
        self.inner.park_timeout(timeout)
    }

    pub fn park_until(&mut self, expire: Instant) -> bool {
        self.inner.park_until(expire)
    }
}

impl Drop for Listener<'_> {
    fn drop(&mut self) {
        self.inner.clear(false);
    }
}

impl<'a> Future for Listener<'a> {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.inner.poll(cx.waker())
    }
}

pub struct Notifier {
    inner: Arc<Inner>,
}

impl Notifier {
    pub fn notify(&self) {
        self.inner.notify();
    }
}

impl Clone for Notifier {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}
