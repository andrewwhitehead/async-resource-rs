use std::sync::{
    atomic::{spin_loop_hint, AtomicUsize, Ordering},
    Arc,
};
use std::thread;

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion};

use option_lock::OptionLock;

// these tests are used mainly to check that no deadlocks occur with many threads

fn lock_contention_yield(threads: usize) {
    let lock = Arc::new(OptionLock::new());
    let done = Arc::new(AtomicUsize::new(0));
    for _ in 0..threads - 1 {
        let done = done.clone();
        let lock = lock.clone();
        thread::spawn(move || {
            let val = loop {
                if let Ok(val) = lock.try_take() {
                    break val;
                }
                loop {
                    thread::yield_now();
                    if lock.status().can_take() {
                        break;
                    }
                }
            };
            done.fetch_add(val, Ordering::AcqRel);
        });
    }
    let mut expected = 0;
    for val in 0..threads - 1 {
        expected += val;
        loop {
            if let Ok(mut guard) = lock.try_lock() {
                if guard.is_none() {
                    guard.replace(val);
                    break;
                }
            }
            loop {
                thread::yield_now();
                if !lock.status().is_locked() {
                    break;
                }
            }
        }
    }
    loop {
        if done.load(Ordering::Relaxed) == expected {
            break;
        }
        thread::yield_now();
    }
}

// this can take a very long time if threads > # cpu cores
fn lock_contention_spin(threads: usize) {
    let lock = Arc::new(OptionLock::new());
    let done = Arc::new(AtomicUsize::new(0));
    for _ in 0..threads - 1 {
        let done = done.clone();
        let lock = lock.clone();
        thread::spawn(move || {
            let val = lock.spin_take();
            done.fetch_add(val, Ordering::AcqRel);
        });
    }
    let mut expected = 0;
    for val in 0..threads - 1 {
        expected += val;
        loop {
            let mut guard = lock.spin_lock();
            if guard.is_none() {
                guard.replace(val);
                break;
            }
        }
    }
    while done.load(Ordering::Relaxed) != expected {
        spin_loop_hint();
    }
}

fn bench_contention(c: &mut Criterion) {
    let yield_thread_count = 500;
    c.bench_with_input(
        BenchmarkId::new("lock_contention_yield", yield_thread_count),
        &yield_thread_count,
        |b, &s| {
            b.iter(|| lock_contention_yield(s));
        },
    );

    let spin_thread_count = 8;
    c.bench_with_input(
        BenchmarkId::new("lock_contention_spin", spin_thread_count),
        &spin_thread_count,
        |b, &s| {
            b.iter(|| lock_contention_spin(s));
        },
    );
}

criterion_group!(benches, bench_contention);
criterion_main!(benches);
