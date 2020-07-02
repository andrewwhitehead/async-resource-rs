/// Slightly cheaper version of futures::oneshot
/// In this case we only need a sync sender and an async receiver,
/// and the result is simply dropped if it can't be sent.
use super::option_lock::OptionLock;
use std::future::Future;
use std::pin::Pin;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::task::{Context, Poll, Waker};

pub fn channel<T>() -> (Sender<T>, Receiver<T>) {
    let inner = Arc::new(Inner::new());
    let receiver = Receiver {
        inner: inner.clone(),
    };
    let sender = Sender { inner };
    (sender, receiver)
}

pub struct Inner<T> {
    complete: AtomicBool,
    data: OptionLock<T>,
    waker: OptionLock<Waker>,
}

impl<T> Inner<T> {
    pub fn new() -> Self {
        Self {
            complete: AtomicBool::new(false),
            data: OptionLock::new(None),
            waker: OptionLock::new(None),
        }
    }

    pub fn finalize(&self, wake: bool) {
        if !self.complete.swap(true, Ordering::SeqCst) {
            if let Some(waker) = self.waker.try_take() {
                if wake {
                    waker.wake();
                }
            }
        }
    }

    pub fn recv(&self, cx: &mut Context<'_>) -> Poll<Option<T>> {
        let done = if self.complete.load(Ordering::SeqCst) {
            true
        } else {
            // if the acquire fails, it's because the sender is already being dropped
            // can safely be ignored
            let waker = cx.waker().clone();
            match self.waker.try_lock() {
                Some(mut guard) => {
                    guard.replace(waker);
                    false
                }
                None => true,
            }
        };

        // check if complete was set while the waker was locked
        if done || self.complete.load(Ordering::SeqCst) {
            // this acquire should never fail because complete is not set until
            // the other side has been dropped. in that rare occurrance, treat it
            // as a failed send
            Poll::Ready(self.data.try_take())
        } else {
            Poll::Pending
        }
    }

    pub fn send(&self, data: T) -> Option<T> {
        if !self.complete.load(Ordering::Acquire) {
            // this acquire should never fail, but we don't mind if it does
            if let Some(mut guard) = self.data.try_lock() {
                if guard.is_none() {
                    guard.replace(data);
                    return None;
                }
            }
        }
        return Some(data);
    }
}

pub struct Receiver<T> {
    inner: Arc<Inner<T>>,
}

impl<T> Future for Receiver<T> {
    type Output = Option<T>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<T>> {
        self.inner.recv(cx)
    }
}

impl<T> Drop for Receiver<T> {
    fn drop(&mut self) {
        self.inner.finalize(false)
    }
}

pub struct Sender<T> {
    inner: Arc<Inner<T>>,
}

impl<T> Sender<T> {
    pub fn send(&self, data: T) -> Option<T> {
        self.inner.send(data)
    }
}

impl<T> Drop for Sender<T> {
    fn drop(&mut self) {
        self.inner.finalize(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::task::{waker_ref, ArcWake};
    use std::sync::atomic::AtomicUsize;

    struct TestWaker {
        calls: AtomicUsize,
    }

    impl TestWaker {
        pub fn new() -> Self {
            Self {
                calls: AtomicUsize::new(0),
            }
        }

        pub fn count(&self) -> usize {
            return self.calls.load(Ordering::Acquire);
        }
    }

    impl ArcWake for TestWaker {
        fn wake_by_ref(arc_self: &Arc<Self>) {
            arc_self.calls.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[test]
    fn send_normal() {
        let (sender, mut receiver) = channel();
        let waker = Arc::new(TestWaker::new());
        let wr = waker_ref(&waker);
        let mut cx = Context::from_waker(&wr);
        assert_eq!(Pin::new(&mut receiver).poll(&mut cx), Poll::Pending);
        assert_eq!(waker.count(), 0);
        assert_eq!(sender.send(1u32), None);
        assert_eq!(Pin::new(&mut receiver).poll(&mut cx), Poll::Pending);
        assert_eq!(waker.count(), 0);
        drop(sender);
        assert_eq!(
            Pin::new(&mut receiver).poll(&mut cx),
            Poll::Ready(Some(1u32))
        );
        assert_eq!(waker.count(), 1);
    }

    #[test]
    fn sender_dropped() {
        let (sender, mut receiver) = channel::<u32>();
        let waker = Arc::new(TestWaker::new());
        let wr = waker_ref(&waker);
        let mut cx = Context::from_waker(&wr);
        assert_eq!(Pin::new(&mut receiver).poll(&mut cx), Poll::Pending);
        drop(sender);
        assert_eq!(Pin::new(&mut receiver).poll(&mut cx), Poll::Ready(None));
        assert_eq!(waker.count(), 1);
    }

    #[test]
    fn receiver_dropped() {
        let (sender, receiver) = channel();
        drop(receiver);
        assert_eq!(sender.send(1u32), Some(1u32));
    }
}
