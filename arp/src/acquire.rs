use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use super::error::AcquireError;
use super::managed::Managed;
use super::pool::Pool;
use super::resource::ResourceFuture;

enum AcquireState<T, E> {
    Init,
    // Waiting(Waiter)
    Resolve(ResourceFuture<T, E>),
}

pub struct Acquire<T, E> {
    pool: Pool<T, E>,
    state: Option<AcquireState<T, E>>,
}

impl<T, E> Acquire<T, E> {
    pub fn new(pool: Pool<T, E>) -> Self {
        Self {
            pool,
            state: Some(AcquireState::Init),
        }
    }
}

impl<T, E> Future for Acquire<T, E> {
    type Output = Result<Managed<T>, AcquireError<E>>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut state = match self.state.take() {
            Some(state) => state,
            None => {
                // future already completed
                return Poll::Ready(Err(AcquireError::Stopped));
            }
        };

        loop {
            state = match state {
                AcquireState::Init => {
                    // Try to acquire from the idle queue
                    if let Some(guard) = self.pool.inner.try_acquire_idle() {
                        // FIXME make ready
                        let mng = Managed::new(guard, self.pool.state().clone());
                        return Poll::Ready(Ok(mng));
                    }

                    // Queue is empty, try to create a new resource
                    if let Some(fut) = self.pool.inner.try_create() {
                        AcquireState::Resolve(fut)
                    } else {
                        // Register a waiter
                        AcquireState::Init
                    }
                }
                AcquireState::Resolve(mut fut) => match fut.as_mut().poll(cx) {
                    Poll::Pending => {
                        self.state.replace(AcquireState::Resolve(fut));
                        return Poll::Pending;
                    }
                    Poll::Ready(res) => {
                        let res = res
                            .map(|guard| Managed::new(guard, self.pool.state().clone()))
                            .map_err(AcquireError::ResourceError);
                        return Poll::Ready(res);
                    }
                }, // AcquireState::Wait(timer) => {
                   //     if timer.completed.load(Ordering::Acquire) {
                   //         return Poll::Ready(Err(AcquireError::Timeout));
                   //     }
                   //     timer.update(Some(cx.waker()));
                   //     self.inner.replace(AcquireState::Wait(timer));
                   //     return Poll::Pending;
                   // }
            };
        }
    }
}
