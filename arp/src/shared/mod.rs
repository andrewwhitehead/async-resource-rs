use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use concurrent_queue::ConcurrentQueue;

use super::resource::{ResourceGuard, ResourceLock};

mod waker;
pub use waker::{shared_waker, SharedWaiter, SharedWaker};

pub(crate) enum SharedEvent<T> {
    Created(ResourceLock<T>),
    Disposed(ResourceLock<T>),
    Verify(Instant, ResourceLock<T>),
}

pub struct Shared<T> {
    pub(crate) busy: AtomicBool,
    pub(crate) count: AtomicUsize,
    pub(crate) event_queue: ConcurrentQueue<SharedEvent<T>>,
    pub(crate) idle_queue: ConcurrentQueue<ResourceLock<T>>,
    pub(crate) idle_timeout: Option<Duration>,
    pub(crate) max_count: usize,
    pub(crate) min_count: usize,

    pub(crate) waker: SharedWaker,
}

impl<T> Shared<T> {
    pub fn new(
        min_count: usize,
        max_count: usize,
        idle_timeout: Option<Duration>,
    ) -> (Self, SharedWaiter) {
        let (waker, waiter) = shared_waker();
        (
            Self {
                busy: AtomicBool::new(false),
                count: AtomicUsize::new(0),
                event_queue: ConcurrentQueue::unbounded(),
                idle_queue: ConcurrentQueue::unbounded(),
                idle_timeout,
                max_count,
                min_count,
                waker,
            },
            waiter,
        )
    }

    pub fn notify(&self) {
        // Wake the manager thread if it is parked
        self.waker.wake();
    }

    pub fn release(&self, mut res: ResourceGuard<T>) {
        if res.is_none() {
            // The resource was disposed, reduce the current count.
            self.count.fetch_sub(1, Ordering::AcqRel);

            // Trigger removal of the entry from the repo.
            // The queue is never closed or full, so this should not fail.
            self.event_queue
                .push(SharedEvent::Disposed(res.unlock()))
                .unwrap_or(());
        } else {
            let now = Instant::now();
            let verify_at = self.idle_timeout.clone().map(|dur| now + dur);
            res.info().last_idle.replace(now);
            res.info().verify_at = verify_at;
            let lock = res.unlock();
            let busy = self.busy.load(Ordering::Acquire);

            // Add a verify timer.
            // When there is no idle timeout and the queue is not busy, the
            // verify timer serves to let the manager dispose of the resource
            // instead of returning it to the idle queue.
            if verify_at.is_some() || !busy {
                // The queue is never closed or full, so this should not fail.
                self.event_queue
                    .push(SharedEvent::Verify(
                        verify_at.clone().unwrap_or_else(|| Instant::now()),
                        lock.clone(),
                    ))
                    .unwrap_or(());
            }

            // Return the resource to the idle queue.
            if verify_at.is_some() || busy {
                // The queue is never closed or full, so this should not fail.
                // If it does then the resource is simply dropped.
                self.idle_queue.push(lock).unwrap_or(());
            }
        }

        // Wake the manager because either an idle entry was added
        // or the count was decreased
        self.notify();
    }

    pub fn try_acquire_idle(&self) -> Option<ResourceGuard<T>> {
        while let Ok(res) = self.idle_queue.pop() {
            // FIXME limit the number of attempts to avoid blocking async?
            if let Some(mut guard) = res.try_lock() {
                if guard.is_some() {
                    guard.info().last_borrow.replace(Instant::now());
                    guard.info().borrow_count += 1;
                    // FIXME Cancel outstanding expiry timer?
                    return Some(guard);
                } else {
                    // Drop the entry - it was taken by the manager thread.
                }
            } else {
                // Drop the entry - currently locked by manager thread
                // and will be disposed or recreated as a new entry.
            }
        }
        // No resources in the queue
        None
    }
}
