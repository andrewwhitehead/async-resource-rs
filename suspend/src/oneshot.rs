use std::cell::UnsafeCell;
use std::mem;
use std::task::{Context, Poll};
use std::time::Instant;

use super::core::InnerSuspend;
use super::error::{Incomplete, TimedOut};

pub(crate) struct Channel<T> {
    data: UnsafeCell<Option<T>>,
    state: InnerSuspend,
}

impl<T> Channel<T> {
    const fn new(value: Option<T>) -> Self {
        Self {
            data: UnsafeCell::new(value),
            state: InnerSuspend::new_waiting(),
        }
    }

    // return value indicates whether the channel should be dropped
    pub fn cancel_recv(&self) -> bool {
        !self.state.unlock_and_clear_waiting()
    }

    // return value indicates whether a result value is ready to be provided
    pub fn try_cancel_recv(&self) -> bool {
        match self.state.lock_and_clear_waiting() {
            (_, false) => {
                // the state was LOCKED or IDLE, meaning the send has completed
                true
            }
            (false, true) => {
                // the state was LOCKED | (WAIT or LISTEN), meaning the sender
                // was pre-empted while updating the value. the receiver must
                // spin until the update is completed
                self.state.wait_unlock();
                true
            }
            (true, true) => {
                // successfully cancelled, check if there is a value
                unsafe { &*self.data.get() }.is_some()
            }
        }
    }

    // return value indicates whether the channel should be dropped
    pub fn cancel_send(&self) -> bool {
        self.state.locked_notify() == (false, false)
    }

    pub fn is_waiting(&self) -> bool {
        self.state.is_waiting()
    }

    pub fn poll(&self, cx: &mut Context) -> Poll<T> {
        if let Poll::Ready(()) = self.state.poll(cx) {
            if let Some(result) = unsafe { self.take() } {
                return Poll::Ready(result);
            }
        }
        // keeps returning Pending if the result was already taken
        Poll::Pending
    }

    pub fn send(&self, value: T) -> Result<(), (bool, T)> {
        match self.state.lock() {
            (true, true) => {
                // acquired lock, receiver still waiting
                unsafe {
                    self.data.get().replace(Some(value));
                }
                match self.state.locked_notify() {
                    (true, _) => {
                        // state was LOCKED | (WAIT or LISTEN): receiver has not cancelled or dropped.
                        // state was LOCKED | IDLE: the receiver tried to cancel and saw the sender
                        // was in the middle of writing the value, so it would spin until notified
                        Ok(())
                    }
                    (false, false) => {
                        // state was IDLE, receiver has been dropped
                        Err((true, unsafe { self.take().unwrap() }))
                    }
                    (false, true) => panic!("Invalid state"),
                }
            }
            (true, false) => {
                // state was IDLE, receiver has been dropped
                Err((true, value))
            }
            (false, false) => {
                // state was LOCKED | IDLE: the receiver has cancelled but not dropped.
                // if unlock succeeds, then the receiver is responsible for dropping
                // the channel, otherwise it has been dropped in the interim
                Err((!self.state.unlock(), value))
            }
            (false, true) => panic!("Invalid state"),
        }
    }

    pub unsafe fn take(&self) -> Option<T> {
        self.data.get().replace(None)
    }

    pub fn try_recv(&self) -> Poll<T> {
        if !self.is_waiting() {
            if let Some(result) = unsafe { self.take() } {
                return Poll::Ready(result);
            }
        }
        Poll::Pending
    }

    pub fn wait(&self) -> T {
        self.state.wait();
        // must have been notified here
        unsafe { self.take() }.unwrap()
    }

    pub fn wait_deadline(&self, expire: Instant) -> Result<T, TimedOut> {
        if self.state.wait_deadline(Some(expire)) {
            Ok(unsafe { self.take() }.unwrap())
        } else {
            if self.cancel_recv() {
                Err(TimedOut)
            } else {
                Ok(unsafe { self.take() }.unwrap())
            }
        }
    }
}

impl<T> Channel<Result<T, Incomplete>> {
    pub fn pair() -> (TaskSender<T>, TaskReceiver<Result<T, Incomplete>>) {
        let channel = Box::into_raw(Box::new(Self::new(Some(Err(Incomplete)))));
        (TaskSender { channel }, TaskReceiver { channel })
    }
}

/// Created by [`message_task`](crate::task::message_task) and used to dispatch
/// a single message to a receiving [`Task`](crate::task::Task).
pub struct TaskSender<T> {
    channel: *mut Channel<Result<T, Incomplete>>,
}

unsafe impl<T: Send> Send for TaskSender<T> {}
unsafe impl<T: Send> Sync for TaskSender<T> {}

impl<T> TaskSender<T> {
    /// Check if the receiver has already been dropped.
    pub fn is_canceled(&self) -> bool {
        !unsafe { &*self.channel }.is_waiting()
    }

    /// Dispatch the result and consume the `TaskSender`.
    pub fn send(self, value: T) -> Result<(), T> {
        let channel = mem::ManuallyDrop::new(self).channel;
        unsafe { &*channel }
            .send(Ok(value))
            .map_err(|(drop_channel, err)| {
                if drop_channel {
                    drop(unsafe { Box::from_raw(channel) });
                }
                err.unwrap()
            })
    }
}

impl<T> Drop for TaskSender<T> {
    fn drop(&mut self) {
        if unsafe { &*self.channel }.cancel_send() {
            drop(unsafe { Box::from_raw(self.channel) });
        }
    }
}

pub(crate) struct TaskReceiver<T> {
    channel: *mut Channel<T>,
}

unsafe impl<T: Send> Send for TaskReceiver<T> {}
unsafe impl<T: Send> Sync for TaskReceiver<T> {}

impl<T> TaskReceiver<T> {
    pub fn cancel(&mut self) -> bool {
        unsafe { &*self.channel }.try_cancel_recv()
    }

    pub fn poll(&mut self, cx: &mut Context) -> Poll<T> {
        unsafe { &*self.channel }.poll(cx)
    }

    pub fn try_recv(&mut self) -> Poll<T> {
        unsafe { &*self.channel }.try_recv()
    }

    pub fn wait(self) -> T {
        // receiver is always the last surviving in this case, so it drops the channel
        let channel = unsafe { Box::from_raw(mem::ManuallyDrop::new(self).channel) };
        channel.wait()
    }

    pub fn wait_deadline(self, expire: Instant) -> Result<T, Self> {
        match unsafe { &*self.channel }.wait_deadline(expire) {
            Ok(result) => {
                let channel = mem::ManuallyDrop::new(self).channel;
                drop(unsafe { Box::from_raw(channel) });
                Ok(result)
            }
            Err(_) => Err(self),
        }
    }
}

impl<T> Drop for TaskReceiver<T> {
    fn drop(&mut self) {
        if unsafe { &*self.channel }.cancel_recv() {
            drop(unsafe { Box::from_raw(self.channel) });
        }
    }
}
