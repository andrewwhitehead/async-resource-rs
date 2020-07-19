/// Copy of simple Lock used by futures::oneshot
use std::cell::UnsafeCell;
use std::ops::{Deref, DerefMut};
use std::sync::atomic::{AtomicU8, Ordering};

const NONE: u8 = 0;
const SOME: u8 = 1;
const HELD: u8 = 2;

pub struct OptionLock<T> {
    data: UnsafeCell<Option<T>>,
    state: AtomicU8,
}

impl<T> Default for OptionLock<T> {
    fn default() -> Self {
        Self::empty()
    }
}

unsafe impl<T: Send> Send for OptionLock<T> {}
unsafe impl<T: Send> Sync for OptionLock<T> {}

impl<T> OptionLock<T> {
    pub fn new(data: Option<T>) -> Self {
        let state = if data.is_some() { SOME } else { NONE };
        Self {
            data: UnsafeCell::new(data),
            state: AtomicU8::new(state),
        }
    }

    pub const fn empty() -> Self {
        Self {
            data: UnsafeCell::new(None),
            state: AtomicU8::new(NONE),
        }
    }

    pub fn into_inner(self) -> Option<T> {
        self.data.into_inner()
    }

    pub fn is_locked(&self) -> bool {
        self.state.load(Ordering::Acquire) == HELD
    }

    pub fn try_lock(&self) -> Option<OptionGuard<'_, T>> {
        match self.state.swap(HELD, Ordering::AcqRel) {
            HELD => None,
            _ => Some(OptionGuard { lock: self }),
        }
    }

    pub fn try_take(&self) -> Option<T> {
        self.state
            .compare_exchange(SOME, HELD, Ordering::AcqRel, Ordering::Acquire)
            .ok()
            .and_then(|_| {
                let result = unsafe { (*self.data.get()).take() };
                self.state.store(NONE, Ordering::Release);
                result
            })
    }
}

pub struct OptionGuard<'a, T> {
    lock: &'a OptionLock<T>,
}

impl<T> Deref for OptionGuard<'_, T> {
    type Target = Option<T>;
    fn deref(&self) -> &Self::Target {
        unsafe { &*self.lock.data.get() }
    }
}

impl<T> DerefMut for OptionGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        unsafe { &mut *self.lock.data.get() }
    }
}

impl<'a, T> Drop for OptionGuard<'a, T> {
    fn drop(&mut self) {
        self.lock
            .state
            .swap(if self.is_some() { SOME } else { NONE }, Ordering::AcqRel);
    }
}

#[cfg(test)]
mod tests {
    use super::OptionLock;

    #[test]
    fn lock_exclusive() {
        let a = OptionLock::new(Some(1));
        let mut a1 = a.try_lock().unwrap();
        assert!(a.try_lock().is_none());
        assert_eq!(*a1, Some(1));
        a1.replace(2);
        drop(a1);
        assert_eq!(*a.try_lock().unwrap(), Some(2));
        assert_eq!(*a.try_lock().unwrap(), Some(2));
    }
}
