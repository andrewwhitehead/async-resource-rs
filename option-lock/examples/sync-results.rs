use option_lock::OptionLock;
use std::sync::{
    atomic::{spin_loop_hint, AtomicUsize, Ordering},
    Arc,
};
use std::thread;

#[derive(Debug)]
struct Results<T> {
    data: Vec<OptionLock<T>>,
    completed: AtomicUsize,
    iter_index: usize,
}

impl<T> Results<T> {
    pub fn new(size: usize) -> Self {
        let mut data = Vec::with_capacity(size);
        data.resize_with(size, OptionLock::default);
        Self {
            data,
            completed: AtomicUsize::default(),
            iter_index: 0,
        }
    }

    pub fn completed(&self) -> bool {
        self.completed.load(Ordering::Relaxed) == self.data.len()
    }

    pub fn return_result(&self, index: usize, value: T) {
        if let Some(mut guard) = self.data.get(index).and_then(OptionLock::try_lock_none) {
            guard.replace(value);
            self.completed.fetch_add(1, Ordering::Release);
        } else {
            panic!("Update conflict");
        }
    }
}

impl<T> Iterator for Results<T> {
    type Item = T;

    fn next(&mut self) -> Option<Self::Item> {
        let idx = self.iter_index;
        if idx < self.data.len() {
            self.iter_index += 1;
            Some(self.data[idx].take().unwrap())
        } else {
            None
        }
    }
}

fn main() {
    let count = 10;
    let res = Arc::new(Results::new(count));
    for index in 0..count {
        let res = res.clone();
        thread::spawn(move || {
            res.return_result(index, index * 2);
        });
    }
    loop {
        if res.completed() {
            break;
        }
        spin_loop_hint();
    }
    let res = Arc::try_unwrap(res).expect("Error unwrapping arc");
    let mut total = 0;
    for item in res {
        total += item;
    }
    assert_eq!(total, 90);
    println!("Completed");
}
