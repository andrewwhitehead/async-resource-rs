use std::pin::Pin;
use std::task::{Context, Poll};

use futures_core::stream::Stream;

use suspend::*;

// fn simple_timer(duration: Duration) -> ListenOnce {
//     let (notifier, task) = notify_once();
//     thread::spawn(move || {
//         thread::sleep(duration);
//         notifier.notify()
//     });
//     task
// }

#[test]
fn block_simple() {
    assert_eq!(block_on(async { 25 }), 25);
}

#[test]
fn iter_poll_next() {
    let results = vec![1, 2, 3];
    let mut rclone = results.clone();
    rclone.reverse();
    let iter = iter_poll_fn(move |_cx| Poll::Ready(rclone.pop()));
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
                cx.waker().wake_by_ref();
                Poll::Pending
            } else {
                Poll::Ready(Some(val))
            }
        }
    }

    let iter = iter_stream(OddStream(1));
    assert_eq!(iter.take(3).collect::<Vec<i32>>(), vec![1, 3, 5]);
}
