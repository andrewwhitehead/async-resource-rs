use std::thread;

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion};

use futures_channel::oneshot::channel as oneshot;
use futures_executor::block_on;

use arp_channel::dropshot::channel;

fn oneshot_telephone(threads: usize) {
    let (sender, mut receiver) = oneshot();
    for _ in 0..threads {
        let (next_send, next_receive) = oneshot();
        thread::spawn(move || loop {
            if let Ok(Some(result)) = receiver.try_recv() {
                next_send.send(result).unwrap();
                break;
            }
            thread::yield_now();
        });
        receiver = next_receive;
    }
    let testval = 1001u32;
    sender.send(testval).unwrap();
    assert_eq!(block_on(receiver), Ok(testval));
}

fn dropshot_telephone(threads: usize) {
    let (sender, mut receiver) = channel();
    for _ in 0..threads {
        let (next_send, next_receive) = channel();
        thread::spawn(move || loop {
            if let Ok(Some(result)) = receiver.try_recv() {
                next_send.send(result);
                break;
            }
            thread::yield_now();
        });
        receiver = next_receive;
    }
    let testval = 1001u32;
    sender.send(testval);
    assert_eq!(block_on(receiver), Ok(testval));
}

fn dropshot_telephone_block(threads: usize) {
    let (sender, mut receiver) = channel();
    for _ in 0..threads {
        let (next_send, next_receive) = channel();
        thread::spawn(move || {
            let result = receiver.recv().ok().unwrap_or(0);
            next_send.send(result);
        });
        receiver = next_receive;
    }
    let testval = 1001u32;
    sender.send(testval);
    assert_eq!(receiver.recv(), Ok(testval));
}

fn bench_telephone(c: &mut Criterion) {
    let count = 500;
    c.bench_with_input(
        BenchmarkId::new("dropshot-telephone", count),
        &count,
        |b, &s| {
            b.iter(|| dropshot_telephone(s));
        },
    );
    c.bench_with_input(
        BenchmarkId::new("dropshot-telephone-block", count),
        &count,
        |b, &s| {
            b.iter(|| dropshot_telephone_block(s));
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
