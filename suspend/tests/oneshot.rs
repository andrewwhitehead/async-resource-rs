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

mod utils;
use utils::TestDrop;

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
    let (message, drops) = TestDrop::new_pair();
    let waker = Arc::new(TestWaker::new());
    let wr = waker_ref(&waker);
    let mut cx = Context::from_waker(&wr);
    assert_eq!(Pin::new(&mut receiver).poll(&mut cx), Poll::Pending);
    assert_eq!(waker.count(), 0);
    assert_eq!(sender.send(message), Ok(()));
    assert_eq!(waker.count(), 1);
    assert_eq!(receiver.is_terminated(), false);
    assert_eq!(drops.count(), 0);
    if let Poll::Ready(Ok(result)) = Pin::new(&mut receiver).poll(&mut cx) {
        assert_eq!(drops.count(), 0);
        drop(result);
        assert_eq!(drops.count(), 1);
        assert_eq!(waker.count(), 1);
        assert_eq!(receiver.is_terminated(), true);
        assert_eq!(Pin::new(&mut receiver).poll(&mut cx), Poll::Pending);
        assert_eq!(waker.count(), 1);
        assert_eq!(drops.count(), 1);
        drop(receiver);
        assert_eq!(drops.count(), 1);
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
    let (message, drops) = TestDrop::new_pair();
    sender.send(message).unwrap();
    assert_eq!(drops.count(), 0);
    //assert!(receiver.wait().is_ok());
    drop(receiver);
    assert_eq!(drops.count(), 1);
}

#[test]
fn channel_receiver_dropped_complete() {
    let (sender, receiver) = message_task();
    let (message, drops) = TestDrop::new_pair();
    sender.send(message).unwrap();
    let result = receiver.wait().unwrap();
    assert_eq!(drops.count(), 0);
    drop(result);
    assert_eq!(drops.count(), 1);
}

#[test]
fn channel_test_future() {
    let (sender, receiver) = message_task::<u32>();
    sender.send(5).unwrap();
    assert_eq!(block_on(receiver), Ok(5));
}

#[test]
fn channel_test_cancel_early() {
    let (sender, mut receiver) = message_task::<u32>();
    assert!(receiver.try_cancel());
    assert_eq!(receiver.wait(), Err(Incomplete));
    assert!(sender.send(5).is_err());
}
