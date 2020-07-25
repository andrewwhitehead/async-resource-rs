use std::fmt::{self, Debug, Formatter};
use std::ops::{Deref, DerefMut};
use std::sync::Arc;

use oneshot;
use option_lock::OptionLock;

pub use self::oneshot::RecvError as Canceled;

pub fn waiter_pair<T>() -> (WaitResponder<T>, Waiter<T>) {
    let (sender, receiver) = oneshot::channel();
    (
        WaitResponder {
            sender: Arc::new(OptionLock::from(sender)),
        },
        Waiter { receiver },
    )
}

pub struct WaitResponder<T> {
    sender: Arc<OptionLock<oneshot::Sender<T>>>,
}

impl<T> WaitResponder<T> {
    pub fn cancel(self) {
        if let Ok(sender) = self.sender.try_take() {
            drop(sender);
        }
    }

    pub fn is_canceled(&self) -> bool {
        !self.sender.status().can_take()
    }

    pub fn send(self, resolve: T) -> Result<(), T> {
        if let Ok(sender) = self.sender.try_take() {
            sender.send(resolve).map_err(|e| e.into_inner())
        } else {
            Err(resolve)
        }
    }
}

impl<T> Clone for WaitResponder<T> {
    fn clone(&self) -> Self {
        Self {
            sender: self.sender.clone(),
        }
    }
}

impl<T> Debug for WaitResponder<T> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("WaitResponder")
            .field("is_canceled", &self.is_canceled())
            .finish()
    }
}

pub struct Waiter<T> {
    receiver: oneshot::Receiver<T>,
}

impl<T> Debug for Waiter<T> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("Waiter").finish()
    }
}

impl<T> Deref for Waiter<T> {
    type Target = oneshot::Receiver<T>;
    fn deref(&self) -> &Self::Target {
        &self.receiver
    }
}

impl<T> DerefMut for Waiter<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.receiver
    }
}
