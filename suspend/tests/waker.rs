use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

use suspend::waker::{waker_from, /*LocalWakerRef,*/ WakeByRef, Wakeable};

mod utils;
use utils::TestDrop;

struct TestWaker<T> {
    value: T,
    woken: Arc<AtomicUsize>,
}

impl<T> TestWaker<T> {
    fn new(value: T) -> Self {
        Self {
            value,
            woken: Arc::new(AtomicUsize::new(0)),
        }
    }

    fn woken(&self) -> usize {
        self.woken.load(Ordering::Acquire)
    }
}

impl<T: Send + Sync> WakeByRef for TestWaker<T> {
    fn wake_by_ref(&self) {
        self.woken.fetch_add(1, Ordering::SeqCst);
    }
}

impl<T: Clone> Clone for TestWaker<T> {
    fn clone(&self) -> Self {
        Self {
            value: self.value.clone(),
            woken: self.woken.clone(),
        }
    }
}

#[test]
fn waker_from_create_and_wake() {
    let (track, drops) = TestDrop::new_pair();
    let source = TestWaker::new(track);
    let waker = waker_from(source.clone());
    assert_eq!(drops.count(), 0);
    assert_eq!(source.woken(), 0);
    waker.wake();
    assert_eq!(drops.count(), 1);
    assert_eq!(source.woken(), 1);
}

#[test]
fn waker_from_create_and_drop() {
    let (track, drops) = TestDrop::new_pair();
    let source = TestWaker::new(track);
    let waker = waker_from(source.clone());
    assert_eq!(drops.count(), 0);
    assert_eq!(source.woken(), 0);
    drop(waker);
    assert_eq!(drops.count(), 1);
    assert_eq!(source.woken(), 0);
}

#[test]
fn waker_from_clone_wake() {
    let (track, drops) = TestDrop::new_pair();
    let source = TestWaker::new(track);
    let waker = waker_from(source.clone());
    let waker2 = waker.clone();
    waker.wake();
    drop(waker2);
    assert_eq!(drops.count(), 1);
    assert_eq!(source.woken(), 1);
}

#[test]
fn wakeable_create_wake() {
    let (track, drops) = TestDrop::new_pair();
    let source = Wakeable::new(TestWaker::new(track));
    let waker = waker_from(&source);
    waker.wake();
    assert_eq!(drops.count(), 0);
    assert_eq!(source.woken(), 1);
    drop(source);
    assert_eq!(drops.count(), 1);
}

#[test]
fn wakeable_create_wake_by_ref() {
    let (track, drops) = TestDrop::new_pair();
    let source = Wakeable::new(TestWaker::new(track));
    let waker = waker_from(&source);
    waker.wake_by_ref();
    assert_eq!(drops.count(), 0);
    assert_eq!(source.woken(), 1);
    drop(source);
    assert_eq!(drops.count(), 0);
    drop(waker);
    assert_eq!(drops.count(), 1);
}

#[test]
fn wakeable_clone() {
    let (track, drops) = TestDrop::new_pair();
    let source = Wakeable::new(TestWaker::new(track));
    let s2 = source.clone();
    drop(s2);
    assert_eq!(drops.count(), 0);
    assert_eq!(source.woken(), 0);
    drop(source);
    assert_eq!(drops.count(), 1);
}
