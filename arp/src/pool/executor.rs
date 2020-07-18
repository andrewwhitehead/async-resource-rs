use futures_util::future::BoxFuture;

pub trait Executor: Send + Sync {
    fn spawn_ok(&self, task: BoxFuture<'static, ()>);
}

#[cfg(feature = "exec-multitask")]
mod exec_multitask {
    use super::BoxFuture;
    use super::Executor;
    use crate::pool::Sentinel;
    use crate::util::{option_lock::OptionLock, thread_waker};
    use multitask::Executor as MTExecutor;
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    };
    use std::thread;

    const GLOBAL_INST: OptionLock<MultitaskExecutor> = OptionLock::empty();

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
                                    println!("shut down thread");
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
                        println!("shut down exec");
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
                if let Some(mut guard) = GLOBAL_INST.try_lock() {
                    if guard.is_some() {
                        break guard.as_ref().unwrap().clone();
                    } else {
                        let inst = MultitaskExecutor::new(5);
                        guard.replace(inst.clone());
                        break inst;
                    }
                }
                thread::yield_now();
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

#[cfg(feature = "exec-multitask")]
pub use exec_multitask::MultitaskExecutor;

#[cfg(feature = "exec-multitask")]
pub fn default_executor() -> Box<dyn Executor> {
    Box::new(exec_multitask::MultitaskExecutor::global())
}
