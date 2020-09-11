use std::future::Future;
use std::pin::Pin;
use std::task::Poll;
use std::time::Instant;

use super::thread::{thread_suspend, thread_suspend_deadline};

/// A convenience method to evaluate a `Future`, blocking the current thread
/// until it is resolved.
pub fn block_on<F>(fut: F) -> F::Output
where
    F: Future,
{
    futures_lite::pin!(fut);
    thread_suspend(|cx| fut.as_mut().poll(cx))
}

/// A convenience method to evaluate a `Future`, blocking the current thread
/// until it is resolved or the timeout expires.
pub fn block_on_deadline<F>(mut fut: F, expire: Instant) -> Result<F::Output, F>
where
    F: Future + Unpin,
{
    let mut pin_fut = Pin::new(&mut fut);
    match thread_suspend_deadline(|cx| pin_fut.as_mut().poll(cx), Some(expire)) {
        Poll::Ready(r) => Ok(r),
        Poll::Pending => Err(fut),
    }
}
