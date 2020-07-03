use std::sync::Arc;
use std::thread;

use super::mpsc::Queue;
use super::waker::UnparkWaker;

pub struct Drain<T> {
    queue: Arc<Queue<T>>,
    waker: Option<UnparkWaker>,
}

impl<T> Drain<T> {
    pub fn new(queue: Arc<Queue<T>>, wait: bool) -> Self {
        Self {
            queue: queue,
            waker: if wait { Some(UnparkWaker::new()) } else { None },
        }
    }
}

impl<T> Iterator for Drain<T> {
    type Item = T;
    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let waker = self.waker.as_ref().map(|w| w.waker());
            match self.queue.pop_wake(waker) {
                Ok(result @ Some(..)) => return result,
                Ok(None) => {
                    if waker.is_some() {
                        thread::park();
                    } else {
                        return None;
                    }
                }
                Err(_) => return None,
            }
        }
    }
}
