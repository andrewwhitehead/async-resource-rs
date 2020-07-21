//! This crate defines a locking structure wrapping an Option value. The lock
//! can be acquired for either an exclusive write lock or a series of read
//! locks, using a single atomic operation per acquisition.
//!
//! The `try_lock`, `try_read`, and `try_take` operations are non-blocking and
//! appropriate for using within a polled Future, but the lock cannot register
//! wakers or park the current thread.
//!
//! This structure allows for multiple usage patterns. As one example, it can
//! be used to implement a version of `once_cell` / `lazy_static`:
//!
//! ```
//! use option_lock::{OptionLock, OptionGuard, OptionRead, ReadError};
//! use std::collections::HashMap;
//!
//! static REPOSITORY: OptionLock<HashMap<String, u32>> = OptionLock::new();
//!
//! pub fn resource_map() -> OptionRead<'static, HashMap<String, u32>> {
//!   loop {
//!     match REPOSITORY.try_read() {
//!       Ok(read) => break read,
//!       Err(ReadError::Empty) => {
//!         if let Ok(mut guard) = REPOSITORY.try_lock() {
//!           let mut inst = HashMap::new();
//!           inst.insert("hello".to_string(), 5);
//!           guard.replace(inst);
//!           break OptionGuard::downgrade(guard).unwrap();
//!         }
//!       }
//!       Err(_) => {}
//!     }
//!     // wait while the resource is created
//!     std::thread::yield_now();
//!   }
//! }
//!
//! assert_eq!(*resource_map().get("hello").unwrap(), 5);
//! ```
//!
//! There are additional examples in the repository.
//!
//! This crate is no-std compatible when compiled without the `std` feature.

#![cfg_attr(not(feature = "std"), no_std)]

// FIXME avoid overflow on too many readers (reserve top bit of status)

use core::cell::UnsafeCell;
use core::fmt::{self, Debug, Display, Formatter};
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{spin_loop_hint, AtomicUsize, Ordering};

const SOME: usize = 0x1;
const FREE: usize = 0x2;
const AVAILABLE: usize = FREE | SOME;
const SHIFT: usize = 2;

/// A read-write lock around an Option value
pub struct OptionLock<T> {
    data: UnsafeCell<Option<T>>,
    state: AtomicUsize,
}

impl<T: Default> Default for OptionLock<T> {
    fn default() -> Self {
        Self::new()
    }
}

/// An error which may be thrown by [`OptionLock::read`]
#[derive(Debug, PartialEq, Eq)]
pub enum ReadError {
    Empty,
    Locked,
}

impl ReadError {
    /// Check if the read/take operation failed because there was no stored
    /// value
    pub fn is_empty(&self) -> bool {
        matches!(self, Self::Empty)
    }

    /// Check if the read/take operation failed because the lock was held by
    /// a competing guard
    pub fn is_locked(&self) -> bool {
        matches!(self, Self::Locked)
    }
}

impl Display for ReadError {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => write!(f, "OptionLock is empty"),
            Self::Locked => write!(f, "OptionLock is locked exclusively"),
        }
    }
}

#[cfg(feature = "std")]
impl std::error::Error for ReadError {}

/// An error which may be thrown by [`OptionLock::try_lock`]
#[derive(Debug, PartialEq, Eq)]
pub struct TryLockError;

impl Display for TryLockError {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "OptionLock cannot be locked exclusively")
    }
}

#[cfg(feature = "std")]
impl std::error::Error for TryLockError {}

/// The result of [`OptionLock::status`]
#[derive(Debug, PartialEq, Eq)]
pub enum Status {
    Available,
    Empty,
    ExclusiveLock,
    ReadLock(usize),
}

impl Status {
    #[inline]
    pub(crate) fn new(val: usize) -> Self {
        match val {
            AVAILABLE => Self::Available,
            state if state & AVAILABLE == FREE => Self::Empty,
            state if state & FREE == 0 && (state & SOME == 0 || state >> SHIFT == 0) => {
                Self::ExclusiveLock
            }
            state => Self::ReadLock(state >> SHIFT),
        }
    }

    /// Check if the lock has a stored value that can be safely read
    pub fn can_read(&self) -> bool {
        matches!(self, Self::Available | Self::ReadLock(..))
    }

    /// Check if the lock has a stored value that can be safely removed
    pub fn can_take(&self) -> bool {
        matches!(self, Self::Available)
    }

    /// Check if the lock is free and has no stored value
    pub fn is_empty(&self) -> bool {
        matches!(self, Self::Empty)
    }

    /// Check if an exclusive lock has been taken
    pub fn is_locked(&self) -> bool {
        matches!(self, Self::ExclusiveLock)
    }

    /// Check the number of concurrent read guards
    pub fn readers(&self) -> usize {
        if let Self::ReadLock(count) = self {
            *count
        } else {
            0
        }
    }
}

unsafe impl<T: Send> Send for OptionLock<T> {}
unsafe impl<T: Send> Sync for OptionLock<T> {}

impl<T> OptionLock<T> {
    /// Create a new, empty instance.
    pub const fn new() -> Self {
        Self {
            data: UnsafeCell::new(None),
            state: AtomicUsize::new(FREE),
        }
    }

    /// Get an unsafe, mutable reference to the contained Option.
    pub unsafe fn get_unchecked(&self) -> &mut Option<T> {
        &mut *self.data.get()
    }

    /// Get a safe mutable reference to the contained Option when holding the
    /// lock instance exclusively.
    pub fn get_mut(&mut self) -> &mut Option<T> {
        unsafe { self.get_unchecked() }
    }

    /// Unwrap an owned lock instance.
    pub fn into_inner(self) -> Option<T> {
        self.data.into_inner()
    }

    /// In a spin loop, wait to acquire the lock.
    pub fn spin_lock(&self) -> OptionGuard<'_, T> {
        loop {
            if let Ok(guard) = self.try_lock() {
                break guard;
            }
            // use a relaxed check in spin loop
            while self.status().is_locked() {
                spin_loop_hint();
            }
        }
    }

    /// In a spin loop, wait to take a value from the lock.
    pub fn spin_take(&self) -> T {
        loop {
            if let Ok(result) = self.try_take() {
                break result;
            }
            // use a relaxed check in spin loop
            while !self.status().can_take() {
                spin_loop_hint();
            }
        }
    }

    /// Fetch the current status of the lock.
    pub fn status(&self) -> Status {
        Status::new(self.state.load(Ordering::Relaxed))
    }

    /// Try to acquire an exclusive lock.
    pub fn try_lock(&self) -> Result<OptionGuard<'_, T>, TryLockError> {
        let state = self.state.fetch_and(!FREE, Ordering::AcqRel);
        if state & FREE == FREE && (state & SOME == 0 || state >> SHIFT == 0) {
            Ok(OptionGuard { lock: self })
        } else {
            Err(TryLockError)
        }
    }

    /// Try to acquire a read-only lock.
    pub fn try_read(&self) -> Result<OptionRead<'_, T>, ReadError> {
        let state = self.state.fetch_add(1 << SHIFT, Ordering::AcqRel);
        if state == AVAILABLE || (state & SOME == SOME && state >> SHIFT != 0) {
            // first reader or subsequent reader
            Ok(OptionRead { lock: self })
        } else if state >> 1 == 0 {
            Err(ReadError::Locked)
        } else {
            Err(ReadError::Empty)
        }
    }

    /// Try to take a stored value from the lock.
    pub fn try_take(&self) -> Result<T, ReadError> {
        loop {
            match self.state.compare_exchange_weak(
                AVAILABLE,
                SOME,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => break Ok(OptionGuard { lock: self }.take().unwrap()),
                Err(AVAILABLE) => {
                    // retry
                }
                Err(state) if state & AVAILABLE == FREE => break Err(ReadError::Empty),
                Err(_) => break Err(ReadError::Locked),
            }
        }
    }
}

impl<T> From<T> for OptionLock<T> {
    fn from(data: T) -> Self {
        Self {
            data: UnsafeCell::new(Some(data)),
            state: AtomicUsize::new(SOME | FREE),
        }
    }
}

impl<T> From<Option<T>> for OptionLock<T> {
    fn from(data: Option<T>) -> Self {
        let state = if data.is_some() { AVAILABLE } else { FREE };
        Self {
            data: UnsafeCell::new(data),
            state: AtomicUsize::new(state),
        }
    }
}

impl<T> Debug for OptionLock<T> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("OptionLock")
            .field("status", &self.status())
            .finish()
    }
}

/// A read guard for the value of a populated [`OptionLock`]
pub struct OptionRead<'a, T> {
    lock: &'a OptionLock<T>,
}

impl<'a, T: 'a> OptionRead<'a, T> {
    /// Try to upgrade a read guard to a write guard.
    ///
    /// This requires that there are no other active readers.
    pub fn try_lock(read: OptionRead<'a, T>) -> Result<OptionGuard<'a, T>, OptionRead<'a, T>> {
        let mut state = (1 << SHIFT) | AVAILABLE;
        loop {
            match read.lock.state.compare_exchange_weak(
                state,
                SOME,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    let guard = OptionGuard { lock: read.lock };
                    std::mem::forget(read);
                    break Ok(guard);
                }
                Err(s) if s & !FREE == (1 << SHIFT) | SOME => {
                    state = s;
                    continue;
                }
                _ => break Err(read),
            }
        }
    }

    /// Try to take the contained value from an acquired read guard.
    ///
    /// This requires that there are no other active readers.
    pub fn try_take(read: OptionRead<'a, T>) -> Result<T, OptionRead<'a, T>> {
        Self::try_lock(read).map(|mut guard| guard.take().unwrap())
    }
}

impl<T> Deref for OptionRead<'_, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        unsafe { self.lock.get_unchecked() }.as_ref().unwrap()
    }
}

impl<T: Debug> Debug for OptionRead<'_, T> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        if f.alternate() {
            f.debug_tuple("OptionRead").field(&**self).finish()
        } else {
            Debug::fmt(&**self, f)
        }
    }
}

impl<T: Display> Display for OptionRead<'_, T> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        Display::fmt(&**self, f)
    }
}

impl<T> Drop for OptionRead<'_, T> {
    fn drop(&mut self) {
        let prev = self.lock.state.fetch_sub(1 << SHIFT, Ordering::AcqRel);
        if prev == 1 << SHIFT | SOME {
            // another process tried to take a write lock and failed, so try to restore FREE flag
            self.lock
                .state
                .compare_exchange(SOME, AVAILABLE, Ordering::AcqRel, Ordering::Relaxed)
                .unwrap_or_default();
        }
    }
}

impl<T> PartialEq for OptionRead<'_, T> {
    fn eq(&self, other: &Self) -> bool {
        std::ptr::eq(self.lock, other.lock)
    }
}

impl<T> Eq for OptionRead<'_, T> {}

/// A write guard for the value of an [`OptionLock`]
pub struct OptionGuard<'a, T> {
    lock: &'a OptionLock<T>,
}

impl<T> OptionGuard<'_, T> {
    /// Downgrade a write guard to a read guard.
    pub fn downgrade<'a>(guard: OptionGuard<'a, T>) -> Option<OptionRead<'_, T>> {
        if guard.is_some() {
            guard
                .lock
                .state
                .store((1 << SHIFT) | AVAILABLE, Ordering::Release);
            let read = OptionRead { lock: guard.lock };
            std::mem::forget(guard);
            Some(read)
        } else {
            None
        }
    }
}

impl<T: Debug> Debug for OptionGuard<'_, T> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        if f.alternate() {
            f.debug_tuple("OptionGuard").field(&**self).finish()
        } else {
            Debug::fmt(&**self, f)
        }
    }
}

impl<T> Deref for OptionGuard<'_, T> {
    type Target = Option<T>;

    fn deref(&self) -> &Self::Target {
        unsafe { self.lock.get_unchecked() }
    }
}

impl<T> DerefMut for OptionGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        unsafe { self.lock.get_unchecked() }
    }
}

impl<'a, T> Drop for OptionGuard<'a, T> {
    fn drop(&mut self) {
        self.lock.state.store(
            if self.is_some() { AVAILABLE } else { FREE },
            Ordering::Release,
        );
    }
}

impl<T> PartialEq for OptionGuard<'_, T> {
    fn eq(&self, other: &Self) -> bool {
        std::ptr::eq(self.lock, other.lock)
    }
}

impl<T> Eq for OptionGuard<'_, T> {}
