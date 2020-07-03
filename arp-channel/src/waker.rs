use std::sync::Arc;
use std::task::{Context, Waker};
use std::thread;

use futures_util::task::{waker, ArcWake};

use super::OptionLock;

struct UnparkWakerInner {
    lock: OptionLock<thread::Thread>,
}

impl UnparkWakerInner {
    pub fn new(thread: Option<thread::Thread>) -> Self {
        Self {
            lock: OptionLock::new(thread),
        }
    }

    pub fn register(&self, thread: thread::Thread) {
        if let Some(mut guard) = self.lock.try_lock() {
            guard.replace(thread);
        } else {
            // another thread is calling the waker, just unpark the thread
            thread.unpark();
        }
    }

    #[allow(unused)]
    pub fn cancel(&self) {
        self.lock.try_take();
        // ignore if the waker was already being called
    }
}

impl ArcWake for UnparkWakerInner {
    fn wake_by_ref(arc_self: &Arc<Self>) {
        if let Some(thread) = arc_self.lock.try_take() {
            thread.unpark();
        }
    }
}

pub struct UnparkWaker {
    generic: Waker,
    inner: Arc<UnparkWakerInner>,
}

impl UnparkWaker {
    pub fn new() -> Self {
        let inner = Arc::new(UnparkWakerInner::new(Some(thread::current())));
        Self {
            generic: waker(inner.clone()),
            inner,
        }
    }

    pub fn context(&self) -> Context {
        Context::from_waker(self.waker())
    }

    pub fn waker(&self) -> &Waker {
        self.inner.register(thread::current());
        &self.generic
    }
}
