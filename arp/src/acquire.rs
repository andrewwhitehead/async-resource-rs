use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Instant;

use super::error::AcquireError;
use super::managed::Managed;
use super::pool::Pool;
use super::resource::ResourceResolve;

enum AcquireState<T: Send + 'static, E: 'static> {
    Init,
    // Waiting(Waiter)
    Resolve(ResourceResolve<T, E>),
}

pub struct Acquire<T: Send + 'static, E: 'static> {
    pool: Pool<T, E>,
    start: Instant,
    state: Option<AcquireState<T, E>>,
}

impl<T: Send, E> Acquire<T, E> {
    pub fn new(pool: Pool<T, E>) -> Self {
        Self {
            pool,
            start: Instant::now(),
            state: Some(AcquireState::Init),
        }
    }
}

impl<T: Send, E> Future for Acquire<T, E> {
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
                    let mut resolve = self.pool.inner.try_acquire_idle();
                    if resolve.is_empty() {
                        // Queue is empty, try to create a new resource
                        resolve = self.pool.inner.try_create();
                    }

                    if resolve.is_empty() {
                        // FIXME Register a waiter
                        AcquireState::Init
                    } else {
                        AcquireState::Resolve(resolve)
                    }
                }

                AcquireState::Resolve(mut resolve) => match Pin::new(&mut resolve).poll(cx) {
                    Poll::Pending => {
                        self.state.replace(AcquireState::Resolve(resolve));
                        return Poll::Pending;
                    }
                    Poll::Ready(Some(res)) => {
                        let res = res
                            .map(|guard| Managed::new(guard, self.pool.queue().clone()))
                            .map_err(AcquireError::ResourceError);
                        return Poll::Ready(res);
                    }
                    Poll::Ready(None) => {
                        // Something went wrong during the resolve, start over
                        AcquireState::Init
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

impl<T: Send, E> Drop for Acquire<T, E> {
    fn drop(&mut self) {
        // FIXME in Waiting state, try to close receiver
        // return value to pool otherwise

        // in Resolve state, return future to executor
        // pool.exec(fut)
    }
}
