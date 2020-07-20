//! This crate defines a locking structure wrapping an Option value, which
//! uses only atomic operations to allow a single exclusive write lock or
//! a series of concurrent read locks on the contained data.
//!
//! This structure allows for multiple usage patterns. It can be used to
//! implement a version of `once_cell` / `lazy_static`:
//!
//! ```
//! use option_lock::{OptionLock, OptionRead};
//! use std::collections::HashMap;
//!
//! static REPOSITORY: OptionLock<HashMap<String, u32>> = OptionLock::new();
//!
//! pub fn resource_map() -> OptionRead<'static, HashMap<String, u32>> {
//!   loop {
//!     if let Ok(read) = REPOSITORY.read() {
//!       break read;
//!     } else if let Ok(mut guard) = REPOSITORY.try_lock() {
//!       let mut inst = HashMap::new();
//!       inst.insert("hello".to_string(), 5);
//!       guard.replace(inst);
//!       break REPOSITORY.downgrade(guard).unwrap();
//!     } else {
//!       // wait while the resource is created
//!       std::thread::yield_now();
//!     }
//!   }
//! }
//!
//! assert_eq!(*resource_map().get("hello").unwrap(), 5);
//! ```

use core::cell::UnsafeCell;
use core::fmt::{self, Debug, Display, Formatter};
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicUsize, Ordering};

const SOME: usize = 0x1;
const FREE: usize = 0x2;
const SHIFT: usize = 2;

/// A read-
pub struct OptionLock<T> {
    data: UnsafeCell<Option<T>>,
    state: AtomicUsize,
}

impl<T: Default> Default for OptionLock<T> {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum ReadError {
    Empty,
    Locked,
}

#[derive(Debug, PartialEq, Eq)]
pub struct TryLockError;

#[derive(Debug, PartialEq, Eq)]
pub enum Status {
    ExclusiveLock,
    None,
    ReadLock(usize),
    Some,
}

impl Status {
    #[inline]
    pub(crate) fn new(val: usize) -> Self {
        if val & !SOME == 0 {
            Self::ExclusiveLock
        } else if val == FREE {
            Self::None
        } else if val == FREE | SOME {
            Self::Some
        } else {
            Self::ReadLock(val >> SHIFT)
        }
    }

    pub fn is_locked(&self) -> bool {
        matches!(self, Self::ExclusiveLock)
    }

    pub fn is_none(&self) -> bool {
        matches!(self, Self::None)
    }

    pub fn is_some(&self) -> bool {
        matches!(self, Self::Some | Self::ReadLock(..))
    }

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
    pub const fn new() -> Self {
        Self {
            data: UnsafeCell::new(None),
            state: AtomicUsize::new(FREE),
        }
    }

    pub fn downgrade<'a>(&self, guard: OptionGuard<'a, T>) -> Option<OptionRead<'_, T>> {
        assert!(std::ptr::eq(guard.lock, self));

        if guard.is_some() {
            self.state
                .store((1 << SHIFT) | FREE | SOME, Ordering::Release);
            std::mem::forget(guard);
            Some(OptionRead { lock: &self })
        } else {
            None
        }
    }

    pub unsafe fn get_unchecked(&self) -> &mut Option<T> {
        &mut *self.data.get()
    }

    pub fn get_mut(&mut self) -> &mut Option<T> {
        unsafe { self.get_unchecked() }
    }

    pub fn into_inner(self) -> Option<T> {
        self.data.into_inner()
    }

    pub fn read(&self) -> Result<OptionRead<'_, T>, ReadError> {
        let state = self.state.fetch_add(1 << SHIFT, Ordering::AcqRel);
        if state == FREE | SOME || (state & SOME == SOME && state >> SHIFT != 0) {
            // first reader or subsequent reader
            Ok(OptionRead { lock: self })
        } else if state >> 1 == 0 {
            Err(ReadError::Locked)
        } else {
            Err(ReadError::Empty)
        }
    }

    pub fn status(&self) -> Status {
        Status::new(self.state.load(Ordering::Acquire))
    }

    pub fn try_lock(&self) -> Result<OptionGuard<'_, T>, TryLockError> {
        let state = self.state.fetch_and(!FREE, Ordering::AcqRel);
        if state & FREE == FREE && (state & SOME == 0 || state >> SHIFT == 0) {
            Ok(OptionGuard { lock: self })
        } else {
            Err(TryLockError)
        }
    }

    pub fn try_take(&self) -> Option<T> {
        if self.state.fetch_and(!FREE, Ordering::AcqRel) & !SOME == FREE {
            let result = unsafe { (*self.data.get()).take() };
            self.state.store(FREE, Ordering::Release);
            result
        } else {
            None
        }
    }

    pub fn try_upgrade<'a>(
        &self,
        read: OptionRead<'a, T>,
    ) -> Result<OptionGuard<'_, T>, OptionRead<'a, T>> {
        assert!(std::ptr::eq(read.lock, self));

        let mut state = (1 << SHIFT) | FREE | SOME;
        loop {
            match self
                .state
                .compare_exchange_weak(state, SOME, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => {
                    std::mem::forget(read);
                    break Ok(OptionGuard { lock: &self });
                }
                Err(s) if s & !FREE == (1 << SHIFT) | SOME => {
                    state = s;
                    continue;
                }
                _ => break Err(read),
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
        let state = if data.is_some() { SOME | FREE } else { FREE };
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

pub struct OptionRead<'a, T> {
    lock: &'a OptionLock<T>,
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
                .compare_exchange(SOME, SOME | FREE, Ordering::AcqRel, Ordering::Relaxed)
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

pub struct OptionGuard<'a, T> {
    lock: &'a OptionLock<T>,
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
            if self.is_some() { SOME | FREE } else { FREE },
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
