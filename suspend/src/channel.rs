use std::cell::UnsafeCell;
use std::mem::{self, MaybeUninit};
use std::ptr::{self, NonNull};
use std::sync::atomic::{spin_loop_hint, AtomicU8, Ordering};
use std::task::{Context, Poll, Waker};
use std::thread;
use std::time::Instant;

use super::error::{Incomplete, TimedOut};
use super::thread::thread_suspend_deadline;

const STATE_DONE: u8 = 0b000;
const STATE_WAIT: u8 = 0b001;
const STATE_RECV: u8 = 0b011;
const STATE_SEND: u8 = 0b010;
const STATE_LOCKED: u8 = 0b100;
const STATE_LOCKED_WAIT: u8 = 0b101;
const STATE_LOCKED_RECV: u8 = 0b111;

// synchronize with release writes to a specific atomic variable
macro_rules! acquire {
    ($x:expr) => {
        $x.load(Ordering::Acquire);
    };
}

pub(crate) struct Channel<T> {
    data: UnsafeCell<Option<T>>,
    state: AtomicU8,
    waker: MaybeUninit<Waker>,
}

impl<T> Channel<T> {
    pub const fn new(value: Option<T>) -> Self {
        Self {
            data: UnsafeCell::new(value),
            state: AtomicU8::new(STATE_WAIT),
            waker: MaybeUninit::uninit(),
        }
    }

    // return value indicates whether the channel should be dropped
    pub fn cancel_recv(&mut self) -> bool {
        match self.state.swap(STATE_DONE, Ordering::AcqRel) {
            STATE_DONE => true,
            STATE_RECV | STATE_LOCKED_RECV => {
                unsafe { self.drop_waker() };
                false
            }
            _ => false,
        }
    }

    // return value indicates whether a result value is ready to be provided
    pub fn try_cancel_recv(&mut self) -> bool {
        match self.lock_and_clear_waiting() {
            (_, false) => {
                // the state was LOCKED or DONE, meaning the send has completed
                true
            }
            (false, true) => {
                // the state was LOCKED | (WAIT or RECV), meaning the sender
                // was pre-empted while updating the value. the receiver must
                // spin until the update is completed
                self.wait_unlock();
                true
            }
            (true, true) => {
                // successfully cancelled, check if there is a value
                unsafe { &*self.data.get() }.is_some()
            }
        }
    }

    // return value indicates whether the channel should be dropped
    #[inline]
    pub fn cancel_send(&mut self) -> bool {
        self.locked_notify(None) == (false, false)
    }

    pub fn is_waiting(&self) -> bool {
        !self.check_done()
    }

    /// Try to start receiving with a new waker
    pub fn poll(&mut self, cx: &mut Context) -> Poll<T> {
        let ready = match self.state.load(Ordering::Acquire) {
            STATE_DONE | STATE_LOCKED => {
                // not currently acquired
                true
            }
            STATE_WAIT => false,
            STATE_RECV => {
                // try to reacquire wait state
                match self.state.compare_exchange(
                    STATE_RECV,
                    STATE_WAIT,
                    Ordering::Release,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => {
                        acquire!(self.state);
                        unsafe { self.drop_waker() };
                        false
                    }
                    Err(STATE_DONE) | Err(STATE_LOCKED) => {
                        // already notified, or being notified
                        acquire!(self.state);
                        true
                    }
                    Err(STATE_WAIT) => {
                        // competition from another thread?
                        // should not happen with wrappers around this instance
                        panic!("Invalid state");
                    }
                    Err(_) => panic!("Invalid state"),
                }
            }
            STATE_LOCKED_WAIT | STATE_LOCKED_RECV => {
                // about to be notified. call the new waker to poll again immediately
                cx.waker().wake_by_ref();
                return Poll::Pending;
            }
            _ => panic!("Invalid state"),
        };

        if !ready {
            // must be in the WAIT state at this point, or LOCKED_WAIT if pre-empted
            // by lock()
            unsafe {
                self.store_waker(cx.waker().clone());
            }

            match self.state.compare_exchange(
                STATE_WAIT,
                STATE_RECV,
                Ordering::Release,
                Ordering::Relaxed,
            ) {
                Ok(_) => {
                    // registered listener
                    return Poll::Pending;
                }
                Err(STATE_DONE) => {
                    // notify() was called while storing
                    acquire!(self.state);
                    unsafe { self.drop_waker() };
                }
                Err(STATE_LOCKED_WAIT) => {
                    // lock() was called while storing. call new the waker now,
                    // so the listener will poll again for the result
                    acquire!(self.state);
                    unsafe { self.wake_waker() };
                    return Poll::Pending;
                }
                _ => panic!("Invalid state"),
            }
        }

        if let Some(result) = unsafe { self.take() } {
            Poll::Ready(result)
        } else {
            // keeps returning Pending if the result was already taken
            Poll::Pending
        }
    }

    #[inline]
    pub fn send_once(&mut self, value: T) -> Result<(), (bool, T)> {
        match self.lock() {
            (true, true) => {
                // acquired lock, receiver still waiting
                unsafe {
                    self.data.get().replace(Some(value));
                }
                match self.locked_notify(None) {
                    (true, _) => {
                        // state was LOCKED | (WAIT or RECV): receiver has not cancelled or dropped.
                        // state was LOCKED: the receiver tried to cancel and saw the sender
                        // was in the middle of writing the value, so it would spin until notified
                        Ok(())
                    }
                    (false, false) => {
                        // state was DONE, receiver has been dropped
                        Err((true, unsafe { self.take().unwrap() }))
                    }
                    (false, true) => panic!("Invalid state"),
                }
            }
            (true, false) => {
                // state was DONE, receiver has been dropped
                Err((true, value))
            }
            (false, false) => {
                // state was LOCKED: the receiver has cancelled but not dropped.
                // if unlock succeeds, then the receiver is responsible for dropping
                // the channel, otherwise it has been dropped in the interim
                Err((!self.unlock(), value))
            }
            (false, true) => panic!("Invalid state"),
        }
    }

    pub fn try_recv(&mut self) -> Poll<T> {
        if self.check_done() {
            if let Some(result) = unsafe { self.take() } {
                return Poll::Ready(result);
            }
        }
        Poll::Pending
    }

    pub fn wait_recv(&mut self) -> T {
        if let Poll::Ready(result) = self.wait_notify(None) {
            result
        } else {
            unreachable!()
        }
    }

    pub fn wait_recv_deadline(&mut self, expire: Instant) -> Result<T, TimedOut> {
        match self.wait_notify(Some(expire)) {
            Poll::Ready(result) => Ok(result),
            Poll::Pending => {
                if self.cancel_recv() {
                    Err(TimedOut)
                } else {
                    Ok(unsafe { self.take() }.unwrap())
                }
            }
        }
    }

    #[inline]
    fn check_done(&self) -> bool {
        self.state.load(Ordering::Acquire) & STATE_RECV == STATE_DONE
    }

    /// Clear the waiting state and set the LOCKED bit. This is used to
    /// indicate when the listener is cancelling a notification, as in the
    /// one-shot Task. Returns a pair of (acquired, cleared) where acquired
    /// indicates that the LOCKED bit was not previously set, and cleared
    /// indicates that the state was either WAIT or RECV.
    #[inline]
    fn lock_and_clear_waiting(&mut self) -> (bool, bool) {
        let prev = self.state.swap(STATE_LOCKED, Ordering::AcqRel);
        let cleared = match prev & STATE_RECV {
            STATE_DONE => false,
            STATE_WAIT => true,
            STATE_RECV => {
                unsafe { self.drop_waker() };
                true
            }
            _ => panic!("Invalid state"),
        };
        (prev & STATE_LOCKED == 0, cleared)
    }

    #[inline]
    fn lock(&mut self) -> (bool, bool) {
        let prev = self.state.fetch_or(STATE_LOCKED, Ordering::AcqRel);
        (prev & STATE_LOCKED == 0, prev & STATE_RECV != 0)
    }

    /// Clear the state if it is waiting, and call the notifier if any.
    /// Returns a pair of (held, notified) where held indicates that the
    /// lock was still held when the notify occurred.
    fn locked_notify(&mut self, waker: Option<&Waker>) -> (bool, bool) {
        let state = if let Some(waker) = waker {
            unsafe {
                self.store_waker(waker.clone());
            }
            STATE_SEND
        } else {
            STATE_DONE
        };
        let prev = self.state.swap(state, Ordering::AcqRel);
        (
            prev & STATE_LOCKED == STATE_LOCKED,
            match prev & STATE_RECV {
                STATE_DONE => false,
                STATE_WAIT => true,
                STATE_RECV => {
                    unsafe { self.wake_waker() };
                    true
                }
                _ => panic!("Invalid state"),
            },
        )
    }

    #[inline]
    fn unlock(&mut self) -> bool {
        self.state.fetch_and(!STATE_LOCKED, Ordering::AcqRel) & STATE_LOCKED == STATE_LOCKED
    }

    #[inline]
    fn wait_unlock(&self) {
        let mut i = 1;
        loop {
            if self.state.load(Ordering::Acquire) & STATE_LOCKED == 0 {
                return;
            }
            if i % 100 == 0 {
                thread::yield_now();
            } else {
                spin_loop_hint();
            }
            i += 1;
        }
    }

    #[inline]
    fn wait_notify(&mut self, expire: Option<Instant>) -> Poll<T> {
        let mut first = true;
        thread_suspend_deadline(
            |cx| {
                if first {
                    first = false;
                    self.poll(cx)
                } else {
                    // no need to update the waker after the first poll
                    if self.check_done() {
                        Poll::Ready(unsafe { self.take() }.unwrap())
                    } else {
                        Poll::Pending
                    }
                }
            },
            expire,
        )
    }

    #[inline]
    unsafe fn take(&mut self) -> Option<T> {
        self.data.get().replace(None)
    }

    #[inline]
    unsafe fn drop_waker(&mut self) {
        ptr::drop_in_place(self.waker.as_mut_ptr());
    }

    #[inline]
    unsafe fn store_waker(&mut self, waker: Waker) {
        self.waker.as_mut_ptr().write(waker)
    }

    #[inline]
    unsafe fn wake_waker(&self) {
        ptr::read(&self.waker).assume_init().wake();
    }
}

impl<T> Channel<Result<T, Incomplete>> {
    pub fn pair() -> (TaskSender<T>, TaskReceiver<Result<T, Incomplete>>) {
        let channel = unsafe {
            NonNull::new_unchecked(Box::into_raw(Box::new(Self::new(Some(Err(Incomplete))))))
        };
        (TaskSender { channel }, TaskReceiver { channel })
    }
}

/// Created by [`message_task`](crate::task::message_task) and used to dispatch
/// a single message to a receiving [`Task`](crate::task::Task).
pub struct TaskSender<T> {
    channel: NonNull<Channel<Result<T, Incomplete>>>,
}

unsafe impl<T: Send> Send for TaskSender<T> {}
unsafe impl<T: Send> Sync for TaskSender<T> {}

impl<T> TaskSender<T> {
    /// Check if the receiver has already been dropped.
    pub fn is_canceled(&self) -> bool {
        unsafe { self.channel.as_ref() }.is_waiting()
    }

    /// Dispatch the result and consume the `TaskSender`.
    pub fn send(self, value: T) -> Result<(), T> {
        let mut channel = mem::ManuallyDrop::new(self).channel;
        unsafe { channel.as_mut() }
            .send_once(Ok(value))
            .map_err(|(drop_channel, err)| {
                if drop_channel {
                    drop(unsafe { Box::from_raw(channel.as_ptr()) });
                }
                err.unwrap()
            })
    }
}

impl<T> Drop for TaskSender<T> {
    fn drop(&mut self) {
        if unsafe { self.channel.as_mut() }.cancel_send() {
            drop(unsafe { Box::from_raw(self.channel.as_ptr()) });
        }
    }
}

pub(crate) struct TaskReceiver<T> {
    channel: NonNull<Channel<T>>,
}

unsafe impl<T: Send> Send for TaskReceiver<T> {}
unsafe impl<T: Send> Sync for TaskReceiver<T> {}

impl<T> TaskReceiver<T> {
    pub fn poll(&mut self, cx: &mut Context) -> Poll<T> {
        unsafe { self.channel.as_mut() }.poll(cx)
    }

    pub fn try_cancel(&mut self) -> bool {
        unsafe { self.channel.as_mut() }.try_cancel_recv()
    }

    pub fn try_recv(&mut self) -> Poll<T> {
        unsafe { self.channel.as_mut() }.try_recv()
    }

    pub fn wait(self) -> T {
        // receiver is always the last surviving in this case, so it drops the channel
        let mut channel = unsafe { Box::from_raw(mem::ManuallyDrop::new(self).channel.as_ptr()) };
        channel.wait_recv()
    }

    pub fn wait_deadline(mut self, expire: Instant) -> Result<T, Self> {
        match unsafe { self.channel.as_mut() }.wait_recv_deadline(expire) {
            Ok(result) => {
                let channel = mem::ManuallyDrop::new(self).channel;
                drop(unsafe { Box::from_raw(channel.as_ptr()) });
                Ok(result)
            }
            Err(_) => Err(self),
        }
    }
}

impl<T> Drop for TaskReceiver<T> {
    fn drop(&mut self) {
        if unsafe { self.channel.as_mut() }.cancel_recv() {
            drop(unsafe { Box::from_raw(self.channel.as_ptr()) });
        }
    }
}
