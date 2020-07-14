use std::cell::Cell;
use std::sync::Arc;

use futures_executor::block_on;

use arp::PoolConfig;

mod utils;
use utils::AtomicCounter;

fn counter_pool() -> PoolConfig<usize, ()> {
    let source = Arc::new(AtomicCounter::default());
    PoolConfig::<usize, ()>::new(move || {
        let s = source.clone();
        async move { Ok(s.increment()) }
    })
}

#[test]
fn test_pool_acquire_order() {
    let pool = counter_pool().build();
    let next = || pool.acquire();
    block_on(async move {
        let mut fst = next().await.unwrap();
        let mut snd = next().await.unwrap();
        assert_eq!(*fst, 1);
        assert_eq!(*snd, 2);
        drop(snd);
        snd = next().await.unwrap();
        assert_eq!(*snd, 2);
        drop(fst);
        fst = next().await.unwrap();
        assert_eq!(*fst, 1);
    })
}

#[test]
fn test_pool_dispose() {
    let disposed = Arc::new(AtomicCounter::default());
    let dcopy = disposed.clone();
    let pool = counter_pool()
        .dispose(move |res, _| {
            let d = dcopy.clone();
            println!("dispose! {}", res);
            async move {
                d.increment();
                Ok(())
            }
        })
        .build();
    block_on(async move {
        pool.acquire().await.unwrap();
        //pool.drain().await;
    });
    assert_eq!(disposed.value(), 1);
}

#[test]
// demonstrate a resource type that is Send but !Sync
fn test_pool_not_sync() {
    let source = Arc::new(AtomicCounter::default());
    let pool = PoolConfig::<Cell<usize>, ()>::new(move || {
        let s = source.clone();
        async move { Ok(Cell::new(s.increment())) }
    })
    .build();
    block_on(async move {
        assert_eq!(pool.acquire().await.unwrap().get(), 1);
    });
}
