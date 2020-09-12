use futures_lite::future::Boxed as BoxFuture;

use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Arc,
};
use std::thread;

use async_executor::Executor as AsyncExecutor;
use event_listener::Event;
use option_lock::{self, OptionLock};

use super::Executor;
use crate::util::sentinel::Sentinel;

const GLOBAL_INST: OptionLock<BoundedExecutor> = OptionLock::new();

struct Inner {
    active: AtomicUsize,
    exec: AsyncExecutor,
    running: AtomicBool,
    shutdown: Event,
}

impl Inner {
    fn run_thread(self: Arc<Self>) -> thread::JoinHandle<()> {
        thread::spawn(move || {
            let inner = Sentinel::new(self, |inner, _| {
                inner.active.fetch_sub(1, Ordering::Release);
                if thread::panicking() && inner.running.load(Ordering::Acquire) {
                    // start a new worker thread
                    inner.run_thread();
                }
            });
            inner.active.fetch_add(1, Ordering::Release);
            suspend::block_on(inner.exec.run(inner.shutdown.listen()))
        })
    }
}

pub struct BoundedExecutor {
    inner: Sentinel<Inner>,
}

impl BoundedExecutor {
    pub fn new(threads: usize) -> Self {
        assert_ne!(threads, 0);
        let inner = Arc::new(Inner {
            active: AtomicUsize::new(0),
            exec: AsyncExecutor::new(),
            running: AtomicBool::new(false),
            shutdown: Event::new(),
        });
        for _ in 0..threads {
            inner.clone().run_thread();
        }

        Self {
            inner: Sentinel::new(inner, move |inner, count| {
                if count == 0 {
                    inner.running.store(false, Ordering::Release);
                    // FIXME loop until active is 0, add a waker to allow await of the executor
                    inner
                        .shutdown
                        .notify(inner.active.load(Ordering::Acquire) + 1);
                }
            }),
        }
    }

    pub fn global() -> Self {
        loop {
            match GLOBAL_INST.try_read() {
                Ok(read) => break read.clone(),
                Err(option_lock::TryReadError::Empty) => {
                    if let Ok(mut guard) = GLOBAL_INST.try_lock() {
                        let inst = Self::new(5);
                        guard.replace(inst.clone());
                        break inst;
                    }
                }
                Err(option_lock::TryReadError::Locked) => {
                    // wait for another thread to populate the instance
                    std::thread::yield_now();
                }
            }
        }
    }
}

impl Clone for BoundedExecutor {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

impl Default for BoundedExecutor {
    fn default() -> Self {
        Self::new(num_cpus::get())
    }
}

impl Executor for BoundedExecutor {
    fn spawn_ok(&self, task: BoxFuture<()>) {
        self.inner.exec.spawn(task).detach()
    }
}
