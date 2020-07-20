/// Copy of simple Lock used by futures::oneshot
use std::cell::UnsafeCell;
use std::fmt::{self, Debug, Display, Formatter};
use std::ops::{Deref, DerefMut};
use std::sync::atomic::{AtomicUsize, Ordering};

const SOME: usize = 1;
const FREE: usize = 2;
const SHIFT: usize = 2;

pub struct OptionLock<T> {
    data: UnsafeCell<Option<T>>,
    state: AtomicUsize,
}

impl<T> Default for OptionLock<T> {
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
pub enum Status {
    ExclusiveLock,
    None,
    ReadLock(usize),
    Some,
}

impl Status {
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

    pub unsafe fn get_unchecked(&self) -> &mut Option<T> {
        &mut *self.data.get()
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
        let state = self.state.load(Ordering::Acquire);
        println!("state: {}", state);
        if state & !SOME == 0 {
            Status::ExclusiveLock
        } else if state == FREE {
            Status::None
        } else if state == FREE | SOME {
            Status::Some
        } else {
            Status::ReadLock(state >> SHIFT)
        }
    }

    pub fn try_lock(&self) -> Option<OptionGuard<'_, T>> {
        if self.state.fetch_and(!FREE, Ordering::AcqRel) & !SOME == FREE {
            Some(OptionGuard { lock: self })
        } else {
            None
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

    pub fn upgrade_lock<'a>(
        &self,
        read: OptionRead<'a, T>,
    ) -> Result<OptionGuard<'_, T>, OptionRead<'a, T>> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn option_lock_exclusive() {
        let a = OptionLock::from(1);
        assert!(!a.status().is_locked());
        let mut guard = a.try_lock().unwrap();
        assert!(a.status().is_locked());
        assert_eq!(a.try_lock(), None);
        assert_eq!(a.try_take(), None);
        assert_eq!(a.read(), Err(ReadError::Locked));

        assert_eq!(*guard, Some(1));
        guard.replace(2);
        drop(guard);
        assert_eq!(*a.try_lock().unwrap(), Some(2));
        assert!(!a.status().is_locked());
    }

    #[test]
    fn option_lock_read() {
        let a = OptionLock::from(99);
        assert_eq!(a.status(), Status::Some);
        assert!(!a.status().is_locked());
        assert!(a.status().is_some());
        assert_eq!(a.status().readers(), 0);

        let read = a.read().unwrap();
        assert_eq!(a.status(), Status::ReadLock(1));
        assert!(!a.status().is_locked());
        assert!(a.status().is_some());
        assert_eq!(a.status().readers(), 1);
        assert_eq!(*read, 99);

        assert_eq!(a.try_lock(), None);
        assert_eq!(a.try_take(), None);

        let read2 = a.read().unwrap();
        assert_eq!(a.status(), Status::ReadLock(2));
        assert!(!a.status().is_locked());
        assert!(a.status().is_some());
        assert_eq!(a.status().readers(), 2);
        assert_eq!(*read2, 99);

        drop(read2);
        assert_eq!(a.status().readers(), 1);

        drop(read);
        assert_eq!(a.status(), Status::Some);

        assert_eq!(a.try_take(), Some(99));
        assert_eq!(a.status(), Status::None);
        assert_eq!(a.read(), Err(ReadError::Empty));
    }

    #[test]
    fn option_lock_read_upgrade() {
        let a = OptionLock::from(61);
        let read = a.read().unwrap();
        let write = a.upgrade_lock(read).unwrap();
        assert!(a.status().is_locked());
        drop(write);
        assert_eq!(a.status(), Status::Some);
        drop(a);

        let b = OptionLock::from(5);
        let read1 = b.read().unwrap();
        let read2 = b.read().unwrap();
        assert_eq!(b.status().readers(), 2);
        let read3 = b.upgrade_lock(read1).unwrap_err();
        assert!(!b.status().is_locked());
        assert_eq!(b.status().readers(), 2);
        drop(read3);
        assert_eq!(b.status().readers(), 1);
        drop(read2);
        assert_eq!(b.status(), Status::Some);
    }
}
