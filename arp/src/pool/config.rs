use std::time::Duration;

use futures_util::future::TryFuture;

use super::{
    resource_create, resource_verify, DisposeFn, ErrorFn, ReleaseFn, ResourceInfo,
    ResourceOperation,
};
use super::{Pool, PoolInternal};

pub struct PoolConfig<T: Send, E> {
    acquire_timeout: Option<Duration>,
    handle_error: Option<ErrorFn<E>>,
    idle_timeout: Option<Duration>,
    min_count: usize,
    max_count: usize,
    max_waiters: Option<usize>,
    on_create: Box<dyn ResourceOperation<T, E> + Send + Sync>,
    on_dispose: Option<DisposeFn<T>>,
    on_release: Option<ReleaseFn<T>>,
    on_verify: Option<Box<dyn ResourceOperation<T, E> + Send + Sync>>,
    thread_count: Option<usize>,
}

impl<T: Send, E> PoolConfig<T, E> {
    pub fn new<C, F>(create: C) -> Self
    where
        C: Fn() -> F + Send + Sync + 'static,
        F: TryFuture<Ok = T, Error = E> + Send + 'static,
        T: Send + 'static,
        E: 'static,
    {
        Self {
            acquire_timeout: None,
            handle_error: None,
            idle_timeout: None,
            min_count: 0,
            max_count: 0,
            max_waiters: None,
            on_create: Box::new(resource_create(create)),
            on_dispose: None,
            on_release: None,
            on_verify: None,
            thread_count: None,
        }
    }

    pub fn acquire_timeout(mut self, val: Duration) -> Self {
        if val.as_micros() > 0 {
            self.acquire_timeout.replace(val);
        } else {
            self.acquire_timeout.take();
        }
        self
    }

    pub fn dispose<F>(mut self, dispose: F) -> Self
    where
        F: Fn(T, ResourceInfo) -> () + Send + Sync + 'static,
    {
        self.on_dispose.replace(Box::new(dispose));
        self
    }

    pub fn handle_error<F>(mut self, handler: F) -> Self
    where
        F: Fn(E) + Send + Sync + 'static,
    {
        self.handle_error.replace(Box::new(handler));
        self
    }

    pub fn idle_timeout(mut self, val: Duration) -> Self {
        if val.as_micros() > 0 {
            self.idle_timeout.replace(val);
        } else {
            self.idle_timeout.take();
        }
        self
    }

    pub fn verify<V, F>(mut self, verify: V) -> Self
    where
        V: Fn(&mut T, ResourceInfo) -> F + Send + Sync + 'static,
        F: TryFuture<Ok = Option<T>, Error = E> + Send + 'static,
        T: Send + 'static,
        E: 'static,
    {
        self.on_verify.replace(Box::new(resource_verify(verify)));
        self
    }

    pub fn max_count(mut self, val: usize) -> Self {
        self.max_count = val;
        self
    }

    pub fn max_waiters(mut self, val: usize) -> Self {
        self.max_waiters.replace(val);
        self
    }

    pub fn min_count(mut self, val: usize) -> Self {
        self.min_count = val;
        self
    }

    pub fn release<F>(mut self, release: F) -> Self
    where
        F: Fn(&mut T, ResourceInfo) -> bool + Send + Sync + 'static,
    {
        self.on_release.replace(Box::new(release));
        self
    }

    pub fn thread_count(mut self, val: usize) -> Self {
        if val > 0 {
            self.thread_count.replace(val);
        } else {
            self.thread_count.take();
        }
        self
    }

    pub fn build(self) -> Pool<T, E> {
        let inner = PoolInternal::new(
            self.acquire_timeout,
            self.on_create,
            self.handle_error,
            self.idle_timeout,
            self.min_count,
            self.max_count,
            self.max_waiters,
            self.on_dispose,
            self.on_release,
            self.thread_count,
            self.on_verify,
        );
        Pool::new(inner)
        // let exec = Executor::new(self.thread_count.unwrap_or(1));
        // Pool::new(queue, mgr, exec)
    }
}
