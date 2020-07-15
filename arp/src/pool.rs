use std::collections::{BTreeMap, HashSet, VecDeque};
use std::sync::{
    atomic::{AtomicU8, Ordering},
    Arc,
};
use std::time::{Duration, Instant};

use concurrent_queue::ConcurrentQueue;

use futures_util::FutureExt;

use super::acquire::Acquire;
use super::executor::Executor;
use super::queue::{Queue, QueueEvent};
use super::resource::{Lifecycle, ResourceFuture, ResourceInfo, ResourceLock, ResourceResolve};
use super::wait::WaitResponder;
use super::waker::QueueWaiter;

const INIT: u8 = 0;
const ACTIVE: u8 = 1;
const SHUTDOWN: u8 = 2;
const STOPPED: u8 = 3;

pub(crate) struct PoolManageState<T: Send + 'static, E: 'static> {
    acquire_timeout: Option<Duration>,
    executor: Executor,
    lifecycle: Lifecycle<T, E>,
    max_waiters: Option<usize>,
    queue_waiter: QueueWaiter,
    state: AtomicU8,
    wait_inject: ConcurrentQueue<(Instant, WaitResponder<T, E>)>,
}

pub struct PoolInner<T: Send + 'static, E: 'static> {
    manage: PoolManageState<T, E>,
    queue: Arc<Queue<T>>,
}

impl<T: Send, E> PoolInner<T, E> {
    pub fn new(
        lifecycle: Lifecycle<T, E>,
        acquire_timeout: Option<Duration>,
        idle_timeout: Option<Duration>,
        min_count: usize,
        max_count: Option<usize>,
        max_waiters: Option<usize>,
        thread_count: Option<usize>,
    ) -> Self {
        let executor = Executor::new(thread_count.unwrap_or(1));
        let (queue, queue_waiter) = Queue::new(min_count, max_count, idle_timeout);
        let manage = PoolManageState {
            acquire_timeout,
            executor,
            lifecycle,
            max_waiters,
            queue_waiter,
            state: AtomicU8::new(INIT),
            wait_inject: ConcurrentQueue::unbounded(),
        };
        Self {
            manage,
            queue: Arc::new(queue),
        }
    }

    pub fn handle_error(&self, err: E) {
        self.manage.lifecycle.handle_error(err);
    }

    fn manage(self: Arc<Self>) {
        let mut timer_id_source: usize = 0;
        let mut timers = BTreeMap::<(Instant, usize), Timer<T, E>>::new();
        let mut next_check = None;
        let mut repo = HashSet::<ResourceLock<T>>::new();
        let mut waiters = VecDeque::<WaitResponder<T, E>>::new();

        let mut state = match self
            .manage
            .state
            .compare_and_swap(INIT, ACTIVE, Ordering::AcqRel)
        {
            INIT => ACTIVE,
            other => other,
        };

        while state != STOPPED {
            let remain_timers = timers.split_off(&(Instant::now(), 0));
            for ((inst, _), timer) in timers {
                match timer {
                    Timer::Verify(res) => {
                        if let Some(mut guard) = res.try_lock() {
                            // The clone of the resource lock in the idle queue can't be
                            // acquired at the moment, so forget that lock and create a
                            // new one.
                            repo.remove(&guard.as_lock());
                            guard = guard.detach();

                            if guard.is_some() && guard.info().verify_at == Some(inst) {
                                // Act upon the verify timer
                                let op = if self.queue.count.load(Ordering::Acquire)
                                    > self.queue.min_count
                                {
                                    self.manage.lifecycle.dispose(guard, &self)
                                } else {
                                    self.manage.lifecycle.keepalive(guard, &self)
                                };
                                // Just give the future to the executor to complete
                                drop(op);
                            } else {
                                // The resource was reacquired and released after the
                                // verify timer was set.
                                repo.insert(guard.as_lock());
                                self.queue.release(guard);
                            }
                        } else {
                            // If the resource is locked, then a consumer thread
                            // has acquired it or is in the process of doing so,
                            // nothing to do here.
                        }
                    }
                    Timer::Waiter(waiter) => {
                        // Try to cancel the waiter. This will only succeed if the
                        // waiter hasn't been fulfilled already, or cancelled by the
                        // other side.
                        waiter.cancel();
                    }
                }
            }
            timers = remain_timers;

            if let Some(fst) = timers.keys().next() {
                next_check = Some(next_check.map_or(fst.0, |t| std::cmp::min(t, fst.0)))
            }

            // Set the waiter state to Idle.
            // If anything is subsequently added to the queues, it will move to Busy.
            self.manage.queue_waiter.prepare_wait();

            while let Ok(event) = self.queue.event_queue.pop() {
                match event {
                    QueueEvent::Created(res) => {
                        repo.insert(res);
                    }
                    QueueEvent::Disposed(res) => {
                        repo.remove(&res);
                    }
                    QueueEvent::Verify(expire, res) => {
                        timer_id_source += 1;
                        timers.insert((expire, timer_id_source), Timer::Verify(res));
                    }
                }
            }

            while let Ok((start, waiter)) = self.manage.wait_inject.pop() {
                if let Some(expire) = self.manage.acquire_timeout.clone().map(|dur| start + dur) {
                    timer_id_source += 1;
                    timers.insert((expire, timer_id_source), Timer::Waiter(waiter.clone()));
                }
                waiters.push_back(waiter);
            }

            if waiters.is_empty() {
                self.queue
                    .busy
                    .compare_and_swap(true, false, Ordering::Release);
            } else {
                while let Some(waiter) = waiters.pop_front() {
                    if waiter.is_canceled() {
                        continue;
                    }
                    // Bypasses busy check because we are satisfying waiters
                    if let Some(idle) = self.queue.try_acquire_idle() {}
                }
            }

            if let Some(next_check) = next_check {
                self.manage.queue_waiter.wait_until(next_check);
            } else {
                self.manage.queue_waiter.wait();
            }
            next_check = None;
        }
    }

    pub fn release_future(self: &Arc<Self>, fut: ResourceFuture<T, E>) {
        let inner = self.clone();
        self.manage.executor.spawn_ok(fut.map(move |res| match res {
            Ok(res) => {
                inner.queue.release(res);
            }
            Err(err) => {
                inner.handle_error(err);
            }
        }))
    }

    pub fn try_acquire_idle(self: &Arc<Self>) -> ResourceResolve<T, E> {
        if self.queue.busy.load(Ordering::Acquire) {
            None.into()
        } else {
            self.queue
                .try_acquire_idle()
                .map(|res| (res, self.queue.clone()))
                .into()
        }
    }

    pub fn try_create(self: &Arc<Self>) -> ResourceResolve<T, E> {
        let mut count = self.queue.count.load(Ordering::Acquire);
        if !self
            .queue
            .max_count
            .clone()
            .map(|max| max > count)
            .unwrap_or(true)
        {
            return None.into();
        }

        // Allocate up-front to reduce the time spent holding the lock
        let lock = ResourceLock::new(ResourceInfo::default(), None);
        let guard = lock.try_lock().unwrap();

        loop {
            match self.queue.count.compare_exchange_weak(
                count,
                count + 1,
                Ordering::SeqCst,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    // FIXME repo not required if there is no resource expiry?
                    // Not concerned with failure here
                    self.queue
                        .event_queue
                        .push(QueueEvent::Created(guard.as_lock()))
                        .unwrap_or(());
                    break self.manage.lifecycle.create(guard, self);
                }
                Err(c) => {
                    if c > count
                        && !self
                            .queue
                            .max_count
                            .clone()
                            .map(|max| max > c)
                            .unwrap_or(true)
                    {
                        // Count was increased beyond max by another thread
                        break None.into();
                    }
                    count = c;
                    continue;
                }
            }
        }
    }
}

enum Timer<T: Send + 'static, E: 'static> {
    Verify(ResourceLock<T>),
    Waiter(WaitResponder<T, E>),
}

pub struct Pool<T: Send + 'static, E: 'static> {
    pub(crate) inner: Arc<PoolInner<T, E>>,
}

impl<T: Send, E> Pool<T, E> {
    pub(crate) fn new(inner: PoolInner<T, E>) -> Self {
        Self {
            inner: Arc::new(inner),
        }
    }

    pub fn acquire(&self) -> Acquire<T, E> {
        Acquire::new(self.clone())
    }

    pub(crate) fn queue(&self) -> &Arc<Queue<T>> {
        &self.inner.queue
    }
}

impl<T: Send, E> Clone for Pool<T, E> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

// FIXME test behaviour of cloned WaitResponder + close
