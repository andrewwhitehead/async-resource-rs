use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion};

struct Repoll(usize);

impl Future for Repoll {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if self.0 == 0 {
            Poll::Ready(())
        } else {
            self.0 -= 1;
            cx.waker().wake_by_ref();
            Poll::Pending
        }
    }
}

fn test_fut() -> Repoll {
    Repoll(1)
}

fn suspend_block_on(count: usize) {
    for _ in 0..count {
        suspend::block_on(test_fut());
    }
}

fn suspend_block_on_boxed(count: usize) {
    for _ in 0..count {
        suspend::block_on(Box::pin(test_fut()));
    }
}

fn suspend_wait_task(count: usize) {
    for _ in 0..count {
        suspend::Task::from_fut(test_fut()).wait();
    }
}

fn futures_block_on(count: usize) {
    for _ in 0..count {
        futures_executor::block_on(test_fut());
    }
}

fn bench_block_on(c: &mut Criterion) {
    let count = 5000;
    c.bench_with_input(
        BenchmarkId::new("suspend-block_on", count),
        &count,
        |b, &s| {
            b.iter(|| suspend_block_on(s));
        },
    );
    c.bench_with_input(
        BenchmarkId::new("suspend-block_on-boxed", count),
        &count,
        |b, &s| {
            b.iter(|| suspend_block_on_boxed(s));
        },
    );
    c.bench_with_input(
        BenchmarkId::new("suspend-wait-task", count),
        &count,
        |b, &s| {
            b.iter(|| suspend_wait_task(s));
        },
    );
    c.bench_with_input(
        BenchmarkId::new("futures-block_on", count),
        &count,
        |b, &s| {
            b.iter(|| futures_block_on(s));
        },
    );
}

criterion_group!(benches, bench_block_on);
criterion_main!(benches);
