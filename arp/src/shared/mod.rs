use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use concurrent_queue::ConcurrentQueue;

use super::resource::{ResourceGuard, ResourceInfo, ResourceLock};

mod waker;
pub use waker::{shared_waker, SharedWaiter, SharedWaker};

pub type ReleaseFn<T> = Box<dyn Fn(&mut T, ResourceInfo) -> bool + Send + Sync>;
pub type DisposeFn<T> = Box<dyn Fn(T, ResourceInfo) + Send + Sync>;

pub enum SharedEvent<T> {
    Created(ResourceLock<T>),
    Verify(Instant, ResourceLock<T>),
}

pub struct Shared<T> {
    busy: AtomicBool,
    count: AtomicUsize,
    dispose_count: AtomicUsize,
    event_queue: ConcurrentQueue<SharedEvent<T>>,
    idle_queue: ConcurrentQueue<ResourceLock<T>>,
    idle_timeout: Option<Duration>,
    max_count: usize,
    min_count: usize,
    on_dispose: Option<DisposeFn<T>>,
    on_release: Option<ReleaseFn<T>>,
    waker: SharedWaker,
}

impl<T> Shared<T> {
    pub fn new(
        on_release: Option<ReleaseFn<T>>,
        on_dispose: Option<DisposeFn<T>>,
        min_count: usize,
        max_count: usize,
        idle_timeout: Option<Duration>,
    ) -> (Self, SharedWaiter) {
        let (waker, waiter) = shared_waker();
        (
            Self {
                busy: AtomicBool::new(false),
                count: AtomicUsize::new(0),
                dispose_count: AtomicUsize::new(0),
                event_queue: ConcurrentQueue::unbounded(),
                idle_queue: ConcurrentQueue::unbounded(),
                idle_timeout,
                max_count,
                min_count,
                on_dispose,
                on_release,
                waker,
            },
            waiter,
        )
    }

    pub fn dispose(&self, mut guard: ResourceGuard<T>) {
        // Reduce the current count
        self.count.fetch_sub(1, Ordering::AcqRel);

        if let Some(res) = guard.take() {
            // Call on_dispose hook, if any
            if let Some(dispose) = self.on_dispose.as_ref() {
                (dispose)(res, *guard.info())
            }
        }

        // Trigger cleanup of the repo
        self.dispose_count.fetch_add(1, Ordering::AcqRel);

        // Wake the manager to trigger timer and waiter processing
        self.notify();
    }

    #[inline]
    fn check_reuse(&self, guard: &mut ResourceGuard<T>) -> bool {
        if guard.is_some() && guard.info().reusable || self.busy.load(Ordering::Acquire) {
            if let Some(check) = self.on_release.as_ref() {
                let info = *guard.info();
                (check)(guard.as_mut().unwrap(), info)
            } else {
                true
            }
        } else {
            self.busy.load(Ordering::Acquire)
        }
    }

    pub fn count(&self) -> usize {
        self.count.load(Ordering::Acquire)
    }

    pub fn dispose_count(&self) -> usize {
        self.dispose_count.load(Ordering::Acquire)
    }

    pub fn have_idle(&self) -> bool {
        !self.idle_queue.is_empty()
    }

    pub fn is_busy(&self) -> bool {
        self.busy.load(Ordering::Acquire)
    }

    pub fn max_count(&self) -> usize {
        self.max_count
    }

    pub fn min_count(&self) -> usize {
        self.min_count
    }

    pub fn notify(&self) {
        // Wake the manager thread if it is parked
        self.waker.wake();
    }

    pub fn pop_event(&self) -> Option<SharedEvent<T>> {
        self.event_queue.pop().ok()
    }

    pub fn push_event(&self, event: SharedEvent<T>) {
        self.event_queue.push(event).unwrap_or(())
    }

    pub fn release(&self, mut guard: ResourceGuard<T>) {
        let now = Instant::now();
        guard.info_mut().last_idle.replace(now);

        if !self.check_reuse(&mut guard) {
            self.dispose(guard);
        } else {
            let verify_at = self.idle_timeout.clone().map(|dur| now + dur);
            guard.info_mut().verify_at = verify_at;
            let lock = guard.unlock();

            // Add a verify timer
            if verify_at.is_some() {
                // The queue is never closed or full, so this should not fail
                self.event_queue
                    .push(SharedEvent::Verify(
                        verify_at.clone().unwrap_or_else(|| Instant::now()),
                        lock.clone(),
                    ))
                    .unwrap_or(());
            }

            // Return the resource to the idle queue
            // The queue is never closed or full, so this should not fail
            // If it does then the resource is simply dropped
            self.idle_queue.push(lock).unwrap_or(());

            // Wake the manager to trigger timer and waiter processing
            self.notify();
        }
    }

    pub fn set_busy(&self, busy: bool) -> bool {
        self.busy.swap(busy, Ordering::Release)
    }

    pub fn try_acquire_idle(&self) -> Option<ResourceGuard<T>> {
        while let Ok(res) = self.idle_queue.pop() {
            // FIXME limit the number of attempts to avoid blocking async?
            if let Some(mut guard) = res.try_lock() {
                if guard.is_some() {
                    guard.info_mut().last_acquire.replace(Instant::now());
                    guard.info_mut().acquire_count += 1;
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

    pub fn try_update_count(&self, prev_count: usize, count: usize) -> Result<usize, usize> {
        self.count
            .compare_exchange_weak(prev_count, count, Ordering::SeqCst, Ordering::Acquire)
    }
}
