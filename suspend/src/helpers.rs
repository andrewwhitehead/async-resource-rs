use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::thread;
use std::time::Instant;

use pin_utils::pin_mut;

use super::waker::THREAD_WAKER;

/// A convenience method to evaluate a `Future`, blocking the current thread
/// until it is resolved.
pub fn block_on<F>(fut: F) -> F::Output
where
    F: Future,
{
    pin_mut!(fut);
    block_on_poll(|cx| fut.as_mut().poll(cx))
}

/// Call a polling function, locking the current thread until it resolves.
#[inline]
pub fn block_on_poll<F, R>(mut poll_fn: F) -> R
where
    F: FnMut(&mut Context) -> Poll<R>,
{
    THREAD_WAKER.with(|waker| {
        let mut cx = Context::from_waker(waker);
        loop {
            match poll_fn(&mut cx) {
                Poll::Ready(result) => break result,
                Poll::Pending => thread::park(),
            }
        }
    })
}

/// A convenience method to evaluate a `Future`, blocking the current thread
/// until it is resolved or the timeout expires.
pub fn block_on_deadline<F>(mut fut: F, expire: Instant) -> Result<F::Output, F>
where
    F: Future + Unpin,
{
    let mut pin_fut = Pin::new(&mut fut);
    match block_on_poll_deadline(|cx| pin_fut.as_mut().poll(cx), expire) {
        Ok(r) => Ok(r),
        Err(_) => Err(fut),
    }
}

/// Call a polling function, locking the current thread until it resolves
/// or the timeout expires.
#[inline]
pub fn block_on_poll_deadline<F, R>(mut poll_fn: F, expire: Instant) -> Result<R, F>
where
    F: FnMut(&mut Context) -> Poll<R>,
{
    THREAD_WAKER.with(|waker| {
        let mut cx = Context::from_waker(waker);
        loop {
            match poll_fn(&mut cx) {
                Poll::Ready(result) => break Ok(result),
                Poll::Pending => {
                    if let Some(dur) = expire.checked_duration_since(Instant::now()) {
                        thread::park_timeout(dur);
                    } else {
                        break Err(poll_fn);
                    }
                }
            }
        }
    })
}
