use std::cell::UnsafeCell;
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

unsafe impl<T> Sync for Inner<T> {}

pub struct ResourceLock<T> {
    inner: Arc<Inner<T>>,
}

impl<T> ResourceLock<T> {
    pub fn new(info: ResourceInfo, res: Option<T>) -> Self {
        let state = if res.is_some() { SOME } else { NONE };
        Self {
            inner: Arc::new(Inner {
                data: UnsafeCell::new((info, res)),
                state: AtomicU8::new(state),
            }),
        }
    }

    pub fn is_locked(&self) -> bool {
        self.inner.state.load(Ordering::Acquire) == HELD
    }

    pub fn try_lock(&self) -> Option<ResourceGuard<T>> {
        match self.inner.state.swap(HELD, Ordering::AcqRel) {
            HELD => None,
            _ => Some(ResourceGuard {
                lock: self.inner.clone(),
            }),
        }
    }

    pub fn try_take(&self) -> Option<T> {
        self.inner
            .state
            .compare_exchange(SOME, HELD, Ordering::AcqRel, Ordering::Acquire)
            .ok()
            .and_then(|_| {
                let result = unsafe { (*self.inner.data.get()).1.take() };
                self.inner.state.store(NONE, Ordering::Release);
                result
            })
    }

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

pub struct ResourceGuard<T> {
    lock: Arc<Inner<T>>,
}

impl<T> ResourceGuard<T> {
    pub fn info(&mut self) -> &mut ResourceInfo {
        unsafe { &mut (*self.lock.data.get()).0 }
    }

    pub fn as_lock(&self) -> ResourceLock<T> {
        ResourceLock {
            inner: self.lock.clone(),
        }
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
