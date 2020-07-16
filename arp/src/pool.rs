use std::collections::{BTreeMap, HashSet, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::sync::{
    atomic::{AtomicU8, Ordering},
    Arc,
};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use concurrent_queue::ConcurrentQueue;

use futures_util::FutureExt;

use super::acquire::Acquire;
use super::executor::Executor;
use super::queue::{Queue, QueueEvent};
use super::resource::{Lifecycle, ResourceFuture, ResourceInfo, ResourceLock, ResourceResolve};
use super::sentinel::Sentinel;
use super::wait::{waiter_pair, WaitResponder, Waiter};
use super::waker::QueueWaiter;

const ACTIVE: u8 = 0;
const SHUTDOWN: u8 = 1;
const STOPPED: u8 = 2;

type WaitResource<T, E> = WaitResponder<ResourceResolve<T, E>>;

pub(crate) struct PoolManageState<T: Send + 'static, E: 'static> {
    acquire_timeout: Option<Duration>,
    executor: Executor,
    lifecycle: Lifecycle<T, E>,
    max_waiters: Option<usize>,
    queue_waiter: QueueWaiter,
    register_inject: ConcurrentQueue<Register<T, E>>,
    state: AtomicU8,
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
        max_count: usize,
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
            register_inject: ConcurrentQueue::unbounded(),
            state: AtomicU8::new(ACTIVE),
        };
        Self {
            manage,
            queue: Arc::new(queue),
        }
    }

    pub fn create_from_count(&self) -> Option<usize> {
        let count = self.queue.count.load(Ordering::Acquire);
        let max = self.queue.max_count;
        if max == 0 || max > count {
            Some(count)
        } else {
            None
        }
    }

    pub fn handle_error(&self, err: E) {
        self.manage.lifecycle.handle_error(err);
    }

    fn manage(self: Arc<Self>) {
        let inner = Sentinel::new(self, |inner, _| {
            // Set the state to Stopped when this thread exits, whether normally
            // or due to a panic.
            inner.manage.state.store(STOPPED, Ordering::Release);
        });
        let mut next_check = None;
        let mut repo = HashSet::<ResourceLock<T>>::new();
        let mut timer_id_source: usize = 0;
        let mut timers = BTreeMap::<(Instant, usize), Timer<T, E>>::new();
        let mut waiters = VecDeque::<WaitResource<T, E>>::new();

        loop {
            let remain_timers = timers.split_off(&(Instant::now(), 0));
            for ((inst, _), timer) in timers {
                match timer {
                    Timer::Shutdown(_) => {
                        // Just drop the shutdown timer to indicate we haven't finished
                    }
                    Timer::Verify(res) => {
                        if let Some(mut guard) = res.try_lock() {
                            // The clone of the resource lock in the idle queue can't be
                            // acquired at the moment, so forget that lock and create a
                            // new one.
                            repo.remove(&guard.as_lock());
                            guard = guard.detach();

                            if guard.is_some() && guard.info().verify_at == Some(inst) {
                                // Act upon the verify timer
                                let op = if inner.queue.count.load(Ordering::Acquire)
                                    > inner.queue.min_count
                                {
                                    inner.manage.lifecycle.dispose(guard, &inner)
                                } else {
                                    inner.manage.lifecycle.keepalive(guard, &inner)
                                };
                                inner.complete_resolve(op);
                            } else {
                                // The resource was reacquired and released after the
                                // verify timer was set.
                                repo.insert(guard.as_lock());
                                inner.queue.release(guard);
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
            // remain_timers.drain_filter(|_, timer| timer.is_canceled());
            timers = remain_timers;

            if let Some(fst) = timers.keys().next() {
                next_check = Some(next_check.map_or(fst.0, |t| std::cmp::min(t, fst.0)))
            }

            // Set the waiter state to Idle.
            // If anything is subsequently added to the queues, it will move to Busy.
            inner.manage.queue_waiter.prepare_wait();

            while let Ok(event) = inner.queue.event_queue.pop() {
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

            while let Ok(register) = inner.manage.register_inject.pop() {
                match register {
                    Register::Shutdown(expire, notify) => {
                        timer_id_source += 1;
                        timers.insert((expire, timer_id_source), Timer::Shutdown(notify));
                    }
                    Register::Waiter(start, waiter) => {
                        if let Some(expire) =
                            inner.manage.acquire_timeout.clone().map(|dur| start + dur)
                        {
                            timer_id_source += 1;
                            timers.insert((expire, timer_id_source), Timer::Waiter(waiter.clone()));
                        }
                        waiters.push_back(waiter);
                    }
                }
            }

            if waiters.is_empty() {
                inner
                    .queue
                    .busy
                    .compare_and_swap(true, false, Ordering::Release);
            } else {
                // This triggers released resources to go to the idle queue even when
                // there is no idle timeout, and prevents subsequent acquires from stealing
                // directly from the idle queue so that the waiters are completed first
                inner
                    .queue
                    .busy
                    .compare_and_swap(false, true, Ordering::Release);

                if !inner.queue.idle_queue.is_empty() || inner.create_from_count().is_some() {
                    let mut res = None;
                    while let Some(waiter) = waiters.pop_front() {
                        if waiter.is_canceled() {
                            continue;
                        }
                        // Bypasses busy check because we are satisfying waiters
                        if let Some(idle) = res.take().or_else(|| inner.queue.try_acquire_idle()) {
                            if let Err(mut failed) = waiter.send((idle, inner.queue.clone()).into())
                            {
                                res = failed.take_resource()
                            }
                        } else {
                            // Return waiter, no resources available
                            waiters.push_front(waiter);
                            break;
                        }
                    }
                    // No active waiters were found, return the resource to the queue
                    if let Some(res) = res {
                        inner.queue.release(res);
                    }
                }
            }

            let state = inner.manage.state.load(Ordering::Acquire);
            if state == SHUTDOWN {
                for res in repo.iter() {
                    if let Some(guard) = res.try_lock() {
                        inner.complete_resolve(inner.manage.lifecycle.dispose(guard, &inner));
                    }
                }
                repo.retain(|res| !res.is_none());
                if repo.is_empty() {
                    break;
                }
            }

            if let Some(next_check) = next_check {
                inner.manage.queue_waiter.wait_until(next_check);
            } else {
                inner.manage.queue_waiter.wait();
            }
            next_check = None;
        }

        // FIXME wait for executor to complete

        for (_, timer) in timers {
            match timer {
                Timer::Shutdown(waiter) => {
                    // Send true to indicate that the shutdown succeeded.
                    // Not bothered if the send fails.
                    waiter.send(true).unwrap_or(());
                }
                // Drop any other waiters, leading them to be cancelled
                _ => (),
            }
        }
    }

    pub fn complete_resolve(self: &Arc<Self>, res: ResourceResolve<T, E>) {
        if res.is_pending() {
            let inner = self.clone();
            self.manage.executor.spawn_ok(res.map(move |res| match res {
                Some(Ok(res)) => {
                    inner.queue.release(res);
                }
                Some(Err(err)) => {
                    inner.handle_error(err);
                }
                None => (),
            }))
        }
    }

    pub fn release_future(self: &Arc<Self>, fut: ResourceFuture<T, E>) {
        self.complete_resolve(ResourceResolve::from((fut.boxed(), self.clone())));
    }

    pub fn shutdown(self: &Arc<Self>) -> bool {
        let mut state = ACTIVE;
        loop {
            match self.manage.state.compare_exchange_weak(
                state,
                SHUTDOWN,
                Ordering::SeqCst,
                Ordering::Acquire,
            ) {
                Ok(_) | Err(SHUTDOWN) => {
                    self.queue.notify();
                    break true;
                }
                Err(STOPPED) => {
                    break false;
                }
                Err(s @ ACTIVE) => {
                    state = s;
                }
                Err(_) => panic!("Invalid pool state"),
            }
        }
    }

    pub fn try_acquire_idle(self: &Arc<Self>) -> ResourceResolve<T, E> {
        if self.queue.busy.load(Ordering::Acquire) {
            ResourceResolve::empty()
        } else {
            // Wrap the resource guard to ensure it is returned to the queue
            self.queue
                .try_acquire_idle()
                .map(|res| (res, self.queue.clone()))
                .into()
        }
    }

    pub fn try_create(self: &Arc<Self>) -> ResourceResolve<T, E> {
        let mut count = match self.create_from_count() {
            Some(c) => c,
            None => return ResourceResolve::empty(),
        };
        let max = self.queue.max_count;

        loop {
            match self.queue.count.compare_exchange_weak(
                count,
                count + 1,
                Ordering::SeqCst,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    // FIXME repo not required if there is no resource expiry?
                    let lock = ResourceLock::new(ResourceInfo::default(), None);
                    let guard = lock.try_lock().unwrap();
                    // Not concerned with failure here
                    self.queue
                        .event_queue
                        .push(QueueEvent::Created(guard.as_lock()))
                        .unwrap_or(());
                    self.queue.notify();
                    break self.manage.lifecycle.create(guard, self);
                }
                Err(c) => {
                    if c > count && max != 0 && c > max {
                        // Count was increased beyond max by another thread
                        break ResourceResolve::empty();
                    }
                    count = c;
                    continue;
                }
            }
        }
    }

    pub fn try_wait(self: &Arc<Self>, started: Instant) -> Waiter<ResourceResolve<T, E>> {
        let (send, receive) = waiter_pair();
        self.manage
            .register_inject
            .push(Register::Waiter(started, send))
            .unwrap_or(());
        self.queue.notify();
        receive
    }
}

pub struct Pool<T: Send + 'static, E: 'static> {
    pub(crate) inner: Sentinel<PoolInner<T, E>>,
}

impl<T: Send, E> Pool<T, E> {
    pub(crate) fn new(inner: PoolInner<T, E>) -> Self {
        let inner = Arc::new(inner);
        let mgr = inner.clone();
        let sentinel = Sentinel::new(inner, |inner, count| {
            if count == 0 {
                inner.shutdown();
            }
        });
        std::thread::spawn(move || mgr.manage());
        Self { inner: sentinel }
    }

    pub fn acquire(&self) -> Acquire<T, E> {
        Acquire::new(self.clone())
    }

    pub(crate) fn queue(&self) -> &Arc<Queue<T>> {
        &self.inner.queue
    }

    pub fn shutdown(self, timeout: Duration) -> PoolShutdown {
        let (send, receive) = waiter_pair();
        self.inner
            .manage
            .register_inject
            .push(Register::Shutdown(Instant::now() + timeout, send))
            .unwrap_or(());
        PoolShutdown { receive }
    }
}

impl<T: Send, E> Clone for Pool<T, E> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

enum Register<T: Send + 'static, E: 'static> {
    Shutdown(Instant, WaitResponder<bool>),
    Waiter(Instant, WaitResource<T, E>),
}

enum Timer<T: Send + 'static, E: 'static> {
    Shutdown(WaitResponder<bool>),
    Verify(ResourceLock<T>),
    Waiter(WaitResource<T, E>),
}

pub struct PoolShutdown {
    receive: Waiter<bool>,
}

impl<T: Send + 'static, E: 'static> Timer<T, E> {
    // fn is_canceled(&self) -> bool {
    //     match self {
    //         Self::Shutdown(ref wait) => wait.is_canceled(),
    //         Self::Verify(..) => false,
    //         Self::Waiter(ref wait) => wait.is_canceled(),
    //     }
    // }
}

impl Future for PoolShutdown {
    type Output = bool;
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        Pin::new(&mut *self.receive)
            .poll(cx)
            .map(|result| result.unwrap_or(false))
    }
}

// FIXME test behaviour of cloned WaitResponder + close
