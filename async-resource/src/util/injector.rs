use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use concurrent_queue::{ConcurrentQueue, PopError, PushError};

use futures_util::stream::Stream;

use suspend::{Listener, Suspend};

pub fn bounded<T>(cap: usize) -> (Sender<T>, Receiver<T>) {
    pair(ConcurrentQueue::bounded(cap))
}

pub fn unbounded<T>() -> (Sender<T>, Receiver<T>) {
    pair(ConcurrentQueue::unbounded())
}

fn pair<T>(queue: ConcurrentQueue<T>) -> (Sender<T>, Receiver<T>) {
    let channel = Arc::new(Channel::new(queue));
    (
        Sender {
            channel: channel.clone(),
        },
        Receiver { channel },
    )
}

pub struct Closed;

struct Channel<T> {
    queue: ConcurrentQueue<T>,
    recv_suspend: Suspend,
}

impl<T> Channel<T> {
    fn new(queue: ConcurrentQueue<T>) -> Self {
        Self {
            queue,
            recv_suspend: Suspend::new(),
        }
    }
}

pub struct Sender<T> {
    channel: Arc<Channel<T>>,
}

impl<T> Sender<T> {
    pub fn send(&self, value: T) -> Result<(), T> {
        match self.channel.queue.push(value) {
            Ok(()) => Ok(()),
            Err(PushError::Closed(val)) => Err(val),
            Err(PushError::Full(val)) => Err(val),
        }
    }
}

impl<T> Clone for Sender<T> {
    fn clone(&self) -> Self {
        Self {
            channel: self.channel.clone(),
        }
    }
}

pub struct Receiver<T> {
    channel: Arc<Channel<T>>,
}

impl<T> Receiver<T> {
    pub fn iter(&mut self) -> IterReceiver<T> {
        IterReceiver {
            channel: self.channel.clone(),
            listener: self.channel.recv_suspend.try_listen().unwrap(),
        }
    }

    pub fn recv(&mut self) -> Result<T, Closed> {
        let listener = self.channel.recv_suspend.try_listen().unwrap();
        Err(Closed)
    }
}

pub struct IterReceiver<'a, T> {
    channel: Arc<Channel<T>>,
    listener: Listener<'a>,
}

impl<T> Iterator for IterReceiver<'_, T> {
    type Item = T;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            match self.channel.queue.pop() {
                Ok(item) => break Some(item),
                Err(PopError::Closed) => break None,
                Err(PopError::Empty) => self.listener.park(),
            }
        }
    }
}

impl<'a, T> Stream for IterReceiver<'a, T> {
    type Item = T;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        loop {
            match self.channel.queue.pop() {
                Ok(item) => return Poll::Ready(Some(item)),
                Err(PopError::Closed) => return Poll::Ready(None),
                Err(PopError::Empty) => match Pin::new(&mut self.listener).poll(cx) {
                    Poll::Pending => return Poll::Pending,
                    Poll::Ready(()) => (),
                },
            }
        }
    }
}
