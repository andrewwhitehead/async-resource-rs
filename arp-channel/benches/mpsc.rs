use std::collections::BTreeMap;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use std::thread;

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion};
use futures_executor::{block_on, block_on_stream};
use futures_util::stream::StreamExt;

use arp_channel::mpsc::unbounded;


fn send_threaded_iter(threads: usize, items: usize) {
    let (sender, mut receiver) = unbounded::<usize>();
    let mut seen = BTreeMap::<usize, usize>::new();
    for _ in 0..threads {
        let s = sender.clone();
        thread::spawn(move || {
            for i in 0..items {
                s.send(i).unwrap();
            }
        });
    }
    drop(sender);
    for val in receiver.iter() {
        *seen.entry(val).or_default() += 1;
    }
    assert_eq!(seen.len(), items);
    for i in seen.values() {
        assert_eq!(*i, threads);
    }
}

fn send_threaded_await(threads: usize, items: usize) {
    let (sender, mut receiver) = unbounded::<usize>();
    let mut seen = BTreeMap::<usize, usize>::new();
    for _ in 0..threads {
        let s = sender.clone();
        thread::spawn(move || {
            for i in 0..items {
                s.send(i).unwrap();
            }
        });
    }
    drop(sender);
    block_on(async {
        while let Some(val) = receiver.next().await {
            *seen.entry(val).or_default() += 1;
        }
    });
    assert_eq!(seen.len(), items);
    for i in seen.values() {
        assert_eq!(*i, threads);
    }
}

fn send_threaded_stream(threads: usize, items: usize) {
    let (sender, receiver) = unbounded::<usize>();
    let mut seen = BTreeMap::<usize, usize>::new();
    for _ in 0..threads {
        let s = sender.clone();
        thread::spawn(move || {
            for i in 0..items {
                s.send(i).unwrap();
            }
        });
    }
    drop(sender);
    for val in block_on_stream(receiver) {
        *seen.entry(val).or_default() += 1;
    }
    assert_eq!(seen.len(), items);
    for i in seen.values() {
        assert_eq!(*i, threads);
    }
}

#[cfg(feature = "sink")]
fn send_threaded_sink(threads: usize, items: usize) {
    let (sender, receiver) = unbounded::<usize>();
    let mut seen = BTreeMap::<usize, usize>::new();
    for _ in 0..threads {
        let s = sender.clone();
        thread::spawn(move || {
            let stream = futures_util::stream::iter((0..items).map(Ok));
            block_on(async {
                stream.forward(s).await.unwrap();
            });
        });
    }
    drop(sender);
    for val in block_on_stream(receiver) {
        *seen.entry(val).or_default() += 1;
    }
    assert_eq!(seen.len(), items);
    for i in seen.values() {
        assert_eq!(*i, threads);
    }
}

fn send_threaded_native(threads: usize, items: usize) {
    let (sender, receiver) = std::sync::mpsc::channel::<usize>();
    let mut seen = BTreeMap::<usize, usize>::new();
    for _ in 0..threads {
        let s = sender.clone();
        thread::spawn(move || {
            for i in 0..items {
                s.send(i).unwrap();
            }
        });
    }
    drop(sender);
    for val in receiver.iter() {
        *seen.entry(val).or_default() += 1;
    }
    assert_eq!(seen.len(), items);
    for i in seen.values() {
        assert_eq!(*i, threads);
    }
}

fn send_threaded_futures(threads: usize, items: usize) {
    let (sender, mut receiver) = futures_channel::mpsc::unbounded::<usize>();
    let mut seen = BTreeMap::<usize, usize>::new();
    for _ in 0..threads {
        let s = sender.clone();
        thread::spawn(move || {
            for i in 0..items {
                s.unbounded_send(i).unwrap();
            }
        });
    }
    drop(sender);
    block_on(async {
        while let Some(val) = receiver.next().await {
            *seen.entry(val).or_default() += 1;
        }
    });
    assert_eq!(seen.len(), items);
    for i in seen.values() {
        assert_eq!(*i, threads);
    }
}

fn send_threaded_conc(threads: usize, items: usize) {
    let queue = Arc::new(concurrent_queue::ConcurrentQueue::unbounded());
    let mut seen = BTreeMap::<usize, usize>::new();
    let done = Arc::new(AtomicUsize::new(0));
    for _ in 0..threads {
        let q = queue.clone();
        let d = done.clone();
        thread::spawn(move || {
            for i in 0..items {
                q.push(i).unwrap();
            }
            d.fetch_add(1, Ordering::SeqCst);
        });
    }
    let mut last = false;
    loop {
        while let Ok(val) = queue.pop() {
            *seen.entry(val).or_default() += 1;
        }
        if last {
            break;
        } else if done.load(Ordering::Acquire) == threads {
            last = true;
        } else {
            thread::yield_now();
        }
    }
    assert_eq!(seen.len(), items);
    for i in seen.values() {
        assert_eq!(*i, threads);
    }
}

fn bench_compare(c: &mut Criterion) {
    let mut group = c.benchmark_group("Speed test");
    for i in [(10, 100), (100, 100)].iter() {
        let id = format!("{}/{}", i.0, i.1);
        group.bench_with_input(BenchmarkId::new("Mpsc-Iter", &id), i, |b, i| {
            b.iter(|| send_threaded_iter(i.0, i.1))
        });
        group.bench_with_input(BenchmarkId::new("Mpsc-Await", &id), i, |b, i| {
            b.iter(|| send_threaded_await(i.0, i.1))
        });
        group.bench_with_input(BenchmarkId::new("Mpsc-Stream", &id), i, |b, i| {
            b.iter(|| send_threaded_stream(i.0, i.1))
        });
        #[cfg(feature = "sink")]
        group.bench_with_input(BenchmarkId::new("Mpsc-Sink", &id), i, |b, i| {
            b.iter(|| send_threaded_sink(i.0, i.1))
        });
        group.bench_with_input(BenchmarkId::new("Native", &id), i, |b, i| {
            b.iter(|| send_threaded_native(i.0, i.1))
        });
        group.bench_with_input(BenchmarkId::new("Futures", &id), i, |b, i| {
            b.iter(|| send_threaded_futures(i.0, i.1))
        });
        group.bench_with_input(BenchmarkId::new("Conc-Queue", &id), i, |b, i| {
            b.iter(|| send_threaded_conc(i.0, i.1))
        });
    }
    group.finish();
}

criterion_group!(benches, bench_compare);
criterion_main!(benches);
