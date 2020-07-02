use std::sync::Arc;
use std::thread;

use futures_util::task::ArcWake;

use super::OptionLock;

pub struct UnparkWaker {
    lock: OptionLock<thread::Thread>,
}

impl UnparkWaker {
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

impl ArcWake for UnparkWaker {
    fn wake_by_ref(arc_self: &Arc<Self>) {
        if let Some(thread) = arc_self.lock.try_take() {
            thread.unpark();
        }
    }
}
