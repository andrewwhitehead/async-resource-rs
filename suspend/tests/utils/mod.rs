use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

#[derive(Debug)]
pub struct TestDrop(Arc<AtomicUsize>);

impl TestDrop {
    pub fn new_pair() -> (Self, Drops) {
        let count = Arc::new(AtomicUsize::new(0));
        (Self(count.clone()), Drops(count))
    }
}

impl Clone for TestDrop {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl PartialEq for TestDrop {
    fn eq(&self, other: &Self) -> bool {
        std::ptr::eq(self, other)
    }
}

impl Eq for TestDrop {}

impl Drop for TestDrop {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

pub struct Drops(Arc<AtomicUsize>);

impl Drops {
    pub fn count(&self) -> usize {
        self.0.load(Ordering::Relaxed)
    }
}
