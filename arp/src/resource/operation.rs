use futures_util::future::{BoxFuture, FutureExt, TryFuture, TryFutureExt};

use super::lock::ResourceGuard;
use super::ResourceInfo;

pub type ResourceFuture<T, E> = BoxFuture<'static, Result<ResourceGuard<T>, E>>;

pub trait ResourceOperation<T, E> {
    fn apply<'a>(&self, guard: ResourceGuard<T>) -> ResourceFuture<T, E>;
}

pub struct ResourceUpdateFn<F> {
    inner: F,
}

impl<T, E, F> ResourceOperation<T, E> for ResourceUpdateFn<F>
where
    F: Fn(ResourceGuard<T>) -> ResourceFuture<T, E>,
{
    fn apply<'a>(&self, guard: ResourceGuard<T>) -> ResourceFuture<T, E> {
        (self.inner)(guard)
    }
}

pub fn resource_create<C, F, T, E>(ctor: C) -> impl ResourceOperation<T, E>
where
    C: Fn() -> F + Send + Sync,
    F: TryFuture<Ok = T, Error = E> + Send + 'static,
    T: Send + 'static,
{
    ResourceUpdateFn {
        inner: move |mut guard: ResourceGuard<T>| {
            assert!(guard.is_none());
            let result = ctor();
            result
                .and_then(|res| async move {
                    guard.replace(res);
                    Ok(guard)
                })
                .boxed()
        },
    }
}

pub fn resource_update<C, F, T, E>(update: C) -> impl ResourceOperation<T, E>
where
    C: Fn(T, ResourceInfo) -> F + Send + Sync,
    F: TryFuture<Ok = Option<T>, Error = E> + Send + 'static,
    T: Send + 'static,
{
    ResourceUpdateFn {
        inner: move |mut guard: ResourceGuard<T>| {
            let res = guard.take().unwrap();
            let result = update(res, *guard.info());
            result
                .and_then(|optres| async move {
                    optres.and_then(|res| guard.replace(res));
                    Ok(guard)
                })
                .boxed()
        },
    }
}

pub fn resource_dispose<C, F, T, E>(dispose: C) -> impl ResourceOperation<T, E>
where
    C: Fn(T, ResourceInfo) -> F + Send + Sync,
    F: TryFuture<Ok = (), Error = E> + Send + 'static,
    T: Send + 'static,
{
    ResourceUpdateFn {
        inner: move |mut guard: ResourceGuard<T>| {
            let res = guard.take().unwrap();
            let result = dispose(res, *guard.info());
            result.and_then(|_| async move { Ok(guard) }).boxed()
        },
    }
}
