use std::fmt::{self, Debug, Display, Formatter};
use std::ops::{Deref, DerefMut};

use super::pool::Pool;
use super::resource::ResourceGuard;

pub struct Managed<T, E> {
    value: Option<ResourceGuard<T>>,
    pool: Option<Pool<T, E>>,
}

impl<T, E> Managed<T, E> {
    pub(crate) fn new(value: ResourceGuard<T>, pool: Pool<T, E>) -> Self {
        Self {
            value: Some(value),
            pool: Some(pool),
        }
    }
}

impl<T: Debug, E> Debug for Managed<T, E> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        if f.alternate() {
            f.debug_struct("ManagedResource")
                .field("value", &self.deref())
                // .field("info", &self.info)
                .finish()
        } else {
            Debug::fmt(self.deref(), f)
        }
    }
}

impl<T: Display, E> Display for Managed<T, E> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        Display::fmt(self.deref(), f)
    }
}

impl<T, E> Deref for Managed<T, E> {
    type Target = T;
    fn deref(&self) -> &Self::Target {
        // note: panics after drop when value is taken
        self.value.as_ref().unwrap().as_ref().unwrap()
    }
}

impl<T, E> DerefMut for Managed<T, E> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        // note: panics after drop when value is taken
        self.value.as_mut().unwrap().as_mut().unwrap()
    }
}

impl<T, E> Drop for Managed<T, E> {
    fn drop(&mut self) {
        if let Some(pool) = self.pool.take() {
            pool.inner.release(self.value.take().unwrap());
        }
    }
}
