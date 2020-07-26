use std::cell::Cell;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use suspend::block_on;

use async_resource::{AcquireError, PoolConfig};

mod utils;
use utils::AtomicCounter;

fn counter_pool_config() -> PoolConfig<usize, ()> {
    let source = Arc::new(AtomicCounter::default());
    PoolConfig::<usize, ()>::new(move || {
        let s = source.clone();
        async move { Ok(s.increment()) }
    })
}

#[test]
fn test_pool_acquire_order_timeout() {
    let disposed = Arc::new(AtomicCounter::default());
    let dcopy = disposed.clone();
    let pool = counter_pool_config()
        .idle_timeout(Duration::from_secs(1))
        .dispose(move |_res, _info| {
            dcopy.increment();
        })
        .build()
        .unwrap();

    block_on(async move {
        let mut fst = pool.acquire().await.unwrap();
        let mut snd = pool.acquire().await.unwrap();
        assert_eq!(*fst, 1);
        assert_eq!(*snd, 2);

        // 2 should be returned directly to the idle queue
        drop(snd);
        snd = pool.acquire().await.unwrap();
        assert_eq!(*snd, 2);

        // 1 should be returned directly to the idle queue
        drop(fst);
        fst = pool.acquire().await.unwrap();
        assert_eq!(*fst, 1);

        // with all resources dropped, shutdown should be quick
        drop(fst);
        drop(snd);
        pool.drain(Duration::from_millis(500)).await.unwrap();

        assert_eq!(disposed.value(), 2);
    })
}

#[test]
fn test_pool_acquire_order_no_timeout() {
    let disposed = Arc::new(AtomicCounter::default());
    let dcopy = disposed.clone();
    let mut pool = counter_pool_config()
        .dispose(move |_res, _| {
            dcopy.increment();
        })
        .build()
        .unwrap();

    block_on(async move {
        let fst = pool.acquire().await.unwrap();
        let snd = pool.acquire().await.unwrap();
        assert_eq!(*fst, 1);
        assert_eq!(*snd, 2);
        drop(snd);

        // when there is no idle timeout and the pool is not busy (no waiters)
        // then 2 should be disposed, not returned to the idle queue
        let trd = pool.acquire().await.unwrap();
        assert_eq!(*trd, 3);

        drop(fst);

        // check the resources we released (1 and 2) have been disposed
        assert_eq!(disposed.value(), 2);

        // shutdown must time out because a resource is held
        pool = pool.drain(Duration::from_millis(50)).await.unwrap_err();

        drop(trd);
        pool.drain(Duration::from_millis(500)).await.unwrap();
    });
}

#[test]
// demonstrate a resource type that is Send but !Sync
fn test_pool_not_sync() {
    let source = Arc::new(AtomicCounter::default());
    let pool = PoolConfig::<Cell<usize>, ()>::new(move || {
        let s = source.clone();
        async move { Ok(Cell::new(s.increment())) }
    })
    .build()
    .unwrap();
    assert_eq!(pool.acquire().wait().unwrap().get(), 1);
}

#[test]
// test support for resource waiters
fn test_pool_waiter() {
    let waiting = Arc::new(AtomicCounter::default());
    let results = Arc::new(Mutex::new(vec![]));
    let pool = counter_pool_config().max_count(1).build().unwrap();
    let p1 = pool.clone();
    let mut waiters = 3;

    // load first resource
    results.lock().unwrap().push(p1.acquire().wait().unwrap());

    // create waiters for the resource
    for _ in 0..waiters {
        let pool = pool.clone();
        let results = results.clone();
        let waiting = waiting.clone();
        thread::spawn(move || {
            let acquire = pool.acquire();
            waiting.increment();
            let result = acquire.wait();
            // intentionally poison mutex on failure (acquire timeout)
            results.lock().unwrap().push(result.unwrap());
            waiting.decrement();
        });
    }

    // spin until waiters are queued up
    loop {
        if waiting.value() == waiters {
            break;
        }
        thread::yield_now();
    }

    // exhaust waiters
    // since the queue is 'busy' from this point and there is no expiry,
    // the same resource will be returned each time
    loop {
        assert_eq!(*results.lock().unwrap().pop().unwrap(), 1);
        waiters -= 1;
        loop {
            if waiting.value() == waiters {
                break;
            }
            // check mutex survives
            assert!(results.lock().unwrap().len() <= 1);
            thread::yield_now();
        }
        if waiters == 0 {
            break;
        }
    }

    let mut res = results.lock().unwrap();
    assert!(*res.pop().unwrap() == 1);
    assert!(res.is_empty());
}

#[test]
// test support for max_waiters setting
fn test_pool_max_waiters() {
    let busy = Arc::new(AtomicCounter::default());
    let done = Arc::new(AtomicCounter::default());
    let wait = Arc::new(AtomicCounter::default());
    let results = Arc::new(Mutex::new(vec![]));
    let pool = counter_pool_config()
        .max_count(1)
        .max_waiters(1)
        .build()
        .unwrap();
    let p1 = pool.clone();

    // load first resource
    results.lock().unwrap().push(p1.acquire().wait().unwrap());

    // create waiters for the resource
    for _ in 0..3 {
        let pool = pool.clone();
        let busy = busy.clone();
        let done = done.clone();
        let wait = wait.clone();
        let results = results.clone();
        thread::spawn(move || {
            let acquire = pool.acquire();
            wait.increment();
            let result = match acquire.wait() {
                Ok(result) => Ok(result),
                Err(AcquireError::PoolBusy) => {
                    busy.increment();
                    done.increment();
                    return;
                }
                Err(other) => Err(other),
            };
            // intentionally poison mutex on failure (acquire timeout, resource error)
            done.increment();
            results.lock().unwrap().push(result.unwrap());
        });
    }

    // spin until waiters are queued up
    loop {
        if wait.value() == 3 && done.value() >= 2 {
            break;
        }
        thread::yield_now();
    }

    assert_eq!(busy.value(), 2);
    assert_eq!(done.value(), 2);

    results.lock().unwrap().is_empty(); // check mutex
}
