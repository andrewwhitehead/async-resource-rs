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

use super::resource::{ResourceGuard, ResourceInfo, ResourceLock};
use super::shared::{DisposeFn, ReleaseFn, Shared, SharedEvent, SharedWaiter};
use super::wait::{waiter_pair, WaitResponder, Waiter};

mod acquire;
pub use acquire::Acquire;

mod config;
pub use config::PoolConfig;

mod error;
pub use error::AcquireError;

mod executor;
use executor::Executor;

mod operation;
use operation::{
    resource_create, resource_verify, ResourceFuture, ResourceOperation, ResourceResolve,
};

mod sentinel;
pub use sentinel::Sentinel;

const ACTIVE: u8 = 0;
const SHUTDOWN: u8 = 1;
const STOPPED: u8 = 2;

type BoxedOperation<T, E> = Box<dyn ResourceOperation<T, E> + Send + Sync>;

type ErrorFn<E> = Box<dyn Fn(E) + Send + Sync>;

type WaitResource<T, E> = WaitResponder<ResourceResolve<T, E>>;

pub(crate) struct PoolManageState<T: Send + 'static, E: 'static> {
    acquire_timeout: Option<Duration>,
    create: BoxedOperation<T, E>,
    executor: Executor,
    handle_error: Option<ErrorFn<E>>,
    max_waiters: Option<usize>,
    shared_waiter: SharedWaiter,
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
        handle_error: Option<Box<dyn Fn(E) + Send + Sync>>,
        idle_timeout: Option<Duration>,
        min_count: usize,
        max_count: usize,
        max_waiters: Option<usize>,
        on_dispose: Option<DisposeFn<T>>,
        on_release: Option<ReleaseFn<T>>,
        thread_count: Option<usize>,
        verify: Option<Box<dyn ResourceOperation<T, E> + Send + Sync>>,
    ) -> Self {
        let executor = Executor::new(thread_count.unwrap_or(1));
        let (shared, shared_waiter) =
            Shared::new(on_release, on_dispose, min_count, max_count, idle_timeout);
        let manage = PoolManageState {
            acquire_timeout,
            create,
            executor,
            handle_error,
            max_waiters,
            register_inject: ConcurrentQueue::unbounded(),
            shared_waiter,
            state: AtomicU8::new(ACTIVE),
            verify,
        };
        Self {
            manage,
            shared: Arc::new(shared),
        }
    }

    pub fn create(self: &Arc<Self>) -> ResourceResolve<T, E> {
        // FIXME repo not required if there is no resource expiry?
        let lock = ResourceLock::new(ResourceInfo::default(), None);
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

    fn manage(self: Arc<Self>) {
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

        loop {
            let remain_timers = timers.split_off(&(Instant::now(), 0));
            for ((inst, _), timer) in timers {
                match timer {
                    Timer::Shutdown(_) => {
                        // Just drop the shutdown timer to indicate we haven't finished
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
                                inner.verify(guard);
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

            // Set the waiter state to Idle.
            // If anything is subsequently added to the queues, it will move to Busy.
            inner.manage.shared_waiter.prepare_wait();

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
                // FIXME dispose idle resources over min_count
                // because 'busy' allows them to return to the idle queue
                inner.shared.set_busy(false);
            } else {
                // This triggers released resources to go to the idle queue even when
                // there is no idle timeout, and prevents subsequent acquires from stealing
                // directly from the idle queue so that the waiters are completed first
                inner.shared.set_busy(true);

                if inner.shared.have_idle() || inner.create_from_count().is_some() {
                    let mut res = None;
                    while let Some(waiter) = waiters.pop_front() {
                        if waiter.is_canceled() {
                            continue;
                        }
                        // Bypasses busy check because we are satisfying waiters
                        if let Some(idle) = res.take().or_else(|| inner.shared.try_acquire_idle()) {
                            if let Err(mut failed) =
                                waiter.send((idle, inner.shared.clone()).into())
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
                        inner.shared.release(res);
                    }
                }
            }

            let state = inner.manage.state.load(Ordering::Acquire);
            if state == SHUTDOWN {
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
                inner.manage.shared_waiter.wait_until(next_check);
            } else {
                inner.manage.shared_waiter.wait();
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
                    inner.shared.release(res);
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

    pub fn shared(&self) -> &Arc<Shared<T>> {
        &self.shared
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
                    self.shared.notify();
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
        if self.shared.is_busy() {
            ResourceResolve::empty()
        } else {
            // Wrap the resource guard to ensure it is returned to the queue
            self.shared
                .try_acquire_idle()
                .map(|res| (res, self.shared.clone()))
                .into()
        }
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

    pub fn try_wait(self: &Arc<Self>, started: Instant) -> Waiter<ResourceResolve<T, E>> {
        let (send, receive) = waiter_pair();
        self.manage
            .register_inject
            .push(Register::Waiter(started, send))
            .unwrap_or(());
        self.shared.notify();
        receive
    }

    pub fn verify(self: &Arc<Self>, guard: ResourceGuard<T>) {
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

pub struct Pool<T: Send + 'static, E: 'static> {
    pub(crate) inner: Sentinel<PoolInternal<T, E>>,
}

impl<T: Send, E> Pool<T, E> {
    pub(crate) fn new(inner: PoolInternal<T, E>) -> Self {
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

    pub fn count(&self) -> usize {
        self.inner.shared.count()
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
