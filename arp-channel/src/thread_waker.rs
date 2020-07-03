use std::marker::PhantomData;
use std::ops::Deref;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::task::{Context, Waker};
use std::thread;
use std::time::{Duration, Instant};

use futures_util::task::{waker, ArcWake};

pub fn thread_waker() -> (ThreadWaiter, ThreadWaker) {
    let inner = Arc::new(Inner {
        woken: AtomicBool::new(false),
        thread: thread::current(),
    });
    let waiter = ThreadWaiter {
        inner: inner.clone(),
        _marker: PhantomData,
    };
    let waker = ThreadWaker {
        inner: waker(inner),
    };
    (waiter, waker)
}

pub struct ThreadWaiter {
    inner: Arc<Inner>,
    // !Send and !Sync
    _marker: PhantomData<*mut u8>,
}

impl ThreadWaiter {
    pub fn wait(&self) {
        while !self.inner.woken.load(Ordering::SeqCst) {
            thread::park();
        }
        // reset for next wait
        self.inner.woken.store(false, Ordering::Release);
    }

    pub fn wait_timeout(&self, mut timeout: Duration) -> bool {
        let end = Instant::now() + timeout;
        loop {
            thread::park_timeout(timeout);
            if self.inner.woken.load(Ordering::SeqCst) {
                // reset for next wait
                self.inner.woken.store(false, Ordering::Release);
                return true;
            } else {
                let now = Instant::now();
                if now >= end {
                    return false;
                }
                timeout = end - now;
            }
        }
    }
}

pub struct ThreadWaker {
    inner: Waker,
}

impl ThreadWaker {
    pub fn to_context(&self) -> Context {
        Context::from_waker(&*self)
    }
}

impl Deref for ThreadWaker {
    type Target = Waker;
    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

struct Inner {
    woken: AtomicBool,
    thread: thread::Thread,
}

impl ArcWake for Inner {
    fn wake_by_ref(arc_self: &Arc<Self>) {
        if arc_self
            .woken
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::Acquire)
            .is_ok()
        {
            arc_self.thread.unpark();
        }
    }
}
