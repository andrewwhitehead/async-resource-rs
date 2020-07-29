use std::future::Future;
use std::pin::Pin;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use std::task::{Context, Poll};
use std::thread;

use futures_core::FusedFuture;
use futures_task::{waker_ref, ArcWake};

use suspend::{block_on, message_task, Incomplete};

#[derive(Debug)]
struct TestDrop<T>(T, Arc<AtomicUsize>);

impl<T: PartialEq> PartialEq for TestDrop<T> {
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0
    }
}

impl<T: PartialEq> PartialEq<T> for TestDrop<T> {
    fn eq(&self, other: &T) -> bool {
        self.0 == *other
    }
}

impl<T> Drop for TestDrop<T> {
    fn drop(&mut self) {
        self.1.fetch_add(1, Ordering::AcqRel);
    }
}

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
fn channel_send_receive_poll() {
    let (sender, mut receiver) = message_task();
    let drops = Arc::new(AtomicUsize::new(0));
    let waker = Arc::new(TestWaker::new());
    let wr = waker_ref(&waker);
    let mut cx = Context::from_waker(&wr);
    assert_eq!(Pin::new(&mut receiver).poll(&mut cx), Poll::Pending);
    assert_eq!(waker.count(), 0);
    assert_eq!(sender.send(TestDrop(1u32, drops.clone())), Ok(()));
    assert_eq!(waker.count(), 1);
    assert_eq!(receiver.is_terminated(), false);
    assert_eq!(drops.load(Ordering::Relaxed), 0);
    if let Poll::Ready(Ok(result)) = Pin::new(&mut receiver).poll(&mut cx) {
        assert_eq!(result, 1u32);
        assert_eq!(drops.load(Ordering::Relaxed), 0);
        drop(result);
        assert_eq!(drops.load(Ordering::Relaxed), 1);
        assert_eq!(waker.count(), 1);
        assert_eq!(receiver.is_terminated(), true);
        assert_eq!(Pin::new(&mut receiver).poll(&mut cx), Poll::Pending);
        assert_eq!(waker.count(), 1);
        assert_eq!(drops.load(Ordering::Relaxed), 1);
        drop(receiver);
        assert_eq!(drops.load(Ordering::Relaxed), 1);
    } else {
        panic!("Error receiving payload")
    }
}

#[test]
fn channel_send_receive_block() {
    let (sender, receiver) = message_task();
    assert_eq!(sender.send(1u32), Ok(()));
    assert_eq!(receiver.wait(), Ok(1u32));
}

#[test]
fn channel_send_receive_thread() {
    let (sender0, receiver0) = message_task();
    let (sender1, receiver1) = message_task();
    thread::spawn(move || sender1.send(receiver0.wait().unwrap()).unwrap());
    thread::spawn(move || sender0.send(1u32).unwrap());
    assert_eq!(receiver1.wait(), Ok(1u32));
}

#[test]
fn channel_sender_dropped() {
    let (sender, mut receiver) = message_task::<u32>();
    let waker = Arc::new(TestWaker::new());
    let wr = waker_ref(&waker);
    let mut cx = Context::from_waker(&wr);
    assert_eq!(Pin::new(&mut receiver).poll(&mut cx), Poll::Pending);
    drop(sender);
    assert_eq!(waker.count(), 1);
    assert_eq!(
        Pin::new(&mut receiver).poll(&mut cx),
        Poll::Ready(Err(Incomplete))
    );
    assert_eq!(waker.count(), 1);
}

#[test]
fn channel_receiver_dropped_early() {
    let (sender, receiver) = message_task();
    drop(receiver);
    assert_eq!(sender.send(1u32), Err(1u32));
}

#[test]
fn channel_receiver_dropped_incomplete() {
    let (sender, receiver) = message_task();
    let drops = Arc::new(AtomicUsize::new(0));
    sender.send(TestDrop(1u32, drops.clone())).unwrap();
    assert_eq!(drops.load(Ordering::Relaxed), 0);
    //assert!(receiver.wait().is_ok());
    drop(receiver);
    assert_eq!(drops.load(Ordering::Relaxed), 1);
}

#[test]
fn channel_receiver_dropped_complete() {
    let (sender, receiver) = message_task();
    let drops = Arc::new(AtomicUsize::new(0));
    sender.send(TestDrop(1u32, drops.clone())).unwrap();
    let result = receiver.wait().unwrap();
    assert_eq!(result, 1u32);
    assert_eq!(drops.load(Ordering::Acquire), 0);
    drop(result);
    assert_eq!(drops.load(Ordering::Acquire), 1);
}

#[test]
fn channel_test_future() {
    let (sender, receiver) = message_task::<u32>();
    sender.send(5).unwrap();
    assert_eq!(block_on(receiver), Ok(5));
}
