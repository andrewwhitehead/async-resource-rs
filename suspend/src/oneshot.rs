use std::cell::UnsafeCell;
use std::mem::MaybeUninit;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Instant;

use super::core::InnerSuspend;
use super::error::{Incomplete, TimedOut};

pub(crate) struct Channel<T> {
    data: UnsafeCell<MaybeUninit<T>>,
    state: InnerSuspend,
}

unsafe impl<T: Send> Send for Channel<T> {}
unsafe impl<T: Send> Sync for Channel<T> {}

impl<T> Channel<T> {
    const fn new() -> Self {
        Self {
            data: UnsafeCell::new(MaybeUninit::uninit()),
            state: InnerSuspend::new_waiting(),
        }
    }

    pub fn close_recv(&self) -> bool {
        self.state.close().is_updated()
    }

    pub fn is_waiting(&self) -> bool {
        self.state.is_waiting()
    }

    pub fn poll(&self, cx: &mut Context) -> Poll<T> {
        self.state.poll(cx).map(|_| unsafe { self.take(true) })
    }

    pub fn send(&self, value: T) -> Result<(), T> {
        if self.state.is_waiting() {
            unsafe {
                self.data.get().write(MaybeUninit::new(value));
            }
            if self.state.notify() {
                Ok(())
            } else {
                // receiver was dropped while sender was pre-empted
                Err(unsafe { self.take(true) })
            }
        } else {
            Err(value)
        }
    }

    pub unsafe fn take(&self, close: bool) -> T {
        let result = self.data.get().read().assume_init();
        if close {
            self.state.close();
        }
        result
    }

    pub fn wait(&self) -> T {
        self.state.wait();
        unsafe { self.take(true) }
    }

    pub fn wait_deadline(&self, expire: Instant) -> Result<T, TimedOut> {
        if self.state.wait_deadline(expire) {
            Ok(unsafe { self.take(true) })
        } else {
            Err(TimedOut)
        }
    }
}

impl<T> Channel<Result<T, Incomplete>> {
    pub fn pair() -> (TaskSender<T>, TaskReceiver<Result<T, Incomplete>>) {
        let channel = Arc::new(Self::new());
        (
            TaskSender {
                channel: channel.clone(),
            },
            TaskReceiver { channel },
        )
    }
}

/// Created by [`message_task`](crate::task::message_task) and used to dispatch
/// a single message to a receiving [`Task`](crate::task::Task).
pub struct TaskSender<T> {
    channel: Arc<Channel<Result<T, Incomplete>>>,
}

impl<T> TaskSender<T> {
    /// Check if the receiver has already been dropped.
    pub fn is_canceled(&self) -> bool {
        !self.channel.is_waiting()
    }

    /// Dispatch the result and consume the `TaskSender`.
    pub fn send(self, value: T) -> Result<(), T> {
        let result = self.channel.send(Ok(value));
        std::mem::forget(self); // skip destructor
        result.map_err(|r| r.unwrap())
    }
}

impl<T> Drop for TaskSender<T> {
    fn drop(&mut self) {
        self.channel.send(Err(Incomplete)).unwrap_or(());
    }
}

pub(crate) struct TaskReceiver<T> {
    channel: Arc<Channel<T>>,
}

impl<T> TaskReceiver<T> {
    pub fn cancel(&mut self) -> bool {
        self.channel.close_recv();
        true
    }

    pub fn poll(&mut self, cx: &mut Context) -> Poll<T> {
        self.channel.poll(cx)
    }

    pub fn wait(&mut self) -> T {
        self.channel.wait()
    }

    pub fn wait_deadline(&mut self, expire: Instant) -> Result<T, TimedOut> {
        self.channel.wait_deadline(expire)
    }
}

impl<T> Drop for TaskReceiver<T> {
    fn drop(&mut self) {
        if self.channel.state.is_waiting() {
            self.channel.close_recv();
        } else {
            unsafe {
                self.channel.take(false);
            }
        }
    }
}
