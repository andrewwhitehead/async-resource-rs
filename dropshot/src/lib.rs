use std::cell::UnsafeCell;
use std::fmt;
use std::future::Future;
use std::mem::MaybeUninit;
use std::pin::Pin;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use std::task::{Context, Poll, Waker};
use std::thread;

use suspend::NotifyOnce;

const INIT: usize = 0x0;
const LOAD: usize = 0x1;
const CANCEL: usize = 0x2;
const SENT: usize = 0x3;

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
    recv_task: NotifyOnce,
    send_state: AtomicUsize,
}

unsafe impl<T> Sync for Inner<T> {}

impl<T> Inner<T> {
    pub const fn new() -> Self {
        Self {
            data: UnsafeCell::new(MaybeUninit::uninit()),
            recv_task: NotifyOnce::new(),
            send_state: AtomicUsize::new(INIT),
        }
    }

    pub fn cancel_recv(&self) -> Option<T> {
        if !self.recv_task.cancel() {
            // already woken by sender, so result is loaded or canceled
            let prev_state = self.send_state.fetch_or(CANCEL, Ordering::AcqRel);
            match prev_state & SENT {
                INIT => {
                    let cancel = (prev_state & !SENT) as *const NotifyCancel;
                    if !cancel.is_null() {
                        unsafe {
                            (*cancel).notify.complete();
                        }
                    }
                }
                LOAD => return Some(self.take()),
                _ => (),
            }
        }
        None
    }

    pub fn cancel_send(&self) {
        let state = INIT;
        loop {
            match self.send_state.compare_exchange_weak(
                state,
                CANCEL,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    self.recv_task.complete();
                }
                Err(INIT) => {}
                Err(CANCEL) | Err(LOAD) | Err(SENT) => {
                    break;
                }
                Err(_) => panic!("Invalid state for dropshot"),
            }
        }
    }

    pub fn poll_canceled(&self, state: Option<&NotifyCancel>) -> Poll<()> {
        if self.recv_task.is_complete() {
            Poll::Ready(())
        } else {
            let state_ptr = if let Some(state) = state {
                state as *const NotifyCancel as usize
            } else {
                INIT
            };
            if self.send_state.swap(state_ptr, Ordering::Release) & CANCEL == CANCEL {
                // we were pre-empted by receive cancellation
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        }
    }

    pub fn poll_recv(&self, waker: Option<&Waker>) -> Poll<Result<T, Canceled>> {
        let mut state = self.send_state.load(Ordering::Acquire);
        loop {
            match state {
                INIT => {
                    if let Some(waker) = waker {
                        match self.recv_task.poll(waker) {
                            Poll::Pending => return Poll::Pending,
                            Poll::Ready(()) => {
                                state = self.send_state.load(Ordering::Acquire);
                            }
                        }
                    } else {
                        break Poll::Pending;
                    }
                }
                LOAD => {
                    if let Some(waker) = waker {
                        match self.recv_task.poll(waker) {
                            Poll::Pending => return Poll::Pending,
                            Poll::Ready(()) => (),
                        }
                    } else if !self.recv_task.is_complete() {
                        // sender was interrupted while storing
                        thread::yield_now();
                        state = self.send_state.load(Ordering::Acquire);
                        continue;
                    }
                    if self.send_state.fetch_or(CANCEL, Ordering::AcqRel) & CANCEL == 0 {
                        break Poll::Ready(Ok(self.take()));
                    } else {
                        break Poll::Ready(Err(Canceled));
                    }
                }
                CANCEL | SENT => {
                    break Poll::Ready(Err(Canceled));
                }
                _ => {
                    panic!("Invalid state for dropshot");
                }
            }
        }
    }

    pub fn recv(&self) -> Result<T, Canceled> {
        self.recv_task.park();
        self.try_recv()
            .map(|r| r.expect("Invalid state for dropshot"))
    }

    pub fn recv_canceled(&self) -> bool {
        self.recv_task.is_complete()
    }

    pub fn try_recv(&self) -> Result<Option<T>, Canceled> {
        match self.poll_recv(None) {
            Poll::Pending => Ok(None),
            Poll::Ready(r) => r.map(Some),
        }
    }

    pub fn send(&self, value: T) -> Result<(), T> {
        loop {
            match self.send_state.compare_exchange_weak(
                INIT,
                LOAD,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    unsafe { self.data.get().write(MaybeUninit::new(value)) };
                    break if !self.recv_task.complete() {
                        // receiver already canceled
                        Err(self.take())
                    } else {
                        Ok(())
                    };
                }
                Err(INIT) => {
                    continue; // retry
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
}

pub struct Sender<T> {
    inner: Arc<Inner<T>>,
}

impl<T> Sender<T> {
    pub fn cancellation(&mut self) -> SendCancelled<'_, T> {
        SendCancelled {
            inner: &self.inner,
            cancel: Box::pin(NotifyCancel::new()),
        }
    }

    pub fn is_canceled(&self) -> bool {
        self.inner.recv_canceled()
    }

    pub fn send(self, data: T) -> Result<(), T> {
        self.inner.send(data)
    }
}

impl<T> Drop for Sender<T> {
    fn drop(&mut self) {
        self.inner.cancel_send()
    }
}

#[repr(align(4))] // the two lower bits are used to store sender state
struct NotifyCancel {
    notify: NotifyOnce,
}

impl NotifyCancel {
    const fn new() -> Self {
        Self {
            notify: NotifyOnce::new(),
        }
    }

    fn pin_notify(self: Pin<&mut Self>) -> Pin<&mut NotifyOnce> {
        unsafe { self.map_unchecked_mut(|s| &mut s.notify) }
    }
}

pub struct SendCancelled<'a, T> {
    inner: &'a Arc<Inner<T>>,
    cancel: Pin<Box<NotifyCancel>>,
}

impl<T> Future for SendCancelled<'_, T> {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        match self.cancel.as_mut().pin_notify().poll(cx) {
            Poll::Ready(()) => Poll::Ready(()),
            Poll::Pending => self.inner.poll_canceled(Some(&*self.cancel)),
        }
    }
}

impl<T> Drop for SendCancelled<'_, T> {
    fn drop(&mut self) {
        drop(self.inner.poll_canceled(None))
    }
}

pub struct Receiver<T> {
    inner: Arc<Inner<T>>,
}

impl<T> Receiver<T> {
    pub fn cancel(&mut self) -> Option<T> {
        self.inner.cancel_recv()
    }

    pub fn recv(&mut self) -> Result<T, Canceled> {
        self.inner.recv()
    }

    pub fn try_recv(&mut self) -> Result<Option<T>, Canceled> {
        self.inner.try_recv()
    }
}

impl<T> Future for Receiver<T> {
    type Output = Result<T, Canceled>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<T, Canceled>> {
        self.inner.poll_recv(Some(cx.waker()))
    }
}

impl<T> Drop for Receiver<T> {
    fn drop(&mut self) {
        self.inner.cancel_recv();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::task::{waker_ref, ArcWake};
    use std::sync::atomic::AtomicUsize;
    use std::thread;

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
    fn dropshot_send_receive_poll() {
        let (sender, mut receiver) = channel();
        let waker = Arc::new(TestWaker::new());
        let wr = waker_ref(&waker);
        let mut cx = Context::from_waker(&wr);
        assert_eq!(Pin::new(&mut receiver).poll(&mut cx), Poll::Pending);
        assert_eq!(waker.count(), 0);
        assert_eq!(sender.send(1u32), Ok(()));
        assert_eq!(waker.count(), 1);
        assert_eq!(Pin::new(&mut receiver).poll(&mut cx), Poll::Ready(Ok(1u32)));
        assert_eq!(waker.count(), 1);
        assert_eq!(
            Pin::new(&mut receiver).poll(&mut cx),
            Poll::Ready(Err(Canceled))
        );
        assert_eq!(waker.count(), 1);
    }

    #[test]
    fn dropshot_send_receive_block() {
        let (sender, mut receiver) = channel();
        assert_eq!(receiver.try_recv(), Ok(None));
        assert_eq!(sender.send(1u32), Ok(()));
        assert_eq!(receiver.recv(), Ok(1u32));
        assert_eq!(receiver.try_recv(), Err(Canceled));
    }
    #[test]
    fn dropshot_send_receive_thread() {
        let (sender0, mut receiver0) = channel();
        let (sender1, mut receiver1) = channel();
        thread::spawn(move || sender1.send(receiver0.recv().unwrap()).unwrap());
        thread::spawn(move || sender0.send(1u32).unwrap());
        assert_eq!(receiver1.recv(), Ok(1u32));
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
        assert_eq!(sender.send(1u32), Err(1u32));
    }

    #[test]
    fn dropshot_test_future() {
        use futures_executor::block_on;
        let (sender, receiver) = channel::<u32>();
        sender.send(5).unwrap();
        assert_eq!(block_on(receiver), Ok(5));
    }
}
