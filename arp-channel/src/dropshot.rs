use super::option_lock::OptionLock;
use std::cell::UnsafeCell;
use std::future::Future;
use std::mem::MaybeUninit;
use std::pin::Pin;
use std::sync::{
    atomic::{AtomicU8, Ordering},
    Arc,
};
use std::task::{Context, Poll, Waker};
use std::thread;

/// Alternative version of futures::oneshot
/// In this case poll_cancelled is not available. It could be added
/// at the expense of another waker per message. This could also be used
/// to confirm delivery of a message and pull it back out on failure.

const INIT: u8 = 0;
const LOAD: u8 = 1;
const READY: u8 = 2;
const SENT: u8 = 3;
const CANCEL: u8 = 4;

pub fn channel<T>() -> (Sender<T>, Receiver<T>) {
    let inner = Arc::new(Inner::new());
    let receiver = Receiver {
        inner: inner.clone(),
    };
    let sender = Sender { inner };
    (sender, receiver)
}

pub struct Inner<T> {
    data: UnsafeCell<MaybeUninit<T>>,
    rx_waker: OptionLock<Waker>,
    state: AtomicU8,
}

impl<T> Inner<T> {
    pub fn new() -> Self {
        Self {
            data: UnsafeCell::new(MaybeUninit::uninit()),
            rx_waker: OptionLock::new(None),
            state: AtomicU8::new(INIT),
        }
    }

    pub fn cancel_rx(&self) {
        self.state.store(CANCEL, Ordering::Release)
    }

    pub fn cancel_tx(&self) {
        if self.state.compare_and_swap(INIT, CANCEL, Ordering::AcqRel) == INIT {
            if let Some(waker) = self.rx_waker.try_take() {
                waker.wake();
            }
        }
    }

    pub fn recv(&self, cx: &mut Context<'_>) -> Poll<Option<T>> {
        loop {
            match self
                .state
                .compare_exchange_weak(READY, SENT, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => {
                    self.state.store(SENT, Ordering::Release);
                    return Poll::Ready(Some(unsafe { self.data.get().read().assume_init() }));
                }
                Err(INIT) => {
                    let waker = cx.waker().clone();
                    match self.rx_waker.try_lock() {
                        Some(mut guard) => {
                            guard.replace(waker);
                        }
                        None => {
                            // the sender is already trying to wake us
                            continue;
                        }
                    }
                    // check the state one more time, in case the sender
                    // failed to get a lock on the waker because we were storing it
                    match self.state.load(Ordering::Acquire) {
                        CANCEL => {
                            // sender dropped
                            println!("sender dropped 1");
                            return Poll::Ready(None);
                        }
                        LOAD => {
                            // sender is currently setting the value, spin
                            thread::yield_now();
                            continue;
                        }
                        READY => {
                            // already completed
                            continue;
                        }
                        _ => return Poll::Pending,
                    }
                }
                Err(CANCEL) => {
                    // sender dropped
                    println!("sender dropped");
                    return Poll::Ready(None);
                }
                Err(LOAD) => {
                    // sender is currently setting the value, spin
                    thread::yield_now();
                    continue;
                }
                Err(READY) => {
                    // spurious failure
                    continue;
                }
                Err(SENT) => {
                    // repeated call to receive
                    println!("already received?");
                    return Poll::Ready(None);
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
                    self.state.store(READY, Ordering::Release);
                    if let Some(waker) = self.rx_waker.try_take() {
                        waker.wake();
                    }
                    return None;
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
        self.inner.cancel_rx()
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
        self.inner.cancel_tx()
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
        assert_eq!(
            Pin::new(&mut receiver).poll(&mut cx),
            Poll::Ready(Some(1u32))
        );
        drop(sender);
        assert_eq!(waker.count(), 1);
        assert_eq!(Pin::new(&mut receiver).poll(&mut cx), Poll::Ready(None));
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
        assert_eq!(Pin::new(&mut receiver).poll(&mut cx), Poll::Ready(None));
        assert_eq!(waker.count(), 1);
    }

    #[test]
    fn dropshot_receiver_dropped() {
        let (sender, receiver) = channel();
        drop(receiver);
        assert_eq!(sender.send(1u32), Some(1u32));
    }
}
