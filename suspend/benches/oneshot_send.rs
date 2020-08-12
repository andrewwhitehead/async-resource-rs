use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion};

use futures_channel::oneshot as futures_oneshot;
use oneshot_rs as oneshot;
use suspend::{block_on, message_task, Suspend};

fn message_many(count: usize, drop_recv: bool) {
    let mut sus = Suspend::new();
    for _ in 0..count {
        let (sender, mut receiver) = message_task();
        sus.poll_unpin(&mut receiver).unwrap_err();
        if drop_recv {
            drop(receiver);
            sender.send(1).unwrap_err();
        } else {
            sender.send(1).unwrap();
            receiver.wait().unwrap();
        }
    }
}

fn futures_many(count: usize, drop_recv: bool) {
    let mut sus = Suspend::new();
    for _ in 0..count {
        let (sender, mut receiver) = futures_oneshot::channel();
        sus.poll_unpin(&mut receiver).unwrap_err();
        if drop_recv {
            drop(receiver);
            sender.send(1).unwrap_err();
        } else {
            sender.send(1).unwrap();
            block_on(receiver).unwrap();
        }
    }
}

fn oneshot_many(count: usize, drop_recv: bool) {
    let mut sus = Suspend::new();
    for _ in 0..count {
        let (sender, mut receiver) = oneshot::channel();
        sus.poll_unpin(&mut receiver).unwrap_err();
        if drop_recv {
            drop(receiver);
            sender.send(1).unwrap_err();
        } else {
            sender.send(1).unwrap();
            receiver.recv().unwrap();
        }
    }
}

fn bench_many(c: &mut Criterion) {
    let count = 5000;
    c.bench_with_input(BenchmarkId::new("message-many", count), &count, |b, &s| {
        b.iter(|| message_many(s, false));
    });
    c.bench_with_input(BenchmarkId::new("futures-many", count), &count, |b, &s| {
        b.iter(|| futures_many(s, false));
    });
    c.bench_with_input(BenchmarkId::new("oneshot-many", count), &count, |b, &s| {
        b.iter(|| oneshot_many(s, false));
    });
}

fn bench_many_drop_recv(c: &mut Criterion) {
    let count = 5000;
    c.bench_with_input(
        BenchmarkId::new("message-many-drop", count),
        &count,
        |b, &s| {
            b.iter(|| message_many(s, true));
        },
    );
    c.bench_with_input(
        BenchmarkId::new("futures-many-drop", count),
        &count,
        |b, &s| {
            b.iter(|| futures_many(s, true));
        },
    );
    c.bench_with_input(
        BenchmarkId::new("oneshot-many-drop", count),
        &count,
        |b, &s| {
            b.iter(|| oneshot_many(s, true));
        },
    );
}

criterion_group!(benches, bench_many, bench_many_drop_recv);
criterion_main!(benches);
