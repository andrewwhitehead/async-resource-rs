use std::cell::UnsafeCell;
use std::fmt::{self, Debug, Formatter};
use std::hash::{Hash, Hasher};
use std::ops::{Deref, DerefMut};
use std::sync::{
    atomic::{AtomicU8, Ordering},
    Arc,
};

use super::ResourceInfo;

const NONE: u8 = 0;
const SOME: u8 = 1;
const HELD: u8 = 2;

pub struct Inner<T> {
    data: UnsafeCell<(ResourceInfo, Option<T>)>,
    state: AtomicU8,
}

impl<T> Inner<T> {
    pub fn new(info: ResourceInfo, res: Option<T>, state: u8) -> Self {
        Self {
            data: UnsafeCell::new((info, res)),
            state: AtomicU8::new(state),
        }
    }
}

unsafe impl<T> Sync for Inner<T> {}

pub struct ResourceLock<T> {
    inner: Arc<Inner<T>>,
}

impl<T> ResourceLock<T> {
    pub fn new(info: ResourceInfo, res: Option<T>) -> Self {
        let state = if res.is_some() { SOME } else { NONE };
        let inner = Inner::new(info, res, state);
        Self {
            inner: Arc::new(inner),
        }
    }

    #[allow(unused)]
    pub fn is_locked(&self) -> bool {
        self.inner.state.load(Ordering::Acquire) == HELD
    }

    pub fn is_none(&self) -> bool {
        self.inner.state.load(Ordering::Acquire) == NONE
    }

    pub fn try_lock(&self) -> Option<ResourceGuard<T>> {
        match self.inner.state.swap(HELD, Ordering::AcqRel) {
            HELD => None,
            _ => Some(ResourceGuard {
                lock: self.inner.clone(),
            }),
        }
    }

    #[allow(unused)]
    pub fn try_unwrap(self) -> Result<(ResourceInfo, Option<T>), Self> {
        match Arc::try_unwrap(self.inner) {
            Ok(inner) => Ok(inner.data.into_inner()),
            Err(inner) => Err(Self { inner }),
        }
    }
}

impl<T> Clone for ResourceLock<T> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

impl<T> Debug for ResourceLock<T> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "ResourceLock({:p})", &*self.inner)
    }
}

impl<T> PartialEq for ResourceLock<T> {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
    }
}

impl<T> Eq for ResourceLock<T> {}

impl<T> Hash for ResourceLock<T> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        // FIXME use Arc::as_ptr when stabilized
        std::ptr::hash(&*self.inner, state)
    }
}

pub struct ResourceGuard<T> {
    lock: Arc<Inner<T>>,
}

impl<T> ResourceGuard<T> {
    pub fn as_lock(&self) -> ResourceLock<T> {
        ResourceLock {
            inner: self.lock.clone(),
        }
    }

    pub fn detach(mut self) -> Self {
        let inner = Inner::new(*self.info(), self.take(), HELD);
        Self {
            lock: Arc::new(inner),
        }
    }

    pub fn info(&mut self) -> &mut ResourceInfo {
        unsafe { &mut (*self.lock.data.get()).0 }
    }

    pub fn unlock(self) -> ResourceLock<T> {
        self.as_lock()
    }
}

impl<T> Deref for ResourceGuard<T> {
    type Target = Option<T>;
    fn deref(&self) -> &Self::Target {
        unsafe { &(*self.lock.data.get()).1 }
    }
}

impl<T> DerefMut for ResourceGuard<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        unsafe { &mut (*self.lock.data.get()).1 }
    }
}

impl<T> Drop for ResourceGuard<T> {
    fn drop(&mut self) {
        self.lock
            .state
            .swap(if self.is_some() { SOME } else { NONE }, Ordering::AcqRel);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn resource_lock_debug() {
        let lock = ResourceLock::new(ResourceInfo::default(), Some(1u32));
        assert_eq!(format!("{:?}", lock.clone()), format!("{:?}", lock))
    }

    #[test]
    fn resource_lock_eq_hash() {
        let lock = ResourceLock::new(ResourceInfo::default(), Some(1u32));
        let lock2 = lock.clone();

        // check equality
        assert_eq!(lock, lock2);

        // check hash
        let mut set = HashSet::new();
        set.insert(lock.clone());
        assert!(set.contains(&lock));

        // try mutating value
        lock.try_lock().unwrap().replace(2);
        assert_eq!(lock, lock2);
        assert!(set.contains(&lock));
    }
}
