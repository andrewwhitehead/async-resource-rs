use std::sync::Arc;

use arp_channel::dropshot;
pub use dropshot::Canceled;

use super::resource::ResourceGuard;

pub struct WaitResponder<T> {
    sender: Arc<dropshot::Sender<ResourceGuard<T>>>,
}

impl<T> WaitResponder<T> {
    pub fn cancel(&self) -> bool {
        self.sender.cancel()
    }

    pub fn is_canceled(&self) -> bool {
        self.sender.is_canceled()
    }

    pub fn send(&self, data: ResourceGuard<T>) -> Result<(), ResourceGuard<T>> {
        self.sender.send(data)
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
    receiver: dropshot::Receiver<ResourceGuard<T>>,
}

impl<T> Waiter<T> {
    pub fn cancel(&mut self) -> Option<ResourceGuard<T>> {
        self.receiver.cancel()
    }

    pub fn try_recv(&mut self) -> Result<Option<ResourceGuard<T>>, Canceled> {
        self.receiver.try_recv()
    }
}
