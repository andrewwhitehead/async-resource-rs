use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::thread;
use std::time::Instant;

use pin_utils::pin_mut;

use super::local_waker;

/// A convenience method to evaluate a `Future`, blocking the current thread
/// until it is resolved.
pub fn block_on<F>(fut: F) -> F::Output
where
    F: Future,
{
    pin_mut!(fut);
    local_waker!(thread_waker, thread::current());
    let mut cx = Context::from_waker(&*thread_waker);
    loop {
        match fut.as_mut().poll(&mut cx) {
            Poll::Ready(result) => break result,
            Poll::Pending => thread::park(),
        }
    }
}

/// A convenience method to evaluate a `Future`, blocking the current thread
/// until it is resolved or the timeout expires.
pub fn block_on_deadline<F>(mut fut: F, expire: Instant) -> Result<F::Output, F>
where
    F: Future + Unpin,
{
    let mut pin_fut = Pin::new(&mut fut);
    local_waker!(thread_waker, thread::current());
    let mut cx = Context::from_waker(&*thread_waker);
    loop {
        match pin_fut.as_mut().poll(&mut cx) {
            Poll::Ready(result) => break Ok(result),
            Poll::Pending => {
                if let Some(dur) = expire.checked_duration_since(Instant::now()) {
                    thread::park_timeout(dur);
                } else {
                    break Err(fut);
                }
            }
        }
    }
}