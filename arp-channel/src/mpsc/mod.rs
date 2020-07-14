use std::cell::UnsafeCell;
use std::fmt;
use std::iter::IntoIterator;
use std::marker::PhantomData;
use std::mem::MaybeUninit;
use std::pin::Pin;
use std::ptr;
use std::sync::{
    atomic::{self, AtomicPtr, AtomicUsize, Ordering},
    Arc,
};
use std::task::{Context, Poll, Waker};
use std::thread;

#[cfg(feature = "sink")]
use futures_util::sink::Sink;
use futures_util::stream::Stream;

use super::OptionLock;

mod drain;
use drain::{Drain, TryDrain};

// Bits indicating the state of a slot:
// * If a value has been written into the slot, `WRITE` is set.
// * If a value has been read from the slot, `READ` is set.
// * If the block is being destroyed, `DESTROY` is set.
const WRITE: usize = 1;
const READ: usize = 2;
const DESTROY: usize = 4;

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

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Disconnected;

impl fmt::Display for Disconnected {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "dropshot canceled")
    }
}

impl std::error::Error for Disconnected {}

/// A slot in a block.
struct Slot<T> {
    /// The value.
    value: UnsafeCell<MaybeUninit<T>>,

    /// The state of the slot.
    state: AtomicUsize,
}

impl<T> Slot<T> {
    const UNINIT: Slot<T> = Slot {
        value: UnsafeCell::new(MaybeUninit::uninit()),
        state: AtomicUsize::new(0),
    };

    /// Waits until a value is written into the slot.
    fn wait_write(&self) {
        while self.state.load(Ordering::Acquire) & WRITE == 0 {
            thread::yield_now();
        }
    }
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

    /// Waits until the next pointer is set.
    fn wait_next(&self) -> *mut Block<T> {
        loop {
            let next = self.next.load(Ordering::Acquire);
            if !next.is_null() {
                return next;
            }
            thread::yield_now();
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
struct Position<T> {
    /// The block in the linked list.
    block: AtomicPtr<Block<T>>,

    /// The index in the queue.
    index: AtomicUsize,
}

impl<T> Position<T> {
    pub fn load(&self, ordering: Ordering) -> (*mut Block<T>, usize) {
        let block = self.block.load(ordering);
        let index = self.index.load(ordering);
        (block, index)
    }
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
        if !self.closed {
            self.queue.inc_senders();
        }
        Self {
            closed: self.closed,
            queue: self.queue.clone(),
        }
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
        Drain::new(self.queue.clone())
    }

    pub fn try_iter(&mut self) -> TryDrain<T> {
        TryDrain::new(self.queue.clone())
    }

    pub fn try_recv(&mut self) -> Result<Option<T>, Disconnected> {
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
        match self.queue.pop_wake(cx.waker()) {
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
    head: Position<T>,

    tail: Position<T>,

    senders: AtomicUsize,

    waker: OptionLock<Waker>,

    /// Indicates that dropping a `Unbounded<T>` may drop values of type `T`.
    _marker: PhantomData<T>,
}

impl<T> Queue<T> {
    pub(crate) fn new() -> Self {
        Self {
            head: Position {
                block: AtomicPtr::new(ptr::null_mut()),
                index: AtomicUsize::new(0),
            },
            tail: Position {
                block: AtomicPtr::new(ptr::null_mut()),
                index: AtomicUsize::new(0),
            },
            senders: AtomicUsize::new(1),
            waker: OptionLock::new(None),
            _marker: PhantomData,
        }
    }

    pub fn push(&self, value: T) -> Option<T> {
        let mut tail = self.tail.index.load(Ordering::Acquire);
        let mut block = self.tail.block.load(Ordering::Acquire);
        let mut next_block = None;

        loop {
            // Check if the queue is closed.
            if tail & MARK_BIT != 0 {
                return Some(value);
            }

            // Calculate the offset of the index into the block.
            let offset = (tail >> SHIFT) % LAP;

            // If we reached the end of the block, wait until the next one is installed.
            if offset == BLOCK_CAP {
                thread::yield_now();
                tail = self.tail.index.load(Ordering::Acquire);
                block = self.tail.block.load(Ordering::Acquire);
                continue;
            }

            // If we're going to have to install the next block, allocate it in advance in order to
            // make the wait for other threads as short as possible.
            if offset + 1 == BLOCK_CAP && next_block.is_none() {
                next_block = Some(Box::new(Block::<T>::new()));
            }

            // If this is the first value to be pushed into the queue, we need to allocate the
            // first block and install it.
            if block.is_null() {
                let new = Box::into_raw(Box::new(Block::<T>::new()));

                if self
                    .tail
                    .block
                    .compare_and_swap(block, new, Ordering::Release)
                    == block
                {
                    self.head.block.store(new, Ordering::Release);
                    block = new;
                } else {
                    next_block = unsafe { Some(Box::from_raw(new)) };
                    tail = self.tail.index.load(Ordering::Acquire);
                    block = self.tail.block.load(Ordering::Acquire);
                    continue;
                }
            }

            let new_tail = tail + (1 << SHIFT);

            // Try advancing the tail forward.
            match self.tail.index.compare_exchange_weak(
                tail,
                new_tail,
                Ordering::SeqCst,
                Ordering::Acquire,
            ) {
                Ok(_) => unsafe {
                    // If we've reached the end of the block, install the next one.
                    if offset + 1 == BLOCK_CAP {
                        let next_block = Box::into_raw(next_block.unwrap());
                        self.tail.block.store(next_block, Ordering::Release);
                        self.tail.index.fetch_add(1 << SHIFT, Ordering::Release);
                        (*block).next.store(next_block, Ordering::Release);
                    }

                    // Write the value into the slot.
                    let slot = (*block).slots.get_unchecked(offset);
                    slot.value.get().write(MaybeUninit::new(value));
                    slot.state.fetch_or(WRITE, Ordering::Release);
                    return None;
                },
                Err(t) => {
                    tail = t;
                    block = self.tail.block.load(Ordering::Acquire);
                }
            }
        }
    }

    pub fn _pop(&self) -> Option<T> {
        let acquire = Ordering::Relaxed;
        let release = Ordering::Relaxed;
        let (mut block, mut head) = self.head.load(acquire);

        loop {
            // Calculate the offset of the index into the block.
            let offset = (head >> SHIFT) % LAP;

            // If we reached the end of the block, wait until the next one is installed.
            if offset == BLOCK_CAP {
                thread::yield_now();
                let (b, h) = self.head.load(acquire);
                block = b;
                head = h;
                continue;
            }

            let mut new_head = head + (1 << SHIFT);

            if new_head & MARK_BIT == 0 {
                atomic::fence(Ordering::SeqCst);
                let tail = self.tail.index.load(Ordering::Relaxed);

                // If the tail equals the head, that means the queue is empty.
                if head >> SHIFT == tail >> SHIFT {
                    return None;
                }

                // If head and tail are not in the same block, set `MARK_BIT` in head.
                if (head >> SHIFT) / LAP != (tail >> SHIFT) / LAP {
                    new_head |= MARK_BIT;
                }
            }

            // Try moving the head index forward.
            match self.head.index.compare_exchange_weak(
                head,
                new_head,
                Ordering::SeqCst,
                Ordering::Acquire,
            ) {
                Ok(_) => unsafe {
                    // If we've reached the end of the block, move to the next one.
                    if offset + 1 == BLOCK_CAP {
                        let next = (*block).wait_next();
                        let mut next_index = (new_head & !MARK_BIT).wrapping_add(1 << SHIFT);
                        if !(*next).next.load(Ordering::Relaxed).is_null() {
                            next_index |= MARK_BIT;
                        }

                        self.head.block.store(next, Ordering::Relaxed);
                        self.head.index.store(next_index, Ordering::Relaxed);
                    }

                    // Read the value.
                    let slot = (*block).slots.get_unchecked(offset);
                    slot.wait_write();
                    let value = slot.value.get().read().assume_init();

                    // Destroy the block if we've reached the end, or if another thread wanted to
                    // destroy but couldn't because we were busy reading from the slot.
                    // if offset + 1 == BLOCK_CAP {
                    //     Block::destroy(block, 0);
                    // } else if slot.state.fetch_or(READ, Ordering::AcqRel) & DESTROY != 0 {
                    //     Block::destroy(block, offset + 1);
                    // }
                    if offset + 1 == BLOCK_CAP {
                        // let destroy = next.block;
                        // next.block = (*next.block).next.load(Ordering::Acquire);
                        drop(Box::from_raw(block));
                    // next.index = next.index.wrapping_add(1 << SHIFT);
                    } else if slot.state.fetch_or(READ, Ordering::AcqRel) & DESTROY != 0 {
                        drop(Box::from_raw(block));
                        //Block::destroy(block, offset + 1);
                    }

                    return Some(value);
                },
                Err(h) => {
                    head = h;
                    block = self.head.block.load(acquire);
                }
            }
        }
    }

    pub fn pop(&self) -> Result<Option<T>, Disconnected> {
        match self._pop() {
            found @ Some(..) => Ok(found),
            _ => {
                if self.tx_closed() {
                    Err(Disconnected)
                } else {
                    Ok(None)
                }
            }
        }
    }

    pub fn pop_wake(&self, waker: &Waker) -> Result<Option<T>, Disconnected> {
        loop {
            match self._pop() {
                found @ Some(_) => return Ok(found),
                None => {
                    if let Some(mut guard) = self.waker.try_lock() {
                        guard.replace(waker.clone());
                    } else {
                        // a sender is trying to wake us already
                        continue;
                    }
                    if !self.is_empty() {
                        // we are slow to register the waker, so this checks if
                        // results are pending (a write was interrupted) or
                        // a write was started after pop()
                        self.wake_rx();
                    } else if self.tx_closed() {
                        return Err(Disconnected);
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
        let tail = self.tail.index.fetch_or(MARK_BIT, Ordering::SeqCst);

        if tail & MARK_BIT == 0 {
            true
        } else {
            false
        }
    }

    /// Returns `true` if the queue is closed.
    pub fn rx_closed(&self) -> bool {
        self.tail.index.load(Ordering::SeqCst) & MARK_BIT != 0
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

    pub fn is_empty(&self) -> bool {
        let head = self.head.index.load(Ordering::SeqCst);
        let tail = self.tail.index.load(Ordering::SeqCst);
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
        let (mut block, mut head) = self.head.load(Ordering::Relaxed);
        let mut tail = self.tail.index.load(Ordering::Relaxed);

        // Erase the lower bits.
        head &= !((1 << SHIFT) - 1);
        tail &= !((1 << SHIFT) - 1);

        unsafe {
            // Drop all values between `head` and `tail` and deallocate the heap-allocated blocks.
            while head != tail {
                let offset = (head >> SHIFT) % LAP;

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

                head = head.wrapping_add(1 << SHIFT);
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
