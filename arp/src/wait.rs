use std::sync::Arc;

use arp_channel::dropshot;
pub use dropshot::Canceled;

use super::resource::ResourceResolve;

pub struct WaitResponder<T: Send + 'static, E: 'static> {
    sender: Arc<dropshot::Sender<ResourceResolve<T, E>>>,
}

impl<T: Send, E> WaitResponder<T, E> {
    pub fn cancel(&self) -> bool {
        self.sender.cancel()
    }

    pub fn is_canceled(&self) -> bool {
        self.sender.is_canceled()
    }

    pub fn send(&self, resolve: ResourceResolve<T, E>) -> Result<(), ResourceResolve<T, E>> {
        self.sender.send(resolve)
    }
}

impl<T: Send, E> Clone for WaitResponder<T, E> {
    fn clone(&self) -> Self {
        Self {
            sender: self.sender.clone(),
        }
    }
}

pub struct Waiter<T: Send + 'static, E: 'static> {
    receiver: dropshot::Receiver<ResourceResolve<T, E>>,
}

impl<T: Send, E> Waiter<T, E> {
    pub fn cancel(&mut self) -> Option<ResourceResolve<T, E>> {
        self.receiver.cancel()
    }

    pub fn try_recv(&mut self) -> Result<Option<ResourceResolve<T, E>>, Canceled> {
        self.receiver.try_recv()
    }
}
