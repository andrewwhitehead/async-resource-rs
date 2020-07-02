use std::cell::UnsafeCell;
use std::iter::IntoIterator;
use std::marker::PhantomData;
use std::mem::MaybeUninit;
use std::pin::Pin;
use std::ptr;
use std::sync::{
    atomic::{AtomicBool, AtomicPtr, AtomicUsize, Ordering},
    Arc,
};
use std::task::{Context, Poll, Waker};
use std::thread;

#[cfg(feature = "sink")]
use futures_util::sink::Sink;
use futures_util::stream::Stream;

use super::drain::Drain;
use super::OptionLock;

// Each block covers one "lap" of indices.
const LAP: usize = 32;
// The maximum number of items a block can hold.
const BLOCK_CAP: usize = LAP - 1;
// How many lower bits are reserved for metadata.
const SHIFT: usize = 1;
// Has two different purposes:
// * If set in head, indicates that the block is not the last one.
// * If set in tail, indicates that the queue is closed.
const MARK_BIT: usize = 1;

/// A slot in a block.
struct Slot<T> {
    /// The value.
    value: UnsafeCell<MaybeUninit<T>>,

    /// The state of the slot.
    written: AtomicBool,
}

impl<T> Slot<T> {
    const UNINIT: Slot<T> = Slot {
        value: UnsafeCell::new(MaybeUninit::uninit()),
        written: AtomicBool::new(false),
    };
}

/// A block in a linked list.
///
/// Each block in the list can hold up to `BLOCK_CAP` values.
struct Block<T> {
    /// The next block in the linked list.
    next: AtomicPtr<Block<T>>,

    /// Slots for values.
    slots: [Slot<T>; BLOCK_CAP],
}

impl<T> Block<T> {
    /// Creates an empty block.
    fn new() -> Block<T> {
        Block {
            next: AtomicPtr::new(ptr::null_mut()),
            slots: [Slot::UNINIT; BLOCK_CAP],
        }
    }
}

struct ReadPosition<T> {
    /// The index in the queue.
    index: usize,

    /// The block in the linked list.
    block: *mut Block<T>,
}

impl<T> Clone for ReadPosition<T> {
    fn clone(&self) -> Self {
        Self {
            index: self.index,
            block: self.block,
        }
    }
}

impl<T> Copy for ReadPosition<T> {}

/// A position in a queue.
struct WritePosition<T> {
    /// The block in the linked list.
    block: AtomicPtr<Block<T>>,

    /// The index in the queue.
    index: AtomicUsize,
}

pub struct Sender<T> {
    closed: bool,
    queue: Arc<Queue<T>>,
}

impl<T> Sender<T> {
    pub(crate) fn new(queue: Arc<Queue<T>>) -> Self {
        Self {
            closed: false,
            queue,
        }
    }

    pub fn close(&mut self) {
        if !self.closed {
            self.closed = true;
            self.queue.dec_senders();
        }
    }

    pub fn send(&self, value: T) -> Result<(), T> {
        if let Some(result) = self.queue.push(value) {
            Err(result)
        } else {
            Ok(())
        }
    }
}

impl<T> Clone for Sender<T> {
    fn clone(&self) -> Self {
        self.queue.inc_senders();
        Self::new(self.queue.clone())
    }
}

unsafe impl<T> Send for Sender<T> {}

#[cfg(feature = "sink")]
impl<T> Sink<T> for Sender<T> {
    type Error = T;

    fn poll_ready(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn start_send(self: Pin<&mut Self>, msg: T) -> Result<(), Self::Error> {
        self.send(msg)
    }

    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn poll_close(mut self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.close();
        Poll::Ready(Ok(()))
    }
}

impl<T> Drop for Sender<T> {
    fn drop(&mut self) {
        self.close()
    }
}

pub struct Receiver<T> {
    queue: Arc<Queue<T>>,
}

impl<T> Receiver<T> {
    pub(crate) fn new(queue: Arc<Queue<T>>) -> Self {
        Self { queue }
    }

    pub fn close(&self) -> bool {
        self.queue.close_rx()
    }

    pub fn iter(&mut self) -> Drain<T> {
        Drain::new(self.queue.clone(), true)
    }

    pub fn try_iter(&mut self) -> Drain<T> {
        Drain::new(self.queue.clone(), false)
    }

    pub fn try_recv(&self) -> Option<T> {
        self.queue.pop()
    }
}

impl<T> IntoIterator for Receiver<T> {
    type Item = T;
    type IntoIter = Drain<T>;

    fn into_iter(mut self) -> Self::IntoIter {
        self.iter()
    }
}

impl<T> Drop for Receiver<T> {
    fn drop(&mut self) {
        self.queue.close_rx();
    }
}

impl<T> Stream for Receiver<T> {
    type Item = T;
    fn poll_next(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Option<Self::Item>> {
        match self.queue.pop_wake(Some(cx.waker())) {
            Ok(result @ Some(..)) => Poll::Ready(result),
            Ok(None) => Poll::Pending,
            Err(_) => Poll::Ready(None),
        }
    }
}

pub fn unbounded<T>() -> (Sender<T>, Receiver<T>) {
    let queue = Arc::new(Queue::new());
    (Sender::new(queue.clone()), Receiver::new(queue))
}

pub struct Queue<T> {
    read: UnsafeCell<ReadPosition<T>>,

    write: WritePosition<T>,

    senders: AtomicUsize,

    waker: OptionLock<Waker>,

    /// Indicates that dropping a `Unbounded<T>` may drop values of type `T`.
    _marker: PhantomData<T>,
}

impl<T> Queue<T> {
    pub(crate) fn new() -> Self {
        let init = Box::into_raw(Box::new(Block::<T>::new()));
        Self {
            read: UnsafeCell::new(ReadPosition {
                block: init,
                index: 0,
            }),
            write: WritePosition {
                block: AtomicPtr::new(init),
                index: AtomicUsize::new(0),
            },
            senders: AtomicUsize::new(1),
            waker: OptionLock::new(None),
            _marker: PhantomData,
        }
    }

    pub fn push(&self, value: T) -> Option<T> {
        let mut tail = self.write.index.load(Ordering::Acquire);
        let mut block = self.write.block.load(Ordering::Acquire);
        let mut next_block = None;

        loop {
            // Check if receiver has been dropped
            if tail & MARK_BIT != 0 {
                return Some(value);
            }

            // Calculate the offset of the index into the block.
            let offset = (tail >> SHIFT) % LAP;

            // If we reached the end of the block, wait until the next one is installed.
            if offset == BLOCK_CAP {
                thread::yield_now();
                tail = self.write.index.load(Ordering::Acquire);
                block = self.write.block.load(Ordering::Acquire);
                continue;
            }

            // If we're going to have to install the next block, allocate it in advance in order to
            // make the wait for other threads as short as possible.
            if offset + 1 == BLOCK_CAP && next_block.is_none() {
                next_block.replace(Box::new(Block::<T>::new()));
            }

            let new_tail = tail + (1 << SHIFT);

            // Try advancing the tail forward.
            match self.write.index.compare_exchange_weak(
                tail,
                new_tail,
                Ordering::SeqCst,
                Ordering::Acquire,
            ) {
                Ok(_) => unsafe {
                    // If we've reached the end of the block, install the next one.
                    if offset + 1 == BLOCK_CAP {
                        let next_block = Box::into_raw(next_block.unwrap());
                        self.write.block.store(next_block, Ordering::Release);
                        self.write.index.fetch_add(1 << SHIFT, Ordering::Release);
                        (*block).next.store(next_block, Ordering::Release);
                    }

                    // Write the value into the slot.
                    let slot = (*block).slots.get_unchecked(offset);
                    slot.value.get().write(MaybeUninit::new(value));
                    slot.written.store(true, Ordering::Release);

                    // Alert the receiver if waiting
                    self.wake_rx();

                    return None;
                },
                Err(t) => {
                    tail = t;
                    block = self.write.block.load(Ordering::Acquire);
                }
            }
        }
    }

    // not thread safe
    pub fn pop(&self) -> Option<T> {
        unsafe {
            let mut next = *self.read.get();
            let offset = (next.index >> SHIFT) % LAP;

            let slot = (*next.block).slots.get_unchecked(offset);
            if !slot.written.load(Ordering::Acquire) {
                return None;
            }
            let value = slot.value.get().read().assume_init();

            if offset == BLOCK_CAP - 1 {
                let destroy = next.block;
                next.block = (*next.block).next.load(Ordering::Acquire);
                drop(Box::from_raw(destroy));
                next.index += 1 << SHIFT;
            }

            next.index += 1 << SHIFT;
            *self.read.get() = next;

            Some(value)
        }
    }

    pub fn pop_wake(&self, waker: Option<&Waker>) -> Result<Option<T>, bool> {
        loop {
            match self.pop() {
                found @ Some(_) => return Ok(found),
                None => {
                    if let Some(waker) = waker {
                        if let Some(mut guard) = self.waker.try_lock() {
                            guard.replace(waker.clone());
                        } else {
                            // a sender is trying to wake us already
                            continue;
                        }
                    }
                    if !self.is_empty() {
                        // we are slow to register the waker, so this checks if
                        // results are pending (a write was interrupted) or
                        // a write was started after pop()
                        self.wake_rx();
                    } else if self.tx_closed() {
                        return Err(false);
                    }
                    return Ok(None);
                }
            }
        }
    }

    /// Closes the queue.
    ///
    /// Returns `true` if this call closed the queue.
    pub fn close_rx(&self) -> bool {
        let tail = self.write.index.fetch_or(MARK_BIT, Ordering::SeqCst);

        if tail & MARK_BIT == 0 {
            true
        } else {
            false
        }
    }

    /// Returns `true` if the queue is closed.
    pub fn rx_closed(&self) -> bool {
        self.write.index.load(Ordering::SeqCst) & MARK_BIT != 0
    }

    pub fn inc_senders(&self) {
        self.senders.fetch_add(1, Ordering::SeqCst);
    }

    pub fn dec_senders(&self) {
        if self.senders.fetch_sub(1, Ordering::SeqCst) == 1 {
            self.wake_rx();
        }
    }

    pub fn tx_closed(&self) -> bool {
        self.senders.load(Ordering::Acquire) == 0
    }

    // not thread safe
    pub fn is_empty(&self) -> bool {
        let head = unsafe { (*self.read.get()).index };
        let tail = self.write.index.load(Ordering::SeqCst);
        head >> SHIFT == tail >> SHIFT
    }

    fn wake_rx(&self) {
        if let Some(waker) = self.waker.try_take() {
            waker.wake();
        }
    }
}

impl<T> Drop for Queue<T> {
    fn drop(&mut self) {
        let ReadPosition {
            mut index,
            mut block,
        } = unsafe { *self.read.get() };
        let mut tail = self.write.index.load(Ordering::Relaxed);

        // Erase the lower bits.
        index &= !((1 << SHIFT) - 1);
        tail &= !((1 << SHIFT) - 1);

        unsafe {
            // Drop all values between `head` and `tail` and deallocate the heap-allocated blocks.
            while index != tail {
                let offset = (index >> SHIFT) % LAP;

                if offset < BLOCK_CAP {
                    // Drop the value in the slot.
                    let slot = (*block).slots.get_unchecked(offset);
                    let value = slot.value.get().read().assume_init();
                    drop(value);
                } else {
                    // Deallocate the block and move to the next one.
                    let next = (*block).next.load(Ordering::Relaxed);
                    drop(Box::from_raw(block));
                    block = next;
                }

                index = index.wrapping_add(1 << SHIFT);
            }

            // Deallocate the last remaining block.
            if !block.is_null() {
                drop(Box::from_raw(block));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn send_iter_receive() {
        let (sender, mut receiver) = unbounded::<u32>();
        sender.send(1).unwrap();
        sender.send(2).unwrap();
        sender.send(3).unwrap();
        drop(sender);
        let results = receiver.iter().collect::<Vec<_>>();
        assert_eq!(results, vec![1, 2, 3]);
    }
}
