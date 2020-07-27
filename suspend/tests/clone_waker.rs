use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

use suspend::{
    local_waker,
    waker::{waker_from, CloneWake, LocalWaker},
};

use pin_utils::pin_mut;

struct TestWaker {
    cloned: Arc<AtomicUsize>,
    dropped: Arc<AtomicUsize>,
    woken: Arc<AtomicUsize>,
}

impl TestWaker {
    fn new() -> Self {
        Self {
            cloned: Arc::new(AtomicUsize::new(0)),
            dropped: Arc::new(AtomicUsize::new(0)),
            woken: Arc::new(AtomicUsize::new(0)),
        }
    }

    fn cloned(&self) -> usize {
        self.cloned.load(Ordering::Acquire)
    }

    fn dropped(&self) -> usize {
        self.dropped.load(Ordering::Acquire)
    }

    fn woken(&self) -> usize {
        self.woken.load(Ordering::Acquire)
    }
}

impl CloneWake for TestWaker {
    fn wake_by_ref(&self) {
        self.woken.fetch_add(1, Ordering::SeqCst);
    }
}

impl Clone for TestWaker {
    fn clone(&self) -> Self {
        self.cloned.fetch_add(1, Ordering::SeqCst);
        Self {
            cloned: self.cloned.clone(),
            dropped: self.dropped.clone(),
            woken: self.woken.clone(),
        }
    }
}

impl Drop for TestWaker {
    fn drop(&mut self) {
        self.dropped.fetch_add(1, Ordering::SeqCst);
    }
}

#[test]
fn test_create_wake() {
    let first = TestWaker::new();
    let waker = waker_from(first.clone());
    assert_eq!(first.cloned(), 1);
    assert_eq!(first.dropped(), 0);
    assert_eq!(first.woken(), 0);
    waker.wake();
    assert_eq!(first.cloned(), 1);
    assert_eq!(first.dropped(), 1);
    assert_eq!(first.woken(), 1);
}

#[test]
fn test_local_create_wake() {
    let first = TestWaker::new();
    pin_mut!(first);
    let waker = LocalWaker::new(first.as_mut());
    assert_eq!(first.cloned(), 0);
    assert_eq!(first.dropped(), 0);
    assert_eq!(first.woken(), 0);
    let w = waker.clone();
    assert_eq!(first.cloned(), 1);
    assert_eq!(first.dropped(), 0);
    assert_eq!(first.woken(), 0);
    w.wake();
    assert_eq!(first.cloned(), 1);
    assert_eq!(first.dropped(), 1);
    assert_eq!(first.woken(), 1);
}

#[test]
fn test_local_macro() {
    let first = TestWaker::new();
    let _ = {
        local_waker!(waker, first.clone());
        assert_eq!(first.cloned(), 1);
        assert_eq!(first.dropped(), 0);
        assert_eq!(first.woken(), 0);
        let w = waker.clone();
        assert_eq!(first.cloned(), 2);
        assert_eq!(first.dropped(), 0);
        assert_eq!(first.woken(), 0);
        w.wake();
        assert_eq!(first.cloned(), 2);
        assert_eq!(first.dropped(), 1);
        assert_eq!(first.woken(), 1);
    };
    assert_eq!(first.cloned(), 2);
    assert_eq!(first.dropped(), 2);
    assert_eq!(first.woken(), 1);
}
