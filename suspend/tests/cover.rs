use std::pin::Pin;
use std::task::{Context, Poll};
use std::thread;
use std::time::Duration;

use futures_core::stream::Stream;

use suspend::*;

fn simple_timer<'a>(duration: Duration) -> Task<'a, ()> {
    let (notifier, task) = notify_once();
    thread::spawn(move || {
        thread::sleep(duration);
        notifier.notify()
    });
    task
}

#[test]
fn block_simple() {
    assert_eq!(block_on(async { 25 }), 25);
}

#[test]
fn task_map() {
    let task = Task::from_future(async { 25 }).map(|x| x * 2);
    assert_eq!(task.wait(), 50);
}

#[test]
fn task_wait_timeout() {
    let task = simple_timer(Duration::from_millis(5));
    assert!(task.wait_timeout(Duration::from_millis(100)).is_ok());

    let task = simple_timer(Duration::from_millis(100));
    assert!(task.wait_timeout(Duration::from_millis(5)).is_err(),);
}

#[test]
fn iter_poll_next() {
    let results = vec![1, 2, 3];
    let mut rclone = results.clone();
    rclone.reverse();
    let iter = Iter::from_poll_next(move |_cx| Poll::Ready(rclone.pop()));
    assert_eq!(iter.collect::<Vec<i32>>(), results);
}

#[test]
fn iter_stream_basic() {
    struct OddStream(i32);

    impl Stream for OddStream {
        type Item = i32;

        fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
            let val = self.0;
            self.0 = val + 1;
            if val % 2 == 0 {
                // testing that the waker functions as expected
                cx.waker().clone().wake();
                Poll::Pending
            } else {
                Poll::Ready(Some(val))
            }
        }
    }

    let iter = iter_stream(OddStream(1));
    assert_eq!(iter.take(3).collect::<Vec<i32>>(), vec![1, 3, 5]);
}

#[cfg(feature = "oneshot")]
#[test]
fn sender_task_basic() {
    let (sender, task) = sender_task();
    sender.send(15).unwrap();
    assert_eq!(task.wait(), Ok(15));
}
