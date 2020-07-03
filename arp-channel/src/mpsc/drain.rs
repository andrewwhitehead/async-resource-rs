use std::sync::Arc;
use std::thread;

use super::Queue;
use crate::thread_waker::{thread_waker, ThreadWaiter, ThreadWaker};

pub struct Drain<T> {
    queue: Arc<Queue<T>>,
    waiter: ThreadWaiter,
    waker: ThreadWaker,
}

impl<T> Drain<T> {
    pub fn new(queue: Arc<Queue<T>>) -> Self {
        let (waiter, waker) = thread_waker();
        Self {
            queue,
            waiter,
            waker,
        }
    }
}

impl<T> Iterator for Drain<T> {
    type Item = T;
    fn next(&mut self) -> Option<Self::Item> {
        loop {
            for _ in 0..5 {
                match self.queue.pop() {
                    Ok(result @ Some(..)) => return result,
                    Ok(None) => thread::yield_now(),
                    Err(_) => return None,
                }
            }
            match self.queue.pop_wake(&self.waker) {
                Ok(result @ Some(..)) => return result,
                Ok(None) => self.waiter.wait(),
                Err(_) => return None,
            }
        }
    }
}

pub struct TryDrain<T> {
    queue: Arc<Queue<T>>,
}

impl<T> TryDrain<T> {
    pub fn new(queue: Arc<Queue<T>>) -> Self {
        Self { queue }
    }
}

impl<T> Iterator for TryDrain<T> {
    type Item = T;
    fn next(&mut self) -> Option<Self::Item> {
        match self.queue.pop() {
            Ok(result @ Some(..)) => result,
            _ => None,
        }
    }
}
