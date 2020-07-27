use std::cell::UnsafeCell;
use std::mem::MaybeUninit;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Instant;

use super::core::{ClearResult, InnerSuspend};
use super::error::{Incomplete, TimedOut};
use super::task::CustomTask;

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
        match self.state.set_idle() {
            ClearResult::NoChange => false,
            _ => true,
        }
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

    pub unsafe fn take(&self, reset: bool) -> T {
        let result = self.data.get().read().assume_init();
        if reset {
            self.state.acquire();
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

/// Created by [`channel`](crate::task::channel) and used to dispatch a single
/// message to a receiving [`Task`](crate::task::Task).
pub struct TaskSender<T> {
    channel: Arc<Channel<Result<T, Incomplete>>>,
}

impl<T> TaskSender<T> {
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

impl<T: Send> CustomTask<T> for TaskReceiver<T> {
    fn cancel(&mut self) -> bool {
        self.channel.close_recv();
        true
    }

    fn poll(&mut self, cx: &mut Context) -> Poll<T> {
        self.channel.poll(cx)
    }

    fn wait(&mut self) -> T {
        self.channel.wait()
    }

    fn wait_deadline(&mut self, expire: Instant) -> Result<T, TimedOut> {
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