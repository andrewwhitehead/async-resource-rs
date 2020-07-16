use std::ops::{Deref, DerefMut};
use std::sync::Arc;

use arp_channel::dropshot;
pub use dropshot::Canceled;

pub fn waiter_pair<T>() -> (WaitResponder<T>, Waiter<T>) {
    let (sender, receiver) = dropshot::channel();
    (
        WaitResponder {
            sender: Arc::new(sender),
        },
        Waiter { receiver },
    )
}

pub struct WaitResponder<T> {
    sender: Arc<dropshot::Sender<T>>,
}

impl<T> WaitResponder<T> {
    pub fn cancel(&self) -> bool {
        self.sender.cancel()
    }

    pub fn is_canceled(&self) -> bool {
        self.sender.is_canceled()
    }

    pub fn send(&self, resolve: T) -> Result<(), T> {
        self.sender.send(resolve)
    }
}

impl<T> Clone for WaitResponder<T> {
    fn clone(&self) -> Self {
        Self {
            sender: self.sender.clone(),
        }
    }
}

pub struct Waiter<T> {
    receiver: dropshot::Receiver<T>,
}

impl<T> Deref for Waiter<T> {
    type Target = dropshot::Receiver<T>;
    fn deref(&self) -> &Self::Target {
        &self.receiver
    }
}

impl<T> DerefMut for Waiter<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.receiver
    }
}
