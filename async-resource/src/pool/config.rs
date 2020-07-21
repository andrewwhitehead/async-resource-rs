use std::time::Duration;

use futures_util::future::TryFuture;

use super::error::ConfigError;
use super::executor::{default_executor, Executor};
use super::operation::{resource_create, resource_verify, ResourceOperation};
use super::pool::{ErrorFn, Pool, PoolInternal};
use crate::resource::ResourceInfo;
use crate::shared::{DisposeFn, ReleaseFn};

/// A pool configuration instance, used to build a new instance of a resource
/// pool.
pub struct PoolConfig<T: Send, E> {
    acquire_timeout: Option<Duration>,
    executor: Option<Box<dyn Executor>>,
    handle_error: Option<ErrorFn<E>>,
    idle_timeout: Option<Duration>,
    min_count: usize,
    max_count: usize,
    max_waiters: Option<usize>,
    on_create: Box<dyn ResourceOperation<T, E> + Send + Sync>,
    on_dispose: Option<DisposeFn<T>>,
    on_release: Option<ReleaseFn<T>>,
    on_verify: Option<Box<dyn ResourceOperation<T, E> + Send + Sync>>,
}

impl<T: Send, E> PoolConfig<T, E> {
    /// Create a new pool configuration from a resource constructor function.
    /// The function must return a Future resolving to a `Result<T, E>`.
    pub fn new<C, F>(create: C) -> Self
    where
        C: Fn() -> F + Send + Sync + 'static,
        F: TryFuture<Ok = T, Error = E> + Send + 'static,
        T: Send + 'static,
        E: 'static,
    {
        Self {
            acquire_timeout: None,
            executor: None,
            handle_error: None,
            idle_timeout: None,
            min_count: 0,
            max_count: 0,
            max_waiters: None,
            on_create: Box::new(resource_create(create)),
            on_dispose: None,
            on_release: None,
            on_verify: None,
        }
    }

    /// Set the pool default timeout for resource acquisition. A zero-length
    /// `Duration` indicates no default timeout.
    pub fn acquire_timeout(mut self, val: Duration) -> Self {
        if val.as_micros() > 0 {
            self.acquire_timeout.replace(val);
        } else {
            self.acquire_timeout.take();
        }
        self
    }

    /// Set a callback method to execute when a resource is disposed
    /// because it is no longer needed or it has failed validation.
    pub fn dispose<F>(mut self, dispose: F) -> Self
    where
        F: Fn(T, ResourceInfo) -> () + Send + Sync + 'static,
    {
        self.on_dispose.replace(Box::new(dispose));
        self
    }

    /// Set a callback method to execute when an error is raised during
    /// resource creation or validation.
    pub fn handle_error<F>(mut self, handler: F) -> Self
    where
        F: Fn(E) + Send + Sync + 'static,
    {
        self.handle_error.replace(Box::new(handler));
        self
    }

    /// Set the idle timeout for resources. A zero-length `Duration` implies
    /// that resources will be disposed immediately when they are released,
    /// unless there are active pending acquisition requests.
    pub fn idle_timeout(mut self, val: Duration) -> Self {
        if val.as_micros() > 0 {
            self.idle_timeout.replace(val);
        } else {
            self.idle_timeout.take();
        }
        self
    }

    /// Set the callback used to verify a resource once its idle timeout has
    /// expired.
    pub fn verify<V, F>(mut self, verify: V) -> Self
    where
        V: Fn(&mut T, ResourceInfo) -> F + Send + Sync + 'static,
        F: TryFuture<Ok = bool, Error = E> + Send + 'static,
        T: Send + 'static,
        E: 'static,
    {
        self.on_verify.replace(Box::new(resource_verify(verify)));
        self
    }

    /// Set the maximum number of resource instances to create within the
    /// resulting `Pool` instance. Set to zero in order to enforce no maximum
    /// count.
    pub fn max_count(mut self, val: usize) -> Self {
        self.max_count = val;
        self
    }

    /// Set the maximum number of waiters allowed by the `Pool` instance when
    /// no idle resources are available and the maximum number of resources
    /// have been created. When the maximum number has been reached, subsequent
    /// attempts to acquire a resource may resolve to `AcquireError::Busy`.
    pub fn max_waiters(mut self, val: usize) -> Self {
        self.max_waiters.replace(val);
        self
    }

    /// Set the minimum number of resources to keep instantiated. This helps to
    /// reduce the latency before a request can be satisfied.
    pub fn min_count(mut self, val: usize) -> Self {
        self.min_count = val;
        self
    }

    /// Set a callback to be executed when a resource instance is released
    /// back to the `Pool` after being acquired. This callback should return
    /// `true` if the instance is capable of being reused, and `false` if it
    /// should be disposed.
    pub fn release<F>(mut self, release: F) -> Self
    where
        F: Fn(&mut T, ResourceInfo) -> bool + Send + Sync + 'static,
    {
        self.on_release.replace(Box::new(release));
        self
    }

    /// Convert the pool configuration into a new `Pool` instance.
    pub fn build(self) -> Result<Pool<T, E>, ConfigError> {
        let inner = PoolInternal::new(
            self.acquire_timeout,
            self.on_create,
            self.executor.map(Ok).unwrap_or_else(default_executor)?,
            self.handle_error,
            self.idle_timeout,
            self.min_count,
            self.max_count,
            self.max_waiters,
            self.on_dispose,
            self.on_release,
            self.on_verify,
        );
        Ok(Pool::new(inner))
    }
}
