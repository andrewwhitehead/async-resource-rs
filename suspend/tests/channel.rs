use std::pin::Pin;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use std::task::{Context, Poll};
use std::thread;

use futures_core::{FusedStream, Stream};
use futures_lite::StreamExt;
use futures_task::{waker_ref, ArcWake};

use suspend::{block_on, channel, Incomplete};

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

// #[test]
// fn channel_send_receive_poll() {
//     let (sender, mut receiver) = channel();
//     let (message, drops) = TestDrop::new_pair();
//     let waker = Arc::new(TestWaker::new());
//     let wr = waker_ref(&waker);
//     let mut cx = Context::from_waker(&wr);
//     assert_eq!(Pin::new(&mut receiver).poll(&mut cx), Poll::Pending);
//     assert_eq!(waker.count(), 0);
//     assert_eq!(sender.send(message), Ok(()));
//     assert_eq!(waker.count(), 1);
//     assert_eq!(receiver.is_terminated(), false);
//     assert_eq!(drops.count(), 0);
//     if let Poll::Ready(Ok(result)) = Pin::new(&mut receiver).poll(&mut cx) {
//         assert_eq!(drops.count(), 0);
//         drop(result);
//         assert_eq!(drops.count(), 1);
//         assert_eq!(waker.count(), 1);
//         assert_eq!(receiver.is_terminated(), true);
//         assert_eq!(
//             Pin::new(&mut receiver).poll(&mut cx),
//             Poll::Ready(Err(Incomplete))
//         );
//         assert_eq!(waker.count(), 1);
//         assert_eq!(drops.count(), 1);
//         drop(receiver);
//         assert_eq!(drops.count(), 1);
//     } else {
//         panic!("Error receiving payload")
//     }
// }

#[test]
fn channel_dispatch_receive_multiple() {
    let (mut sender, mut receiver) = channel::<i32>();
    thread::spawn(move || {
        for idx in 0..10 {
            assert_eq!(block_on(sender.send(idx)), Ok(()));
        }
    });
    for idx in 0..10 {
        assert_eq!(receiver.wait_next(), Some(idx));
    }
}

#[test]
fn channel_send_receive_block() {
    let (sender, mut receiver) = channel();
    assert_eq!(sender.into_send(1u32), Ok(()));
    assert_eq!(receiver.wait_next(), Some(1u32));
}

#[test]
fn channel_send_receive_thread() {
    let (sender0, mut receiver0) = channel();
    let (sender1, mut receiver1) = channel();
    thread::spawn(move || sender1.into_send(receiver0.wait_next().unwrap()).unwrap());
    thread::spawn(move || sender0.into_send(1u32).unwrap());
    assert_eq!(receiver1.wait_next(), Some(1u32));
}

#[test]
fn channel_sender_dropped() {
    let (sender, mut receiver) = channel::<u32>();
    let waker = Arc::new(TestWaker::new());
    let wr = waker_ref(&waker);
    let mut cx = Context::from_waker(&wr);
    assert_eq!(Pin::new(&mut receiver).poll_next(&mut cx), Poll::Pending);
    drop(sender);
    assert_eq!(waker.count(), 1);
    assert_eq!(
        Pin::new(&mut receiver).poll_next(&mut cx),
        Poll::Ready(None)
    );
    assert_eq!(waker.count(), 1);
}

#[test]
fn channel_receiver_dropped_early() {
    let (sender, receiver) = channel();
    drop(receiver);
    assert_eq!(sender.into_send(1u32), Err(1u32));
}

#[test]
fn channel_receiver_dropped_incomplete() {
    let (sender, receiver) = channel();
    let (message, drops) = TestDrop::new_pair();
    sender.into_send(message).unwrap();
    assert_eq!(drops.count(), 0);
    //assert!(receiver.wait().is_ok());
    drop(receiver);
    assert_eq!(drops.count(), 1);
}

#[test]
fn channel_receiver_dropped_complete() {
    let (sender, mut receiver) = channel();
    let (message, drops) = TestDrop::new_pair();
    sender.into_send(message).unwrap();
    let result = receiver.wait_next().unwrap();
    assert_eq!(drops.count(), 0);
    drop(result);
    assert_eq!(drops.count(), 1);
    drop(receiver);
    assert_eq!(drops.count(), 1);
}

#[test]
fn channel_receiver_block_on() {
    let (sender, mut receiver) = channel::<u32>();
    sender.into_send(5).unwrap();
    assert_eq!(block_on(receiver.next()), Some(5));
}

#[test]
fn channel_receiver_stream_one() {
    let (sender, mut receiver) = channel::<u32>();
    sender.into_send(5).unwrap();
    assert_eq!(receiver.is_terminated(), false);
    assert_eq!(block_on(receiver.next()), Some(5));
    assert_eq!(receiver.is_terminated(), true);
    assert_eq!(block_on(receiver.next()), None);
    assert_eq!(receiver.is_terminated(), true);
}

#[test]
fn channel_receiver_stream_empty() {
    let (sender, mut receiver) = channel::<u32>();
    drop(sender);
    assert_eq!(receiver.is_terminated(), true);
    assert_eq!(block_on(receiver.next()), None);
    assert_eq!(receiver.is_terminated(), true);
}

#[test]
fn channel_receiver_cancel_early() {
    let (sender, receiver) = channel::<u32>();
    assert_eq!(receiver.cancel(), None);
    assert!(sender.into_send(5).is_err());
}

#[test]
fn channel_receiver_cancel_late() {
    let (sender, receiver) = channel::<u32>();
    sender.into_send(5).unwrap();
    assert_eq!(receiver.cancel(), Some(5));
}
