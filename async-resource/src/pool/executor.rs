use futures_util::future::BoxFuture;

use super::error::ConfigError;

pub trait Executor: Send + Sync {
    fn spawn_ok(&self, task: BoxFuture<'static, ()>);
}

#[cfg(feature = "multitask-exec")]
mod exec_multitask {
    use super::BoxFuture;
    use super::Executor;
    use crate::pool::Sentinel;
    use crate::util::thread_waker;
    use multitask::Executor as MTExecutor;
    use option_lock::{self, OptionLock};
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    };
    use std::thread;

    const GLOBAL_INST: OptionLock<MultitaskExecutor> = OptionLock::new();

    pub struct MultitaskExecutor {
        inner: Sentinel<(
            MTExecutor,
            Vec<(thread::JoinHandle<()>, thread_waker::Waker)>,
        )>,
    }

    impl MultitaskExecutor {
        pub fn new(threads: usize) -> Self {
            assert_ne!(threads, 0);
            let ex = MTExecutor::new();
            let running = Arc::new(AtomicBool::new(true));
            let tickers = (0..threads)
                .map(|_| {
                    let (waker, waiter) = thread_waker::pair();
                    let waker_copy = waker.clone();
                    let ticker = ex.ticker(move || waker_copy.wake());
                    let running = running.clone();
                    (
                        thread::spawn(move || loop {
                            if !ticker.tick() {
                                waiter.prepare_wait();
                                if !running.load(Ordering::Acquire) {
                                    break;
                                }
                                waiter.wait();
                            }
                        }),
                        waker,
                    )
                })
                .collect();

            Self {
                inner: Sentinel::new(Arc::new((ex, tickers)), move |inner, count| {
                    if count == 0 {
                        running.store(false, Ordering::Release);
                        if let Ok((_, threads)) = Arc::try_unwrap(inner) {
                            for (thread, waker) in threads {
                                waker.wake();
                                thread.join().unwrap();
                            }
                        } else {
                            panic!("Error unwrapping executor state")
                        }
                    }
                }),
            }
        }

        pub fn global() -> Self {
            loop {
                match GLOBAL_INST.try_read() {
                    Ok(read) => break read.clone(),
                    Err(option_lock::ReadError::Empty) => {
                        if let Ok(mut guard) = GLOBAL_INST.try_lock() {
                            let inst = Self::new(5);
                            guard.replace(inst.clone());
                            break inst;
                        }
                    }
                    Err(_) => {}
                }
                // wait for another thread to populate the instance
                std::thread::yield_now();
            }
        }
    }

    impl Clone for MultitaskExecutor {
        fn clone(&self) -> Self {
            Self {
                inner: self.inner.clone(),
            }
        }
    }

    impl Executor for MultitaskExecutor {
        fn spawn_ok(&self, task: BoxFuture<'static, ()>) {
            self.inner.0.spawn(task).detach()
        }
    }
}

#[cfg(feature = "multitask-exec")]
pub use exec_multitask::MultitaskExecutor;

#[cfg(feature = "multitask-exec")]
pub fn default_executor() -> Result<Box<dyn Executor>, ConfigError> {
    Ok(Box::new(exec_multitask::MultitaskExecutor::global()))
}

#[cfg(not(any(feature = "multitask-exec")))]
pub fn default_executor() -> Result<Box<dyn Executor>, ConfigError> {
    Err(ConfigError("No default executor is provided"))
}
