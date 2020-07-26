use std::future::Future;
use std::pin::Pin;
use std::task::Poll;
use std::time::Instant;

use suspend::Task;

use super::error::AcquireError;
use super::operation::ResourceResolve;
use super::pool::Pool;
use super::wait::Waiter;
use crate::resource::Managed;

pub type Acquire<T, E> = Task<'static, Result<Managed<T>, AcquireError<E>>>;

enum AcquireState<T: Send + 'static, E: 'static> {
    Init,
    Resolve(ResourceResolve<T, E>),
    Waiting(Waiter<ResourceResolve<T, E>>),
}

pub fn acquire<T: Send + 'static, E: 'static>(pool: Pool<T, E>) -> Acquire<T, E> {
    let start = Instant::now();
    let mut save_state = Some(AcquireState::Init);

    Task::from_poll(move |cx| {
        let mut state = match save_state.take() {
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
                    let mut resolve = pool.inner.try_acquire_idle();
                    if resolve.is_empty() {
                        // Queue is empty, try to create a new resource
                        resolve = pool.inner.try_create();
                    }

                    if resolve.is_empty() {
                        // Register a waiter
                        if let Some(waiter) = pool.inner.try_wait(start) {
                            AcquireState::Waiting(waiter)
                        } else {
                            return Poll::Ready(Err(AcquireError::PoolBusy));
                        }
                    } else {
                        // Evaluate any attached future if necessary
                        AcquireState::Resolve(resolve)
                    }
                }

                AcquireState::Resolve(mut resolve) => match Pin::new(&mut resolve).poll(cx) {
                    Poll::Pending => {
                        // FIXME check timer (need to register one)
                        save_state.replace(AcquireState::Resolve(resolve));
                        return Poll::Pending;
                    }
                    Poll::Ready(Some(Ok(guard))) => {
                        if guard.info().expired {
                            pool.inner.shared().release(guard);
                            // retry
                            AcquireState::Init
                        } else {
                            return Poll::Ready(Ok(Managed::new(
                                guard,
                                pool.inner.shared().clone(),
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
                        save_state.replace(AcquireState::Waiting(waiter));
                        return Poll::Pending;
                    }
                    Poll::Ready(result) => match result {
                        Ok(resolve) => AcquireState::Resolve(resolve),
                        Err(_) => return Poll::Ready(Err(AcquireError::Timeout)),
                    },
                },
            };
        }
    })
}
