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
    let waker = Arc::new(TestWaker::new());
    let wr = waker_ref(&waker);
    let mut cx = Context::from_waker(&wr);
    assert_eq!(Pin::new(&mut receiver).poll(&mut cx), Poll::Pending);
    assert_eq!(waker.count(), 0);
    assert_eq!(sender.send(1u32), Ok(()));
    assert_eq!(waker.count(), 1);
    assert_eq!(receiver.is_terminated(), false);
    assert_eq!(Pin::new(&mut receiver).poll(&mut cx), Poll::Ready(Ok(1u32)));
    assert_eq!(waker.count(), 1);
    assert_eq!(receiver.is_terminated(), true);
    assert_eq!(Pin::new(&mut receiver).poll(&mut cx), Poll::Pending);
    assert_eq!(waker.count(), 1);
}

#[test]
fn channel_send_receive_block() {
    let (sender, receiver) = message_task();
    // assert_eq!(receiver.try_recv(), Ok(None));
    assert_eq!(sender.send(1u32), Ok(()));
    assert_eq!(receiver.wait(), Ok(1u32));
    // assert_eq!(receiver.try_recv(), Err(Incomplete));
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
fn channel_receiver_dropped() {
    let (sender, receiver) = message_task();
    drop(receiver);
    assert_eq!(sender.send(1u32), Err(1u32));
}

#[test]
fn channel_message_dropped() {
    let (sender, receiver) = message_task();
    let drops = Arc::new(AtomicUsize::new(0));
    TestDrop(1u32, drops.clone());
    assert_eq!(drops.load(Ordering::Acquire), 1);
    sender.send(TestDrop(2u32, drops.clone())).unwrap();
    drop(receiver);
    assert_eq!(drops.load(Ordering::Acquire), 2);
}

#[test]
fn channel_test_future() {
    let (sender, receiver) = message_task::<u32>();
    sender.send(5).unwrap();
    assert_eq!(block_on(receiver), Ok(5));
}
