use std::sync::Arc;
use std::task::Waker;
use std::thread;

use super::mpsc::Queue;
use super::waker::UnparkWaker;

use futures_util::task::waker;

pub struct Drain<T> {
    queue: Arc<Queue<T>>,
    waker: Option<DrainWaker>,
}

pub struct DrainWaker {
    generic: Waker,
    unpark: Arc<UnparkWaker>,
}

impl DrainWaker {
    pub fn new() -> Self {
        let unpark = Arc::new(UnparkWaker::new(Some(thread::current())));
        Self {
            generic: waker(unpark.clone()),
            unpark,
        }
    }

    pub fn reset(&self) {
        self.unpark.register(thread::current());
    }
}

impl<T> Drain<T> {
    pub fn new(queue: Arc<Queue<T>>, wait: bool) -> Self {
        Self {
            queue: queue,
            waker: if wait { Some(DrainWaker::new()) } else { None },
        }
    }
}

impl<T> Iterator for Drain<T> {
    type Item = T;
    fn next(&mut self) -> Option<Self::Item> {
        let waker = self.waker.as_ref().map(|w| &w.generic);
        loop {
            match self.queue.pop_wake(waker) {
                Ok(result @ Some(..)) => return result,
                Ok(None) => {
                    if waker.is_some() {
                        thread::park();
                        self.waker.as_ref().unwrap().reset();
                    } else {
                        return None;
                    }
                }
                Err(_) => return None,
            }
        }
    }
}
