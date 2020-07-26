use std::fmt::{self, Debug, Formatter};
use std::ops::{Deref, DerefMut};
use std::sync::Arc;

use option_lock::OptionLock;

pub use suspend::Incomplete as Canceled;

pub fn waiter_pair<T: Send + 'static>() -> (WaitResponder<T>, Waiter<T>) {
    let (sender, receiver) = suspend::channel();
    (
        WaitResponder {
            sender: Arc::new(OptionLock::from(sender)),
        },
        Waiter { receiver },
    )
}

pub struct WaitResponder<T> {
    sender: Arc<OptionLock<suspend::TaskSender<T>>>,
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
            sender.send(resolve)
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
    receiver: suspend::Task<'static, Result<T, Canceled>>,
}

impl<T> Debug for Waiter<T> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("Waiter").finish()
    }
}

impl<T> Deref for Waiter<T> {
    type Target = suspend::Task<'static, Result<T, Canceled>>;
    fn deref(&self) -> &Self::Target {
        &self.receiver
    }
}

impl<T> DerefMut for Waiter<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.receiver
    }
}
