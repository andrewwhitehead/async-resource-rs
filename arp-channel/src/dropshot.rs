use std::cell::UnsafeCell;
use std::fmt;
use std::future::Future;
use std::mem::MaybeUninit;
use std::pin::Pin;
use std::sync::{
    atomic::{AtomicU8, Ordering},
    Arc,
};
use std::task::{Context, Poll, Waker};
use std::thread;

use super::option_lock::OptionLock;
use super::waker::UnparkWaker;

/// Alternative version of futures::oneshot
/// In this case poll_cancelled is not available. It could be added
/// at the expense of another waker per message. This could also be used
/// to confirm delivery of a message and pull it back out on failure.

const INIT: u8 = 0;
const LOAD: u8 = 1;
const READY: u8 = 2;
const SENT: u8 = 3;
const CANCEL: u8 = 4;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Canceled;

impl fmt::Display for Canceled {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "dropshot canceled")
    }
}

impl std::error::Error for Canceled {}

pub fn channel<T>() -> (Sender<T>, Receiver<T>) {
    let inner = Arc::new(Inner::new());
    let receiver = Receiver {
        inner: inner.clone(),
    };
    let sender = Sender { inner };
    (sender, receiver)
}

struct Inner<T> {
    data: UnsafeCell<MaybeUninit<T>>,
    recv_waker: OptionLock<Waker>,
    state: AtomicU8,
}

unsafe impl<T> Sync for Inner<T> {}

impl<T> Inner<T> {
    pub fn new() -> Self {
        Self {
            data: UnsafeCell::new(MaybeUninit::uninit()),
            recv_waker: OptionLock::new(None),
            state: AtomicU8::new(INIT),
        }
    }

    pub fn cancel_recv(&self) -> Option<T> {
        match self.state.swap(CANCEL, Ordering::SeqCst) {
            READY => Some(self.take()),
            _ => None,
        }
    }

    pub fn cancel_send(&self) {
        if self.state.compare_and_swap(INIT, CANCEL, Ordering::SeqCst) == INIT {
            if let Some(waker) = self.recv_waker.try_take() {
                waker.wake();
            }
        }
    }

    pub fn poll_recv(&self, cx: &mut Context<'_>) -> Poll<Result<T, Canceled>> {
        loop {
            match self.try_recv() {
                Ok(Some(val)) => return Poll::Ready(Ok(val)),
                Ok(None) => {
                    let waker = cx.waker().clone();
                    match self.recv_waker.try_lock() {
                        Some(mut guard) => {
                            guard.replace(waker);
                        }
                        None => {
                            // the sender is already trying to wake us
                            continue;
                        }
                    }

                    // check the state again, in case the sender
                    // failed to get a lock on the waker because we were storing it
                    match self.state.load(Ordering::Acquire) {
                        INIT => {
                            return Poll::Pending;
                        }
                        CANCEL => {
                            // sender dropped
                            return Poll::Ready(Err(Canceled));
                        }
                        LOAD => {
                            // sender was interrupted while setting the value, spin
                            thread::yield_now();
                            continue;
                        }
                        READY => {
                            // sender completed concurrently
                            continue;
                        }
                        _ => {
                            panic!("Invalid state for dropshot");
                        }
                    }
                }
                Err(err) => return Poll::Ready(Err(err)),
            }
        }
    }

    pub fn try_recv(&self) -> Result<Option<T>, Canceled> {
        loop {
            match self
                .state
                .compare_exchange_weak(READY, SENT, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => {
                    return Ok(Some(self.take()));
                }
                Err(INIT) => {
                    return Ok(None);
                }
                Err(CANCEL) => {
                    // sender dropped
                    return Err(Canceled);
                }
                Err(LOAD) => {
                    // sender was interrupted while setting the value, spin
                    thread::yield_now();
                    continue;
                }
                Err(READY) => {
                    // spurious failure
                    continue;
                }
                Err(SENT) => {
                    // receive was called after taking the value
                    return Err(Canceled);
                }
                Err(_) => {
                    panic!("Invalid state for dropshot");
                }
            }
        }
    }

    pub fn send(&self, value: T) -> Option<T> {
        loop {
            match self
                .state
                .compare_exchange_weak(INIT, LOAD, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => {
                    unsafe { self.data.get().write(MaybeUninit::new(value)) };
                    match self.state.compare_exchange(
                        LOAD,
                        READY,
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    ) {
                        Ok(_) => {
                            if let Some(waker) = self.recv_waker.try_take() {
                                waker.wake();
                            }
                            return None;
                        }
                        Err(CANCEL) => {
                            // receiver dropped mid-send
                            return Some(self.take());
                        }
                        _ => panic!("Invalid state for dropshot"),
                    }
                }
                Err(INIT) => {
                    // spurious failure
                    continue;
                }
                Err(CANCEL) | Err(LOAD) | Err(READY) | Err(SENT) => {
                    // receiver hung up, or send was called repeatedly
                    return Some(value);
                }
                Err(_) => {
                    panic!("Invalid state for dropshot");
                }
            }
        }
    }

    #[inline]
    fn take(&self) -> T {
        unsafe { self.data.get().read().assume_init() }
    }

    pub fn state(&self) -> (u8, bool) {
        (
            self.state.load(Ordering::Acquire),
            self.recv_waker.try_take().is_some(),
        )
    }
}

pub struct Receiver<T> {
    inner: Arc<Inner<T>>,
}

impl<T> Receiver<T> {
    pub fn close(&mut self) -> Option<T> {
        self.inner.cancel_recv()
    }

    pub fn recv(&mut self) -> Result<T, Canceled> {
        for _ in 0..20 {
            match self.inner.try_recv() {
                Ok(Some(value)) => return Ok(value),
                Ok(None) => {
                    thread::yield_now();
                }
                Err(err) => return Err(err),
            }
        }
        let waker = UnparkWaker::new();
        loop {
            match self.inner.poll_recv(&mut waker.context()) {
                Poll::Ready(result) => return result,
                Poll::Pending => {
                    // println!("park!");
                    let ts = std::time::Instant::now();
                    thread::park_timeout(std::time::Duration::from_millis(500));
                    if std::time::Instant::now() - ts > std::time::Duration::from_millis(100) {
                        println!("{:?}", self.inner.state());
                        return Err(Canceled);
                    }
                }
            }
            //println!("unpark!");
        }
    }

    pub fn try_recv(&mut self) -> Result<Option<T>, Canceled> {
        self.inner.try_recv()
    }
}

impl<T> Future for Receiver<T> {
    type Output = Result<T, Canceled>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<T, Canceled>> {
        self.inner.poll_recv(cx)
    }
}

impl<T> Drop for Receiver<T> {
    fn drop(&mut self) {
        self.inner.cancel_recv();
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
        self.inner.cancel_send()
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
    fn dropshot_send_normal() {
        let (sender, mut receiver) = channel();
        let waker = Arc::new(TestWaker::new());
        let wr = waker_ref(&waker);
        let mut cx = Context::from_waker(&wr);
        assert_eq!(Pin::new(&mut receiver).poll(&mut cx), Poll::Pending);
        assert_eq!(waker.count(), 0);
        assert_eq!(sender.send(1u32), None);
        assert_eq!(waker.count(), 1);
        assert_eq!(Pin::new(&mut receiver).poll(&mut cx), Poll::Ready(Ok(1u32)));
        drop(sender);
        assert_eq!(waker.count(), 1);
        assert_eq!(
            Pin::new(&mut receiver).poll(&mut cx),
            Poll::Ready(Err(Canceled))
        );
        assert_eq!(waker.count(), 1);
    }

    #[test]
    fn dropshot_sender_dropped() {
        let (sender, mut receiver) = channel::<u32>();
        let waker = Arc::new(TestWaker::new());
        let wr = waker_ref(&waker);
        let mut cx = Context::from_waker(&wr);
        assert_eq!(Pin::new(&mut receiver).poll(&mut cx), Poll::Pending);
        drop(sender);
        assert_eq!(waker.count(), 1);
        assert_eq!(
            Pin::new(&mut receiver).poll(&mut cx),
            Poll::Ready(Err(Canceled))
        );
        assert_eq!(waker.count(), 1);
    }

    #[test]
    fn dropshot_receiver_dropped() {
        let (sender, receiver) = channel();
        drop(receiver);
        assert_eq!(sender.send(1u32), Some(1u32));
    }

    #[test]
    fn dropshot_test_future() {
        use futures_executor::block_on;
        let (sender, receiver) = channel::<u32>();
        sender.send(5);
        assert_eq!(block_on(receiver), Ok(5));
    }
}
