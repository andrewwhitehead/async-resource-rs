use std::sync::Arc;

use super::mpsc::Queue;
use super::thread_waker::{thread_waker, ThreadWaiter, ThreadWaker};

pub struct BlockingDrain<T> {
    queue: Arc<Queue<T>>,
    waiter: ThreadWaiter,
    waker: ThreadWaker,
}

impl<T> BlockingDrain<T> {
    pub fn new(queue: Arc<Queue<T>>) -> Self {
        let (waiter, waker) = thread_waker();
        Self {
            queue,
            waiter,
            waker,
        }
    }
}

impl<T> Iterator for BlockingDrain<T> {
    type Item = T;
    fn next(&mut self) -> Option<Self::Item> {
        loop {
            match self.queue.pop_wake(&self.waker) {
                Ok(result @ Some(..)) => return result,
                Ok(None) => self.waiter.wait(),
                Err(_) => return None,
            }
        }
    }
}

pub struct Drain<T> {
    queue: Arc<Queue<T>>,
}

impl<T> Drain<T> {
    pub fn new(queue: Arc<Queue<T>>) -> Self {
        Self { queue }
    }
}

impl<T> Iterator for Drain<T> {
    type Item = T;
    fn next(&mut self) -> Option<Self::Item> {
        match self.queue.pop() {
            Ok(result @ Some(..)) => result,
            _ => None,
        }
    }
}
