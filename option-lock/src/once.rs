use super::lock::OptionLock;
use core::fmt::{self, Debug, Display, Formatter};

/// An `Option` value which can be safely written once.
pub struct OnceCell<T>(OptionLock<T>);

impl<T> OnceCell<T> {
    /// Create a new, empty `OnceCell`.
    pub const fn empty() -> Self {
        Self(OptionLock::empty())
    }

    /// Create a `OnceCell` from an owned value.
    pub const fn new(value: T) -> Self {
        Self(OptionLock::new(value))
    }

    /// Get a shared reference to the contained value, if any.
    pub fn get(&self) -> Option<&T> {
        if self.0.is_some_unlocked() {
            // safe because the value is never reassigned
            Some(unsafe { &*self.0.as_mut_ptr() })
        } else {
            None
        }
    }

    /// Get a mutable reference to the contained value, if any.
    pub fn get_mut(&mut self) -> Option<&mut T> {
        self.0.get_mut()
    }

    /// Assign the value of the OnceCell, returning `Some(value)` if
    /// the cell is already locked or populated.
    pub fn set(&self, value: T) -> Option<T> {
        if let Some(mut guard) = self.0.try_lock_none() {
            guard.replace(value)
        } else {
            Some(value)
        }
    }

    /// Extract the inner value.
    pub fn into_inner(self) -> Option<T> {
        self.0.into_inner()
    }

    /// Check if the lock is currently acquired.
    pub fn is_locked(&self) -> bool {
        self.0.is_locked()
    }
}

impl<T: Clone> Clone for OnceCell<T> {
    fn clone(&self) -> Self {
        Self::from(self.get().cloned())
    }
}

impl<T> Default for OnceCell<T> {
    fn default() -> Self {
        Self(None.into())
    }
}

impl<T: Debug> Debug for OnceCell<T> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_tuple("OnceCell").field(&self.get()).finish()
    }
}

impl<T: Display> Display for OnceCell<T> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        if let Some(val) = self.get() {
            Display::fmt(val, f)
        } else {
            write!(f, "None")
        }
    }
}

impl<T> From<T> for OnceCell<T> {
    fn from(data: T) -> Self {
        Self(data.into())
    }
}

impl<T> From<Option<T>> for OnceCell<T> {
    fn from(data: Option<T>) -> Self {
        Self(data.into())
    }
}

impl<T> From<OptionLock<T>> for OnceCell<T> {
    fn from(lock: OptionLock<T>) -> Self {
        Self(lock)
    }
}
