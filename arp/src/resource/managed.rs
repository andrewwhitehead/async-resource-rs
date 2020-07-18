use std::fmt::{self, Debug, Display, Formatter};
use std::ops::{Deref, DerefMut};
use std::sync::Arc;

use super::{ResourceGuard, ResourceInfo};
use crate::shared::Shared;

pub struct Managed<T> {
    shared: Option<Arc<Shared<T>>>,
    value: Option<ResourceGuard<T>>,
}

impl<T> Managed<T> {
    pub(crate) fn new(value: ResourceGuard<T>, shared: Arc<Shared<T>>) -> Self {
        Self {
            shared: Some(shared),
            value: Some(value),
        }
    }

    pub fn discard(mng_self: &mut Self) {
        mng_self.value.as_mut().unwrap().discard()
    }

    pub fn info(mng_self: &Self) -> &ResourceInfo {
        mng_self.value.as_ref().unwrap().info()
    }
}

impl<T: Debug> Debug for Managed<T> {
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

impl<T: Display> Display for Managed<T> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        Display::fmt(self.deref(), f)
    }
}

impl<T> Deref for Managed<T> {
    type Target = T;
    fn deref(&self) -> &Self::Target {
        // note: panics after drop when value is taken
        self.value.as_ref().unwrap().as_ref().unwrap()
    }
}

impl<T> DerefMut for Managed<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        // note: panics after drop when value is taken
        self.value.as_mut().unwrap().as_mut().unwrap()
    }
}

impl<T> Drop for Managed<T> {
    fn drop(&mut self) {
        if let Some(shared) = self.shared.take() {
            shared.release(self.value.take().unwrap());
        }
    }
}
