use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion};

use futures_channel::oneshot as futures_oneshot;
use oneshot_rs as oneshot;
use suspend::{channel, Suspend};

fn channel_many(count: usize) {
    let mut sus = Suspend::new();
    for _ in 0..count {
        let (sender, mut receiver) = channel();
        sus.poll_unpin(&mut receiver).unwrap_err();
        drop(receiver);
        sender.send(1).unwrap_err();
    }
}

fn futures_many(count: usize) {
    let mut sus = Suspend::new();
    for _ in 0..count {
        let (sender, mut receiver) = futures_oneshot::channel();
        sus.poll_unpin(&mut receiver).unwrap_err();
        drop(receiver);
        sender.send(1).unwrap_err();
    }
}

fn oneshot_many(count: usize) {
    let mut sus = Suspend::new();
    for _ in 0..count {
        let (sender, mut receiver) = oneshot::channel();
        sus.poll_unpin(&mut receiver).unwrap_err();
        drop(receiver);
        sender.send(1).unwrap_err();
    }
}

fn bench_many(c: &mut Criterion) {
    let count = 5000;
    c.bench_with_input(BenchmarkId::new("channel-many", count), &count, |b, &s| {
        b.iter(|| channel_many(s));
    });
    c.bench_with_input(BenchmarkId::new("futures-many", count), &count, |b, &s| {
        b.iter(|| futures_many(s));
    });
    c.bench_with_input(BenchmarkId::new("oneshot-many", count), &count, |b, &s| {
        b.iter(|| oneshot_many(s));
    });
}

criterion_group!(benches, bench_many);
criterion_main!(benches);
