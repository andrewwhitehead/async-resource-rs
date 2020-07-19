use std::cell::UnsafeCell;
use std::mem::MaybeUninit;
use std::sync::{
    atomic::{AtomicU8, Ordering},
    Arc,
};
use std::task::Waker as TaskWaker;
use std::thread;
use std::time::Instant;

use futures_util::task::{waker, ArcWake};

const IDLE: u8 = 0;
const BUSY: u8 = 1;
const WAKE: u8 = 2;

pub fn pair() -> (Waker, Waiter) {
    let inner = Arc::new(Inner {
        state: AtomicU8::new(BUSY),
        thread: UnsafeCell::new(MaybeUninit::uninit()),
    });
    (
        Waker {
            inner: inner.clone(),
        },
        Waiter { inner },
    )
}

struct Inner {
    state: AtomicU8,
    thread: UnsafeCell<MaybeUninit<thread::Thread>>,
}

impl Inner {
    pub fn wake(self: &Arc<Self>) {
        if self.state.swap(BUSY, Ordering::Release) == WAKE {
            unsafe { self.thread.get().read().assume_init() }.unpark()
        }
    }
}

impl ArcWake for Inner {
    fn wake_by_ref(arc_self: &Arc<Self>) {
        arc_self.wake()
    }
}

unsafe impl Sync for Inner {}

pub struct Waker {
    inner: Arc<Inner>,
}

impl Waker {
    pub fn task_waker(&self) -> TaskWaker {
        waker(self.inner.clone())
    }

    pub fn wake(&self) {
        (&self.inner).wake()
    }
}

impl Clone for Waker {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

pub struct Waiter {
    inner: Arc<Inner>,
}

impl Waiter {
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
