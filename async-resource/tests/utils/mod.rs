use std::sync::atomic::{AtomicUsize, Ordering};

pub struct AtomicCounter {
    count: AtomicUsize,
}

#[allow(unused)]
impl AtomicCounter {
    pub fn new(val: usize) -> Self {
        Self {
            count: AtomicUsize::new(val),
        }
    }

    pub fn increment(&self) -> usize {
        self.count.fetch_add(1, Ordering::SeqCst) + 1
    }

    pub fn decrement(&self) -> usize {
        self.count.fetch_sub(1, Ordering::SeqCst) - 1
    }

    pub fn value(&self) -> usize {
        self.count.load(Ordering::Acquire)
    }

    pub fn try_increment(&self, max: usize) -> Result<usize, usize> {
        let mut count = self.count.load(Ordering::SeqCst);
        if count < max {
            count = self.increment();
            if count > max {
                self.decrement();
                Err(count)
            } else {
                Ok(count)
            }
        } else {
            Err(count)
        }
    }
}

impl Default for AtomicCounter {
    fn default() -> Self {
        Self::new(0)
    }
}
