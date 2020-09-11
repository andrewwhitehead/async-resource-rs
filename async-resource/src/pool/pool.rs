use std::collections::{BTreeMap, HashSet, VecDeque};
use std::fmt::{self, Debug, Formatter};
use std::future::Future;
use std::pin::Pin;
use std::sync::{
    atomic::{AtomicU8, Ordering},
    Arc,
};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use concurrent_queue::ConcurrentQueue;
use futures_lite::future::FutureExt;
use suspend::{Notifier, Suspend};

use crate::resource::{ResourceGuard, ResourceInfo, ResourceLock};
use crate::shared::{DisposeFn, ReleaseFn, Shared, SharedEvent};
use crate::util::sentinel::Sentinel;

use super::acquire;
use super::executor::Executor;
use super::operation::{ResourceFuture, ResourceOperation, ResourceResolve};
use super::wait::{waiter_pair, WaitResponder, Waiter};

const ACTIVE: u8 = 0;
const DRAIN: u8 = 1;
const SHUTDOWN: u8 = 2;
const STOPPED: u8 = 3;

type BoxedOperation<T, E> = Box<dyn ResourceOperation<T, E> + Send + Sync>;

pub type ErrorFn<E> = Box<dyn Fn(E) + Send + Sync>;

type WaitResource<T, E> = WaitResponder<ResourceResolve<T, E>>;

pub(crate) struct PoolManageState<T: Send + 'static, E: 'static> {
    acquire_timeout: Option<Duration>,
    create: BoxedOperation<T, E>,
    executor: Box<dyn Executor>,
    handle_error: Option<ErrorFn<E>>,
    register_inject: ConcurrentQueue<Register<T, E>>,
    state: AtomicU8,
    verify: Option<BoxedOperation<T, E>>,
}

pub struct PoolInternal<T: Send + 'static, E: 'static> {
    manage: PoolManageState<T, E>,
    shared: Arc<Shared<T>>,
}

impl<T: Send, E> PoolInternal<T, E> {
    pub fn new(
        acquire_timeout: Option<Duration>,
        create: Box<dyn ResourceOperation<T, E> + Send + Sync>,
        executor: Box<dyn Executor>,
        handle_error: Option<Box<dyn Fn(E) + Send + Sync>>,
        idle_timeout: Option<Duration>,
        min_count: usize,
        max_count: usize,
        max_waiters: Option<usize>,
        notifier: Notifier,
        on_dispose: Option<DisposeFn<T>>,
        on_release: Option<ReleaseFn<T>>,
        verify: Option<Box<dyn ResourceOperation<T, E> + Send + Sync>>,
    ) -> Self {
        let shared = Shared::new(
            notifier,
            on_release,
            on_dispose,
            min_count,
            max_count,
            max_waiters,
            idle_timeout,
        );
        let manage = PoolManageState {
            acquire_timeout,
            create,
            executor,
            handle_error,
            register_inject: ConcurrentQueue::unbounded(),
            state: AtomicU8::new(ACTIVE),
            verify,
        };
        Self {
            manage,
            shared: Arc::new(shared),
        }
    }

    pub fn create(self: &Arc<Self>) -> ResourceResolve<T, E> {
        let mut info = ResourceInfo::default();
        info.reusable = self.shared.can_reuse();
        info.last_acquire.replace(Instant::now());
        info.acquire_count = 1;

        let lock = ResourceLock::new(info, None);
        let guard = lock.try_lock().unwrap();

        // Send the resource lock into the repo, for collection later
        // Not concerned with failure here
        self.shared
            .push_event(SharedEvent::Created(guard.as_lock()));

        self.manage.create.apply(guard, self)
    }

    pub fn create_from_count(&self) -> Option<usize> {
        let count = self.shared.count();
        let max = self.shared.max_count();
        if max == 0 || max > count {
            Some(count)
        } else {
            None
        }
    }

    pub fn handle_error(&self, err: E) {
        if let Some(handler) = self.manage.handle_error.as_ref() {
            (handler)(err)
        }
    }

    fn manage(self: Arc<Self>, mut suspend: Suspend) {
        let mut drain_count: usize = 0;
        let inner = Sentinel::new(self, |inner, _| {
            // Set the state to Stopped when this thread exits, whether normally
            // or due to a panic.
            inner.manage.state.store(STOPPED, Ordering::Release);
        });
        let mut last_dispose_count = 0;
        let mut next_check = None;
        let mut repo = HashSet::<ResourceLock<T>>::new();
        let mut timer_id_source: usize = 0;
        let mut timers = BTreeMap::<(Instant, usize), Timer<T, E>>::new();
        let mut waiters = VecDeque::<WaitResource<T, E>>::new();
        let mut upd_waiter_count = None;

        loop {
            let remain_timers = timers.split_off(&(Instant::now(), 0));
            for ((inst, _), timer) in timers {
                match timer {
                    Timer::Drain(waiter) => {
                        // Cancel drain
                        drain_count -= 1;
                        if drain_count == 0 {
                            inner.stop_drain();
                        }
                        waiter.cancel();
                    }
                    Timer::Verify(res) => {
                        if let Some(mut guard) = res.try_lock() {
                            // Any clone of the resource lock in the idle queue can't be
                            // acquired at the moment, so forget that lock and create a
                            // new one
                            repo.remove(&guard.as_lock());
                            guard = guard.detach();

                            if guard.is_some() && guard.info().verify_at == Some(inst) {
                                // Act upon the verify timer
                                inner.verify_or_dispose(guard);
                            } else {
                                // The resource was reacquired and released after the
                                // verify timer was set, put it back into the idle queue.
                                // FIXME attach timer to the resource lock so it can be
                                // checked without acquiring it
                                repo.insert(guard.as_lock());
                                // FIXME this can trigger another verify!
                                inner.shared.release(guard);
                            }
                        } else {
                            // If the resource is locked, then a consumer thread
                            // has acquired it or is in the process of doing so,
                            // nothing to do here
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

            // Set the waiter state to Wait.
            // If anything is subsequently added to the queues, it will move to Idle.
            let listener = suspend.listen();

            while let Some(event) = inner.shared.pop_event() {
                match event {
                    SharedEvent::Created(res) => {
                        repo.insert(res);
                    }
                    SharedEvent::Verify(expire, res) => {
                        timer_id_source += 1;
                        timers.insert((expire, timer_id_source), Timer::Verify(res));
                    }
                }
            }

            while let Ok(register) = inner.manage.register_inject.pop() {
                match register {
                    Register::Drain(expire, notify) => {
                        inner.start_drain(false);
                        timer_id_source += 1;
                        timers.insert((expire, timer_id_source), Timer::Drain(notify));
                        drain_count += 1;
                    }
                    Register::Waiter(start, waiter) => {
                        if waiter.is_canceled() {
                        } else {
                            if let Some(expire) =
                                inner.manage.acquire_timeout.clone().map(|dur| start + dur)
                            {
                                timer_id_source += 1;
                                timers.insert(
                                    (expire, timer_id_source),
                                    Timer::Waiter(waiter.clone()),
                                );
                            }
                            waiters.push_back(waiter);
                        }
                    }
                }
            }

            upd_waiter_count.take();
            if !waiters.is_empty() {
                if inner.shared.have_idle() || inner.create_from_count().is_some() {
                    let mut res = None;
                    while let Some(waiter) = waiters.pop_front() {
                        if waiter.is_canceled() {
                            upd_waiter_count.replace(inner.shared.decrement_waiters());
                            continue;
                        }
                        // Bypasses busy check because we are satisfying waiters
                        if let Some(idle) = res.take().or_else(|| inner.shared.try_acquire_idle()) {
                            if let Err(mut failed) =
                                waiter.send((idle, inner.shared.clone()).into())
                            {
                                res = failed.take_resource()
                            }
                            upd_waiter_count.replace(inner.shared.decrement_waiters());
                        } else {
                            // Return waiter, no resources available
                            waiters.push_front(waiter);
                            break;
                        }
                    }
                    // No active waiters were found, return the resource to the queue
                    if let Some(res) = res {
                        inner.shared.release(res);
                    }
                }
            }

            // FIXME when waiter_count returns to zero, dispose idle resources
            // over min_count because 'busy' allows them to return to the idle
            // queue

            let state = inner.manage.state.load(Ordering::Acquire);
            if state == DRAIN {
                for res in repo.iter() {
                    if let Some(guard) = res.try_lock() {
                        inner.shared.dispose(guard);
                    }
                }
                repo.retain(|res| !res.is_none());
                if repo.is_empty() {
                    break;
                }
            } else {
                let dispose_count = inner.shared.dispose_count();
                if last_dispose_count != dispose_count {
                    repo.retain(|res| !res.is_none());
                    last_dispose_count = dispose_count;
                }
            }

            if let Some(next_check) = next_check {
                listener.wait_deadline(next_check).unwrap_or(());
            } else {
                listener.wait();
            }
            next_check = None;
        }

        // FIXME wait for executor to complete

        for (_, timer) in timers {
            match timer {
                Timer::Drain(waiter) => {
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
            self.manage.executor.spawn_ok(
                async move {
                    match res.await {
                        Some(Ok(res)) => {
                            inner.shared.release(res);
                        }
                        Some(Err(err)) => {
                            inner.handle_error(err);
                        }
                        None => (),
                    }
                }
                .boxed(),
            )
        }
    }

    pub fn release_future(self: &Arc<Self>, fut: ResourceFuture<T, E>) {
        self.complete_resolve(ResourceResolve::from((fut.boxed(), self.clone())));
    }

    fn register(&self, reg: Register<T, E>) {
        self.manage
            .register_inject
            .push(reg)
            .unwrap_or_else(|_| panic!("Pool manager injector error"));
        self.shared.notify();
    }

    pub fn shared(&self) -> &Arc<Shared<T>> {
        &self.shared
    }

    pub fn start_drain(self: &Arc<Self>, shutdown: bool) {
        let mut state = ACTIVE;
        let next_state = if shutdown { SHUTDOWN } else { DRAIN };
        loop {
            match self.manage.state.compare_exchange_weak(
                state,
                next_state,
                Ordering::Release,
                Ordering::Relaxed,
            ) {
                Ok(_) => {
                    self.shared.notify();
                }
                Err(DRAIN) if next_state == DRAIN => {
                    break;
                }
                Err(s @ ACTIVE) | Err(s @ DRAIN) => {
                    state = s;
                }
                Err(SHUTDOWN) | Err(STOPPED) => {
                    break;
                }
                Err(_) => panic!("Invalid pool state"),
            }
        }
    }

    pub fn stop_drain(self: &Arc<Self>) {
        self.manage
            .state
            .compare_and_swap(DRAIN, ACTIVE, Ordering::AcqRel);
    }

    pub fn try_acquire_idle(self: &Arc<Self>) -> ResourceResolve<T, E> {
        if !self.shared.is_busy() {
            while let Some(mut guard) = self.shared.try_acquire_idle() {
                if guard
                    .info()
                    .verify_at
                    .map(|v| v < Instant::now())
                    .unwrap_or(false)
                {
                    if let Some(verify) = self.manage.verify.as_ref() {
                        guard.info_mut().last_acquire.replace(Instant::now());
                        guard.info_mut().acquire_count += 1;
                        return verify.apply(guard, self);
                    } else {
                        guard.info_mut().expired = true;
                        self.shared.release(guard);
                    }
                } else {
                    // Wrap the resource guard to ensure it is returned to the queue
                    guard.info_mut().last_acquire.replace(Instant::now());
                    guard.info_mut().acquire_count += 1;
                    return (guard, self.shared.clone()).into();
                }
            }
        }
        ResourceResolve::empty()
    }

    pub fn try_create(self: &Arc<Self>) -> ResourceResolve<T, E> {
        let mut count = match self.create_from_count() {
            Some(c) => c,
            None => return ResourceResolve::empty(),
        };
        let max = self.shared.max_count();

        loop {
            match self.shared.try_update_count(count, count + 1) {
                Ok(_) => {
                    break self.create();
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

    pub fn try_wait(self: &Arc<Self>, started: Instant) -> Option<Waiter<ResourceResolve<T, E>>> {
        if self.shared.try_increment_waiters().is_some() {
            let (send, receive) = waiter_pair();
            self.register(Register::Waiter(started, send));
            self.shared.notify();
            Some(receive)
        } else {
            None
        }
    }

    pub fn verify_or_dispose(self: &Arc<Self>, guard: ResourceGuard<T>) {
        if guard.info().reusable {
            if let Some(verify) = self.manage.verify.as_ref() {
                if self.shared.count() <= self.shared.min_count() {
                    return self.complete_resolve(verify.apply(guard, &self));
                }
            }
        }
        self.shared.dispose(guard);
    }
}

/// A resource pool instance, which handles acquisition of managed resources
/// of type `T`.
pub struct Pool<T: Send + 'static, E: 'static> {
    pub(crate) inner: Sentinel<PoolInternal<T, E>>,
}

impl<T: Send, E> Pool<T, E> {
    pub(crate) fn new(inner: PoolInternal<T, E>, suspend: Suspend) -> Self {
        let inner = Arc::new(inner);
        let mgr = inner.clone();
        let sentinel = Sentinel::new(inner, |inner, count| {
            if count == 0 {
                inner.start_drain(true);
            }
        });
        std::thread::spawn(move || mgr.manage(suspend));
        Self { inner: sentinel }
    }

    /// Returns an `Acquire<T, E>` which is a `Future` that resolves to either
    /// a `Managed<T>` wrapper around an acquired resource, or an
    /// `AcquireError<E>`.
    pub fn acquire(&self) -> acquire::Acquire<T, E> {
        acquire::acquire(self.clone())
    }

    /// Fetch the current number of allocated resources for this `Pool`
    /// instance.
    pub fn count(&self) -> usize {
        self.inner.shared.count()
    }

    /// Attempt to drain and shut down this `Pool` instance, disposing of any
    /// allocated resource instances.
    pub fn drain(self, timeout: Duration) -> PoolDrain<T, E> {
        let (send, receive) = waiter_pair();
        self.inner
            .register(Register::Drain(Instant::now() + timeout, send));
        PoolDrain {
            inner: Some(self.inner),
            receive,
        }
    }
}

impl<T: Send, E> Clone for Pool<T, E> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

impl<T: Send, E> Debug for Pool<T, E> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("Pool")
            .field("count", &self.count())
            .finish()
    }
}

#[derive(Debug)]
enum Register<T: Send + 'static, E: 'static> {
    Drain(Instant, WaitResponder<bool>),
    Waiter(Instant, WaitResource<T, E>),
}

#[derive(Debug)]
enum Timer<T: Send + 'static, E: 'static> {
    Drain(WaitResponder<bool>),
    Verify(ResourceLock<T>),
    Waiter(WaitResource<T, E>),
}

impl<T: Send + 'static, E: 'static> Timer<T, E> {
    // fn is_canceled(&self) -> bool {
    //     match self {
    //         Self::Drain(ref wait) => wait.is_canceled(),
    //         Self::Verify(..) => false,
    //         Self::Waiter(ref wait) => wait.is_canceled(),
    //     }
    // }
}

/// A Future which resolves to `Ok(())` on a successful drain and shutdown of
/// the resource pool, or an `Err` containing the `Pool` instance if the
/// drain operation timed out (due to unreleased resources or clones of the
/// `Pool` instance).
pub struct PoolDrain<T: Send + 'static, E: 'static> {
    inner: Option<Sentinel<PoolInternal<T, E>>>,
    receive: Waiter<bool>,
}

impl<T: Send + 'static, E: 'static> Debug for PoolDrain<T, E> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("PoolDrain").finish()
    }
}

impl<T: Send, E> Future for PoolDrain<T, E> {
    type Output = Result<(), Pool<T, E>>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if self.inner.is_none() {
            return Poll::Ready(Ok(()));
        }

        match Pin::new(&mut *self.receive).poll(cx) {
            Poll::Ready(done) => {
                if done.unwrap_or(false) {
                    Poll::Ready(Ok(()))
                } else {
                    Poll::Ready(Err(Pool {
                        inner: self.inner.take().unwrap(),
                    }))
                }
            }
            Poll::Pending => Poll::Pending,
        }
    }
}
