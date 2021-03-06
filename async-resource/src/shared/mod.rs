use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use concurrent_queue::ConcurrentQueue;
use suspend::Notifier;

use super::resource::{ResourceGuard, ResourceInfo, ResourceLock};

pub type ReleaseFn<T> = Box<dyn Fn(&mut T, ResourceInfo) -> bool + Send + Sync>;
pub type DisposeFn<T> = Box<dyn Fn(T, ResourceInfo) + Send + Sync>;

pub enum SharedEvent<T> {
    Created(ResourceLock<T>),
    Verify(Instant, ResourceLock<T>),
}

pub struct Shared<T> {
    count: AtomicUsize,
    dispose_count: AtomicUsize,
    event_queue: ConcurrentQueue<SharedEvent<T>>,
    idle_queue: ConcurrentQueue<ResourceLock<T>>,
    idle_timeout: Option<Duration>,
    max_count: usize,
    min_count: usize,
    max_waiters: Option<usize>,
    notifier: Notifier,
    on_dispose: Option<DisposeFn<T>>,
    on_release: Option<ReleaseFn<T>>,
    waiter_count: AtomicUsize,
}

impl<T> Shared<T> {
    pub fn new(
        notifier: Notifier,
        on_release: Option<ReleaseFn<T>>,
        on_dispose: Option<DisposeFn<T>>,
        min_count: usize,
        max_count: usize,
        max_waiters: Option<usize>,
        idle_timeout: Option<Duration>,
    ) -> Self {
        Self {
            count: AtomicUsize::new(0),
            dispose_count: AtomicUsize::new(0),
            event_queue: ConcurrentQueue::unbounded(),
            idle_queue: ConcurrentQueue::unbounded(),
            idle_timeout,
            max_count,
            min_count,
            max_waiters,
            notifier,
            on_dispose,
            on_release,
            waiter_count: AtomicUsize::new(0),
        }
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

    pub fn can_reuse(&self) -> bool {
        self.idle_timeout.is_some()
    }

    #[inline]
    fn check_reuse(&self, guard: &mut ResourceGuard<T>) -> bool {
        let min_count = self.min_count();
        if guard.is_some()
            && !guard.info().expired
            && (guard.info().reusable
                || self.is_busy()
                || (min_count > 0 && self.count() <= min_count))
        {
            if let Some(check) = self.on_release.as_ref() {
                let info = *guard.info();
                (check)(guard.as_mut().unwrap(), info)
            } else {
                true
            }
        } else {
            false
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
        self.waiter_count.load(Ordering::Acquire) > 0
    }

    pub fn max_count(&self) -> usize {
        self.max_count
    }

    pub fn min_count(&self) -> usize {
        self.min_count
    }

    pub fn max_waiters(&self) -> Option<usize> {
        self.max_waiters.clone()
    }

    pub fn notify(&self) {
        // Wake the manager thread if it is parked
        self.notifier.notify();
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

    pub fn try_acquire_idle(&self) -> Option<ResourceGuard<T>> {
        while let Ok(res) = self.idle_queue.pop() {
            // FIXME limit the number of attempts to avoid blocking in async?
            if let Some(guard) = res.try_lock() {
                if guard.is_some() {
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

    pub fn try_increment_waiters(&self) -> Option<usize> {
        let mut count = self.waiter_count.load(Ordering::Acquire);
        loop {
            if self.max_waiters().map(|w| w > count).unwrap_or(true) {
                match self.waiter_count.compare_exchange_weak(
                    count,
                    count + 1,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                ) {
                    Ok(_) => {
                        break Some(count + 1);
                    }
                    Err(c) => {
                        count = c;
                    }
                }
            } else {
                break None;
            }
        }
    }

    pub fn decrement_waiters(&self) -> usize {
        self.waiter_count.fetch_sub(1, Ordering::AcqRel) - 1
    }
}
