use std::fmt::Debug;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Instant;

use super::error::AcquireError;
use super::operation::ResourceResolve;
use super::pool::Pool;
use super::wait::Waiter;
use crate::resource::Managed;

enum AcquireState<T: Send + 'static, E: 'static> {
    Init,
    Resolve(ResourceResolve<T, E>),
    Waiting(Waiter<ResourceResolve<T, E>>),
}

/// A Future resolving to a `Managed<T>` or an `AcquireError`.
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

impl<T: Send, E: Debug> Future for Acquire<T, E> {
    type Output = Result<Managed<T>, AcquireError<E>>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut state = match self.state.take() {
            Some(state) => state,
            None => {
                // future already completed
                return Poll::Ready(Err(AcquireError::PoolClosed));
            }
        };

        loop {
            state = match state {
                AcquireState::Init => {
                    // FIXME check timer since we may return here after a failure

                    // Try to acquire from the idle queue
                    let mut resolve = self.pool.inner.try_acquire_idle();
                    if resolve.is_empty() {
                        // Queue is empty, try to create a new resource
                        resolve = self.pool.inner.try_create();
                    }

                    if resolve.is_empty() {
                        // Register a waiter
                        let waiter = self.pool.inner.try_wait(self.start);
                        AcquireState::Waiting(waiter)
                    } else {
                        // Evaluate any attached future if necessary
                        AcquireState::Resolve(resolve)
                    }
                }

                AcquireState::Resolve(mut resolve) => match Pin::new(&mut resolve).poll(cx) {
                    Poll::Pending => {
                        // FIXME check timer (need to register one)
                        self.state.replace(AcquireState::Resolve(resolve));
                        return Poll::Pending;
                    }
                    Poll::Ready(Some(Ok(guard))) => {
                        if guard.info().expired {
                            self.pool.inner.shared().release(guard);
                            // retry
                            AcquireState::Init
                        } else {
                            return Poll::Ready(Ok(Managed::new(
                                guard,
                                self.pool.inner.shared().clone(),
                            )));
                        }
                    }
                    Poll::Ready(Some(Err(err))) => {
                        return Poll::Ready(Err(AcquireError::ResourceError(err)));
                    }
                    Poll::Ready(None) => {
                        // Something went wrong during the resolve, start over
                        AcquireState::Init
                    }
                },

                AcquireState::Waiting(mut waiter) => match Pin::new(&mut *waiter).poll(cx) {
                    Poll::Pending => {
                        self.state.replace(AcquireState::Waiting(waiter));
                        return Poll::Pending;
                    }
                    Poll::Ready(result) => match result {
                        Ok(resolve) => AcquireState::Resolve(resolve),
                        Err(_) => return Poll::Ready(Err(AcquireError::Timeout)),
                    },
                },
            };
        }
    }
}
