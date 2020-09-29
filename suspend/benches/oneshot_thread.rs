use std::thread;

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion};

use futures_channel::oneshot as futures_oneshot;
use oneshot_rs as oneshot;
use suspend::{block_on, send_once};

fn channel_telephone(threads: usize) {
    let (sender, mut receiver) = send_once();
    for _ in 0..threads {
        let (next_send, next_receive) = send_once();
        thread::spawn(move || {
            let result = receiver.wait().unwrap_or(0);
            next_send.send(result).unwrap();
        });
        receiver = next_receive;
    }
    let testval = 1001u32;
    sender.send(testval).unwrap();
    assert_eq!(receiver.wait(), Ok(testval));
}

fn futures_telephone(threads: usize) {
    let (sender, mut receiver) = futures_oneshot::channel();
    for _ in 0..threads {
        let (next_send, next_receive) = futures_oneshot::channel();
        thread::spawn(move || {
            let result = block_on(receiver).unwrap_or(0);
            next_send.send(result).unwrap();
        });
        receiver = next_receive;
    }
    let testval = 1001u32;
    sender.send(testval).unwrap();
    assert_eq!(block_on(receiver), Ok(testval));
}

fn oneshot_telephone(threads: usize) {
    let (sender, mut receiver) = oneshot::channel();
    for _ in 0..threads {
        let (next_send, next_receive) = oneshot::channel();
        thread::spawn(move || {
            let result = receiver.recv().unwrap_or(0);
            next_send.send(result).unwrap();
        });
        receiver = next_receive;
    }
    let testval = 1001u32;
    sender.send(testval).unwrap();
    assert_eq!(receiver.recv(), Ok(testval));
}

fn bench_telephone(c: &mut Criterion) {
    let count = 1000;
    c.bench_with_input(
        BenchmarkId::new("message-telephone", count),
        &count,
        |b, &s| {
            b.iter(|| channel_telephone(s));
        },
    );
    c.bench_with_input(
        BenchmarkId::new("futures-telephone", count),
        &count,
        |b, &s| {
            b.iter(|| futures_telephone(s));
        },
    );
    c.bench_with_input(
        BenchmarkId::new("oneshot-telephone", count),
        &count,
        |b, &s| {
            b.iter(|| oneshot_telephone(s));
        },
    );
}

criterion_group!(benches, bench_telephone);
criterion_main!(benches);
