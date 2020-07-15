use std::sync::Arc;

use super::lock::ResourceGuard;
use super::operation::{ResourceOperation, ResourceResolve};
use crate::pool::PoolInner;

pub struct Lifecycle<T: Send, E> {
    pub create: Box<dyn ResourceOperation<T, E> + Send + Sync>,
    pub dispose: Option<Box<dyn ResourceOperation<T, E> + Send + Sync>>,
    pub handle_error: Option<Box<dyn Fn(E) + Send + Sync>>,
    pub keepalive: Option<Box<dyn ResourceOperation<T, E> + Send + Sync>>,
}

impl<T: Send, E> Lifecycle<T, E> {
    pub fn new(create: Box<dyn ResourceOperation<T, E> + Send + Sync>) -> Self {
        Self {
            create,
            dispose: None,
            handle_error: None,
            keepalive: None,
        }
    }

    pub fn create(
        &self,
        guard: ResourceGuard<T>,
        pool: &Arc<PoolInner<T, E>>,
    ) -> ResourceResolve<T, E> {
        self.create.apply(guard, pool)
    }

    pub fn keepalive(
        &self,
        guard: ResourceGuard<T>,
        pool: &Arc<PoolInner<T, E>>,
    ) -> ResourceResolve<T, E> {
        println!("keepalive");
        if let Some(handler) = self.keepalive.as_ref() {
            handler.apply(guard, pool)
        } else {
            self.dispose(guard, pool)
        }
    }

    pub fn handle_error(&self, err: E) {
        if let Some(handler) = self.handle_error.as_ref() {
            (handler)(err)
        }
    }

    // pub fn verify_acquire(&self, fut: ResourceFuture<R, E>) -> ResourceFuture<R, E> {
    //     fut
    // }

    // pub fn verify_release(&self, fut: ResourceFuture<R, E>) -> ResourceFuture<R, E> {
    //     fut
    // }

    pub fn dispose(
        &self,
        guard: ResourceGuard<T>,
        pool: &Arc<PoolInner<T, E>>,
    ) -> ResourceResolve<T, E> {
        println!("dispose");
        if let Some(handler) = self.dispose.as_ref() {
            handler.apply(guard, pool)
        } else {
            ResourceResolve::empty()
        }
    }
}
