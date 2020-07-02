use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use std::thread;

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion};

use arp_channel::OptionLock;

fn lock_contention(threads: usize) {
    let lock = Arc::new(OptionLock::new(None));
    let done = Arc::new(AtomicUsize::new(0));
    for _ in 0..threads {
        let done = done.clone();
        let lock = lock.clone();
        thread::spawn(move || {
            let val = loop {
                match lock.try_take() {
                    Some(val) => break val,
                    None => thread::yield_now(),
                }
            };
            done.fetch_add(val, Ordering::SeqCst);
        });
    }
    let mut expected = 0;
    for val in 0..threads {
        expected += val;
        loop {
            let done = if let Some(mut guard) = lock.try_lock() {
                if guard.is_none() {
                    guard.replace(val);
                    true
                } else {
                    false
                }
            } else {
                false
            };
            thread::yield_now();
            if done {
                break;
            }
        }
    }
    loop {
        if done.load(Ordering::Acquire) == expected {
            break;
        }
        thread::yield_now();
    }
}

fn bench_contention(c: &mut Criterion) {
    let count = 500;
    c.bench_with_input(
        BenchmarkId::new("lock_contention", count),
        &count,
        |b, &s| {
            b.iter(|| lock_contention(s));
        },
    );
}

criterion_group!(benches, bench_contention);
criterion_main!(benches);
