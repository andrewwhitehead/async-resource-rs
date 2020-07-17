use std::cell::UnsafeCell;
use std::mem::MaybeUninit;
use std::sync::{
    atomic::{AtomicU8, Ordering},
    Arc,
};
use std::thread;
use std::time::Instant;

const IDLE: u8 = 0;
const BUSY: u8 = 1;
const WAKE: u8 = 2;

pub fn shared_waker() -> (SharedWaker, SharedWaiter) {
    let inner = Arc::new(Inner {
        state: AtomicU8::new(BUSY),
        thread: UnsafeCell::new(MaybeUninit::uninit()),
    });
    (
        SharedWaker {
            inner: inner.clone(),
        },
        SharedWaiter { inner },
    )
}

struct Inner {
    state: AtomicU8,
    thread: UnsafeCell<MaybeUninit<thread::Thread>>,
}

unsafe impl Sync for Inner {}

pub struct SharedWaker {
    inner: Arc<Inner>,
}

impl SharedWaker {
    pub fn wake(&self) {
        if self.inner.state.swap(BUSY, Ordering::Release) == WAKE {
            unsafe { self.inner.thread.get().read().assume_init() }.unpark()
        }
    }
}

pub struct SharedWaiter {
    inner: Arc<Inner>,
}

impl SharedWaiter {
    pub fn prepare_wait(&self) {
        self.inner.state.store(IDLE, Ordering::Release);
        unsafe {
            self.inner
                .thread
                .get()
                .write(MaybeUninit::new(thread::current()))
        }
    }

    pub fn wait(&self) {
        if self
            .inner
            .state
            .compare_and_swap(IDLE, WAKE, Ordering::AcqRel)
            != IDLE
        {
            return;
        }
        loop {
            thread::park();
            if self.inner.state.load(Ordering::Acquire) == BUSY {
                break;
            }
        }
    }

    pub fn wait_until(&self, expire: Instant) -> bool {
        if self
            .inner
            .state
            .compare_and_swap(IDLE, WAKE, Ordering::AcqRel)
            != IDLE
        {
            return false;
        }
        while let Some(dur) = expire.checked_duration_since(Instant::now()) {
            thread::park_timeout(dur);
            if self.inner.state.load(Ordering::Acquire) == BUSY {
                return false;
            }
        }
        true
    }
}
