use core::cell::UnsafeCell;
use core::fmt::{self, Debug, Formatter};
use core::mem::{ManuallyDrop, MaybeUninit};
use core::ptr::drop_in_place;
use core::sync::atomic::{spin_loop_hint, AtomicU8, Ordering};

const SOME: u8 = 0x1;
const FREE: u8 = 0x2;
const AVAILABLE: u8 = FREE | SOME;

/// A read/write lock around an Option value.
pub struct OptionLock<T> {
    data: UnsafeCell<MaybeUninit<T>>,
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
    /// Create a new, empty instance.
    pub const fn empty() -> Self {
        Self {
            data: UnsafeCell::new(MaybeUninit::uninit()),
            state: AtomicU8::new(FREE),
        }
    }

    /// Create a new populated instance.
    pub const fn new(value: T) -> Self {
        Self {
            data: UnsafeCell::new(MaybeUninit::new(value)),
            state: AtomicU8::new(AVAILABLE),
        }
    }

    #[inline]
    pub(crate) unsafe fn as_mut_ptr(&self) -> *mut T {
        (&mut *self.data.get()).as_mut_ptr()
    }

    #[inline]
    pub(crate) fn state(&self) -> u8 {
        self.state.load(Ordering::Relaxed)
    }

    /// Check if there is a stored value and no guard held.
    #[inline]
    pub fn is_some_unlocked(&self) -> bool {
        self.state() == AVAILABLE
    }

    /// Check if there is no stored value and no guard held.
    #[inline]
    pub fn is_none_unlocked(&self) -> bool {
        self.state() == FREE
    }

    /// Check if a guard is held.
    #[inline]
    pub fn is_locked(&self) -> bool {
        self.state() & FREE == 0
    }

    /// Get a mutable reference to the contained value, if any.
    pub fn get_mut(&mut self) -> Option<&mut T> {
        if self.is_some_unlocked() {
            Some(unsafe { &mut *self.as_mut_ptr() })
        } else {
            None
        }
    }

    /// Unwrap an owned lock instance.
    pub fn into_inner(self) -> Option<T> {
        if self.state() & SOME != 0 {
            let slf = ManuallyDrop::new(self);
            Some(unsafe { slf.as_mut_ptr().read() })
        } else {
            None
        }
    }

    /// In a spin loop, wait to acquire the lock.
    pub fn spin_lock(&self) -> OptionGuard<'_, T> {
        loop {
            if let Some(guard) = self.try_lock() {
                break guard;
            }
            // use a relaxed check in spin loop
            while self.state() & FREE == 0 {
                spin_loop_hint();
            }
        }
    }

    /// In a spin loop, wait to acquire the lock with a value of None.
    pub fn spin_lock_none(&self) -> OptionGuard<'_, T> {
        loop {
            if let Some(guard) = self.try_lock_none() {
                break guard;
            }
            // use a relaxed check in spin loop
            while self.state() != FREE {
                spin_loop_hint();
            }
        }
    }

    /// In a spin loop, wait to take a value from the lock.
    pub fn spin_take(&self) -> T {
        loop {
            if let Some(result) = self.try_take() {
                break result;
            }
            // use a relaxed check in spin loop
            while self.state() != AVAILABLE {
                spin_loop_hint();
            }
        }
    }

    /// Try to acquire an exclusive lock.
    ///
    /// On successful acquisition `Some(OptionGuard<'_, T>)` is returned, representing
    /// an exclusive read/write lock.
    pub fn try_lock(&self) -> Option<OptionGuard<'_, T>> {
        let state = self.state.fetch_and(!FREE, Ordering::Release);
        if state & FREE != 0 {
            Some(OptionGuard {
                lock: self,
                filled: state & SOME != 0,
            })
        } else {
            None
        }
    }

    /// Try to acquire an exclusive lock, but only if the value is currently None.
    ///
    /// On successful acquisition `Some(OptionGuard<'_, T>)` is returned, representing
    /// an exclusive read/write lock.
    pub fn try_lock_none(&self) -> Option<OptionGuard<'_, T>> {
        loop {
            match self
                .state
                .compare_exchange_weak(FREE, 0, Ordering::AcqRel, Ordering::Relaxed)
            {
                Ok(_) => {
                    break Some(OptionGuard {
                        lock: self,
                        filled: false,
                    })
                }
                Err(FREE) => {
                    // retry
                }
                Err(_) => break None,
            }
        }
    }

    /// Try to take a stored value from the lock.
    ///
    /// On successful acquisition `Some(T)` is returned.
    ///
    /// On failure, `None` is returned. Acquisition can fail either because
    /// there is no contained value, or because the lock is held by a guard.
    pub fn try_take(&self) -> Option<T> {
        loop {
            match self.state.compare_exchange_weak(
                AVAILABLE,
                SOME,
                Ordering::AcqRel,
                Ordering::Relaxed,
            ) {
                Ok(_) => {
                    break Some(
                        OptionGuard {
                            lock: self,
                            filled: true,
                        }
                        .take()
                        .unwrap(),
                    )
                }
                Err(AVAILABLE) => {
                    // retry
                }
                Err(_) => break None,
            }
        }
    }

    /// Replace the value in an owned `OptionLock`.
    pub fn replace(&mut self, value: T) -> Option<T> {
        let result = if self.state() & SOME != 0 {
            Some(unsafe { self.as_mut_ptr().read() })
        } else {
            self.state.fetch_or(SOME, Ordering::Relaxed);
            None
        };
        unsafe { self.as_mut_ptr().write(value) };
        result
    }

    /// Take the value (if any) from an owned `OptionLock`.
    pub fn take(&mut self) -> Option<T> {
        if self.state() & SOME != 0 {
            self.state.fetch_and(!SOME, Ordering::Relaxed);
            Some(unsafe { self.as_mut_ptr().read() })
        } else {
            None
        }
    }
}

impl<T> Drop for OptionLock<T> {
    fn drop(&mut self) {
        if self.state() & SOME != 0 {
            unsafe {
                drop_in_place(self.as_mut_ptr());
            }
        }
    }
}

impl<T> From<T> for OptionLock<T> {
    fn from(data: T) -> Self {
        Self::new(data)
    }
}

impl<T> From<Option<T>> for OptionLock<T> {
    fn from(data: Option<T>) -> Self {
        if let Some(data) = data {
            Self::new(data)
        } else {
            Self::empty()
        }
    }
}

impl<T> Into<Option<T>> for OptionLock<T> {
    fn into(mut self) -> Option<T> {
        self.take()
    }
}

impl<T> Debug for OptionLock<T> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "OptionLock({})",
            match self.state() {
                FREE => "None",
                AVAILABLE => "Some",
                _ => "Locked",
            }
        )
    }
}

/// A write guard for the value of an [`OptionLock`]
pub struct OptionGuard<'a, T> {
    lock: &'a OptionLock<T>,
    filled: bool,
}

impl<T> OptionGuard<'_, T> {
    pub fn as_ref(&self) -> Option<&T> {
        if self.filled {
            Some(unsafe { &*self.lock.as_mut_ptr() })
        } else {
            None
        }
    }

    pub fn as_mut_ref(&mut self) -> Option<&mut T> {
        if self.filled {
            Some(unsafe { &mut *self.lock.as_mut_ptr() })
        } else {
            None
        }
    }

    #[inline]
    pub fn is_none(&self) -> bool {
        !self.filled
    }

    #[inline]
    pub fn is_some(&self) -> bool {
        self.filled
    }

    /// Replace the value in the lock, returning the previous value, if any.
    pub fn replace(&mut self, value: T) -> Option<T> {
        let ret = if self.filled {
            Some(unsafe { self.lock.as_mut_ptr().read() })
        } else {
            self.filled = true;
            None
        };
        unsafe {
            self.lock.as_mut_ptr().write(value);
        }
        ret
    }

    /// Take the current value from the lock, if any.
    pub fn take(&mut self) -> Option<T> {
        if self.filled {
            self.filled = false;
            Some(unsafe { self.lock.as_mut_ptr().read() })
        } else {
            None
        }
    }
}

impl<T: Debug> Debug for OptionGuard<'_, T> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_tuple("OptionGuard").field(&self.as_ref()).finish()
    }
}

impl<'a, T> Drop for OptionGuard<'a, T> {
    fn drop(&mut self) {
        self.lock.state.store(
            if self.filled { AVAILABLE } else { FREE },
            Ordering::Release,
        );
    }
}
