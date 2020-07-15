use std::time::Duration;

use futures_util::future::TryFuture;

use super::pool::{Inner as PoolInner, Pool};
use super::resource::{
    resource_create, resource_dispose, resource_update, Lifecycle, ResourceInfo,
};

pub struct PoolConfig<T, E> {
    acquire_timeout: Option<Duration>,
    idle_timeout: Option<Duration>,
    min_count: usize,
    max_count: Option<usize>,
    max_waiters: Option<usize>,
    thread_count: Option<usize>,
    lifecycle: Lifecycle<T, E>,
}

impl<T, E> PoolConfig<T, E> {
    pub fn new<C, F>(create: C) -> Self
    where
        C: Fn() -> F + Send + Sync + 'static,
        F: TryFuture<Ok = T, Error = E> + Send + 'static,
        T: Send + 'static,
        E: 'static,
    {
        let lifecycle = Lifecycle::new(Box::new(resource_create(create)));
        Self {
            acquire_timeout: None,
            idle_timeout: None,
            min_count: 0,
            max_count: None,
            max_waiters: None,
            thread_count: None,
            lifecycle,
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

    pub fn dispose<D, F>(mut self, dispose: D) -> Self
    where
        D: Fn(T, ResourceInfo) -> F + Send + Sync + 'static,
        F: TryFuture<Ok = (), Error = E> + Send + 'static,
        T: Send + 'static,
        E: 'static,
    {
        self.lifecycle
            .dispose
            .replace(Box::new(resource_dispose(dispose)));
        self
    }

    pub fn handle_error<F>(mut self, handler: F) -> Self
    where
        F: Fn(E) + Send + Sync + 'static,
    {
        self.lifecycle.handle_error.replace(Box::new(handler));
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

    pub fn keepalive<K, F>(mut self, keepalive: K) -> Self
    where
        K: Fn(T, ResourceInfo) -> F + Send + Sync + 'static,
        F: TryFuture<Ok = Option<T>, Error = E> + Send + 'static,
        T: Send + 'static,
        E: 'static,
    {
        self.lifecycle
            .keepalive
            .replace(Box::new(resource_update(keepalive)));
        self
    }

    pub fn max_count(mut self, val: usize) -> Self {
        if val > 0 {
            self.max_count.replace(val);
        } else {
            self.max_count.take();
        }
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

    pub fn thread_count(mut self, val: usize) -> Self {
        if val > 0 {
            self.thread_count.replace(val);
        } else {
            self.thread_count.take();
        }
        self
    }

    pub fn build(self) -> Pool<T, E> {
        let inner = PoolInner::new(
            self.lifecycle,
            self.acquire_timeout,
            self.idle_timeout,
            self.min_count,
            self.max_count,
            self.max_waiters,
        );
        Pool::new(inner)
        // let exec = Executor::new(self.thread_count.unwrap_or(1));
        // Pool::new(queue, mgr, exec)
    }
}
