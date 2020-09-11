use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Instant;

use futures_lite::future::{Boxed as BoxFuture, FutureExt};

use super::pool::PoolInternal;
use crate::resource::{ResourceGuard, ResourceInfo};
use crate::shared::Shared;

pub type ResourceFuture<T, E> = BoxFuture<Result<ResourceGuard<T>, E>>;

pub enum ResourceResolveType<T: Send + 'static, E: 'static> {
    Resource(Option<(ResourceGuard<T>, Arc<Shared<T>>)>),
    Future(ResourceFuture<T, E>, Arc<PoolInternal<T, E>>),
}

pub struct ResourceResolve<T: Send + 'static, E: 'static>(ResourceResolveType<T, E>);

impl<T: Send, E> ResourceResolve<T, E> {
    pub fn empty() -> Self {
        Self(ResourceResolveType::Resource(None))
    }

    pub fn is_empty(&self) -> bool {
        matches!(self.0, ResourceResolveType::Resource(None))
    }

    pub fn is_pending(&self) -> bool {
        matches!(self.0, ResourceResolveType::Future(..))
    }

    pub fn take_resource(&mut self) -> Option<ResourceGuard<T>> {
        if let ResourceResolveType::Resource(ref mut res) = &mut self.0 {
            res.take().map(|(res, _)| res)
        } else {
            None
        }
    }
}

impl<T: Send, E> From<(ResourceGuard<T>, Arc<Shared<T>>)> for ResourceResolve<T, E> {
    fn from((guard, shared): (ResourceGuard<T>, Arc<Shared<T>>)) -> Self {
        Self(ResourceResolveType::Resource(Some((guard, shared))))
    }
}

impl<T: Send, E> From<Option<(ResourceGuard<T>, Arc<Shared<T>>)>> for ResourceResolve<T, E> {
    fn from(res: Option<(ResourceGuard<T>, Arc<Shared<T>>)>) -> Self {
        Self(ResourceResolveType::Resource(res))
    }
}

impl<T: Send, E> From<(ResourceFuture<T, E>, Arc<PoolInternal<T, E>>)> for ResourceResolve<T, E> {
    fn from((fut, exec): (ResourceFuture<T, E>, Arc<PoolInternal<T, E>>)) -> Self {
        Self(ResourceResolveType::Future(fut, exec))
    }
}

impl<T: Send, E> Drop for ResourceResolve<T, E> {
    fn drop(&mut self) {
        let mut entry = ResourceResolveType::Resource(None);
        std::mem::swap(&mut self.0, &mut entry);
        match entry {
            ResourceResolveType::Future(fut, pool) => {
                pool.release_future(fut);
            }
            ResourceResolveType::Resource(Some((res, shared))) => {
                shared.release(res);
            }
            ResourceResolveType::Resource(None) => (),
        }
    }
}

impl<T: Send, E> Future for ResourceResolve<T, E> {
    type Output = Option<Result<ResourceGuard<T>, E>>;

    fn poll(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<ResourceGuard<T>, E>>> {
        match self.0 {
            ResourceResolveType::Resource(ref mut res) => Poll::Ready(res.take().map(|r| Ok(r.0))),
            ResourceResolveType::Future(ref mut fut, _) => match fut.as_mut().poll(cx) {
                Poll::Pending => Poll::Pending,
                Poll::Ready(result) => {
                    self.0 = ResourceResolveType::Resource(None);
                    Poll::Ready(Some(result))
                }
            },
        }
    }
}

pub trait ResourceOperation<T: Send, E> {
    fn apply<'a>(
        &self,
        guard: ResourceGuard<T>,
        pool: &Arc<PoolInternal<T, E>>,
    ) -> ResourceResolve<T, E>;
}

pub struct ResourceUpdateFn<F> {
    inner: F,
}

impl<T: Send, E, F> ResourceOperation<T, E> for ResourceUpdateFn<F>
where
    F: Fn(ResourceGuard<T>) -> ResourceFuture<T, E>,
{
    fn apply<'a>(
        &self,
        guard: ResourceGuard<T>,
        pool: &Arc<PoolInternal<T, E>>,
    ) -> ResourceResolve<T, E> {
        ((self.inner)(guard), pool.clone()).into()
    }
}

pub fn resource_create<C, F, T, E>(ctor: C) -> impl ResourceOperation<T, E>
where
    C: Fn() -> F + Send + Sync,
    F: Future<Output = Result<T, E>> + Send + 'static,
    T: Send + 'static,
{
    ResourceUpdateFn {
        inner: move |mut guard: ResourceGuard<T>| {
            assert!(guard.is_none());
            let result = ctor();
            async move {
                result.await.map(|res| {
                    guard.replace(res);
                    guard.info_mut().created_at.replace(Instant::now());
                    guard
                })
            }
            .boxed()
        },
    }
}

pub fn resource_verify<C, F, T, E>(update: C) -> impl ResourceOperation<T, E>
where
    C: Fn(&mut T, ResourceInfo) -> F + Send + Sync,
    F: Future<Output = Result<bool, E>> + Send + 'static,
    T: Send + 'static,
{
    ResourceUpdateFn {
        inner: move |mut guard: ResourceGuard<T>| {
            let mut res = guard.take().unwrap();
            let result = update(&mut res, *guard.info());
            async move {
                result.await.map(|verified| {
                    guard.replace(res);
                    guard.info_mut().expired = !verified;
                    guard
                })
            }
            .boxed()
        },
    }
}
