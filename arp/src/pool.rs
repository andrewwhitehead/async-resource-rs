use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc, Mutex,
};

use concurrent_queue::ConcurrentQueue;

use super::acquire::Acquire;

use super::resource::{Lifecycle, ResourceFuture, ResourceGuard, ResourceInfo, ResourceLock};

pub(crate) struct PoolState<T> {
    count: AtomicUsize,
    idle_queue: ConcurrentQueue<ResourceLock<T>>,
    max_count: Option<usize>,
    repo: Mutex<Vec<ResourceLock<T>>>,
}

impl<T> PoolState<T> {
    pub fn try_acquire_idle(&self) -> Option<ResourceGuard<T>> {
        while let Ok(res) = self.idle_queue.pop() {
            // FIXME limit the number of attempts to avoid blocking async?
            if let Some(guard) = res.try_lock() {
                if guard.is_some() {
                    return Some(guard);
                } else {
                    // Drop the entry - it was taken by the manager thread.
                }
            } else {
                // Drop the entry - currently locked by manager thread
                // and will be disposed or recreated as a new entry.
            }
        }
        None
    }

    pub fn release(&self, res: ResourceGuard<T>) {
        // The queue is never closed or full, so this should not fail.
        // If it does then the resource is simply dropped.
        self.idle_queue.push(res.unlock()).unwrap_or(());
    }
}

pub(crate) struct Inner<T, E> {
    state: Arc<PoolState<T>>,
    lifecycle: Lifecycle<T, E>,
    // wait_queue: ConcurrentQueue<Waiter>,
}

impl<T, E> Inner<T, E> {
    pub fn new(lifecycle: Lifecycle<T, E>, max_count: Option<usize>) -> Self {
        let state = Arc::new(PoolState {
            count: AtomicUsize::new(0),
            idle_queue: ConcurrentQueue::unbounded(),
            max_count,
            repo: Mutex::new(Vec::new()),
        });
        Self { lifecycle, state }
    }

    pub fn try_acquire_idle(&self) -> Option<ResourceGuard<T>> {
        self.state.try_acquire_idle()
    }

    pub fn try_create(&self) -> Option<ResourceFuture<T, E>> {
        let mut count = self.state.count.load(Ordering::Acquire);
        if !self
            .state
            .max_count
            .clone()
            .map(|max| max > count)
            .unwrap_or(true)
        {
            return None;
        }

        // Allocate up-front to reduce the time spent holding the lock
        let lock = ResourceLock::new(ResourceInfo::default(), None);
        let guard = lock.try_lock().unwrap();

        if let Ok(mut repo) = self.state.repo.try_lock() {
            loop {
                match self.state.count.compare_exchange_weak(
                    count,
                    count + 1,
                    Ordering::SeqCst,
                    Ordering::Acquire,
                ) {
                    Ok(_) => {
                        repo.push(guard.as_lock());
                        drop(repo);
                        let fut = self.lifecycle.create(guard);
                        break Some(fut);
                    }
                    Err(c) => {
                        if c > count
                            && !self
                                .state
                                .max_count
                                .clone()
                                .map(|max| max > c)
                                .unwrap_or(true)
                        {
                            // Count was increased beyond max by another thread
                            break None;
                        }
                        count = c;
                        continue;
                    }
                }
            }
        } else {
            None
        }
    }
}

pub struct Pool<T, E> {
    pub(crate) inner: Arc<Inner<T, E>>,
}

impl<T, E> Pool<T, E> {
    pub(crate) fn new(inner: Inner<T, E>) -> Self {
        Self {
            inner: Arc::new(inner),
        }
    }

    pub fn acquire(&self) -> Acquire<T, E> {
        Acquire::new(self.clone())
    }

    pub(crate) fn state(&self) -> &Arc<PoolState<T>> {
        &self.inner.state
    }
}

impl<T, E> Clone for Pool<T, E> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}
