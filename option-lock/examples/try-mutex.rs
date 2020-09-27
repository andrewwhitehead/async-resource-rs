use core::ops::{Deref, DerefMut};

use std::{
    sync::{atomic::spin_loop_hint, Arc},
    thread,
};

use option_lock::{OptionGuard, OptionLock};

/// A simple wrapper around an OptionLock which ensures that there
/// is always a stored value
struct TryMutex<T> {
    lock: OptionLock<T>,
}

impl<T> TryMutex<T> {
    pub fn new(value: T) -> Self {
        Self { lock: value.into() }
    }

    pub fn try_lock(&self) -> Option<TryMutexLock<'_, T>> {
        self.lock.try_lock().map(|guard| TryMutexLock { guard })
    }
}

struct TryMutexLock<'a, T> {
    guard: OptionGuard<'a, T>,
}

impl<T> Deref for TryMutexLock<'_, T> {
    type Target = T;
    fn deref(&self) -> &Self::Target {
        self.guard.as_ref().unwrap()
    }
}

impl<T> DerefMut for TryMutexLock<'_, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.guard.as_mut_ref().unwrap()
    }
}

fn main() {
    let shared = Arc::new(TryMutex::new(0i32));
    let threads = 100;
    for _ in 0..threads {
        let shared = shared.clone();
        thread::spawn(move || loop {
            if let Some(mut guard) = shared.try_lock() {
                *guard += 1;
                break;
            }
            spin_loop_hint()
        });
    }
    loop {
        if shared.try_lock().map(|guard| *guard) == Some(threads) {
            break;
        }
        spin_loop_hint()
    }
    println!("Completed {} threads", threads);
}
