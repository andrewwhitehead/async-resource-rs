use std::fmt::{self, Debug, Display, Formatter};
use std::ops::{Deref, DerefMut};
use std::sync::Arc;

use super::queue::Queue;
use super::resource::ResourceGuard;

pub struct Managed<T> {
    value: Option<ResourceGuard<T>>,
    queue: Option<Arc<Queue<T>>>,
}

impl<T> Managed<T> {
    pub(crate) fn new(value: ResourceGuard<T>, queue: Arc<Queue<T>>) -> Self {
        Self {
            value: Some(value),
            queue: Some(queue),
        }
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
        if let Some(queue) = self.queue.take() {
            queue.release(self.value.take().unwrap());
        }
    }
}
