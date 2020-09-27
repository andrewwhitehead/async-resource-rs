use futures_core::FusedStream;
use futures_lite::{pin, StreamExt};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use suspend::{async_stream::make_stream, block_on, stream, try_stream};

#[test]
fn non_macro() {
    let s = make_stream(|mut sender| async move {
        sender.send(1u32).await;
        sender.send(2u32).await;
        sender.send(3u32).await;
    });
    pin!(s);
    assert_eq!(block_on(s.next()), Some(1));
    assert_eq!(block_on(s.next()), Some(2));
    assert_eq!(block_on(s.next()), Some(3));
    assert_eq!(block_on(s.next()), None);
    assert_eq!(block_on(s.next()), None);
}

#[test]
fn basic_stream() {
    let s = stream! {
        send!(1);
        send!(2);
        send!(3);
    };
    pin!(s);
    assert_eq!(block_on(s.next()), Some(1));
    assert_eq!(block_on(s.next()), Some(2));
    assert_eq!(block_on(s.next()), Some(3));
    assert_eq!(block_on(s.next()), None);
    assert_eq!(block_on(s.next()), None);
}

#[test]
fn empty_stream() {
    let mut ran = false;
    {
        let r = &mut ran;
        let s = stream! {
            *r = true;
        };
        pin!(s);
        assert_eq!(block_on(s.next()), Option::<i32>::None);
    }
    assert!(ran);
}

#[test]
fn nest_stream() {
    let s = stream! {
        let s2 = stream! {
            send!(1);
            send!(2);
        };
        pin!(s2);
        while let Some(item) = s2.next().await {
            send!(item);
        }
        send!(3);
    };
    pin!(s);
    assert_eq!(block_on(s.next()), Some(1));
    assert_eq!(block_on(s.next()), Some(2));
    assert_eq!(block_on(s.next()), Some(3));
    assert_eq!(block_on(s.next()), None);
    assert_eq!(block_on(s.next()), None);
}

#[test]
fn basic_try_stream() {
    let s = try_stream! {
        send!(1);
        send!(2);
        send!(3);
        Result::<_, ()>::Ok(())
    };
    pin!(s);
    assert_eq!(block_on(s.next()), Some(Ok(1)));
    assert_eq!(block_on(s.next()), Some(Ok(2)));
    assert_eq!(block_on(s.next()), Some(Ok(3)));
    assert_eq!(block_on(s.next()), None);
    assert_eq!(block_on(s.next()), None);
}

#[test]
fn try_stream_fail() {
    let s = try_stream! {
        send!(1);
        Err(2)?;
        send!(3);
        Ok(())
    };
    pin!(s);
    assert_eq!(block_on(s.next()), Some(Ok(1)));
    assert_eq!(block_on(s.next()), Some(Err(2)));
    assert_eq!(block_on(s.next()), None);
    assert_eq!(block_on(s.next()), None);
}

#[test]
fn nest_try_stream() {
    let s = try_stream! {
        let s2 = try_stream! {
            send!(1);
            Err(2)?;
            Result::<_, i32>::Ok(())
        };
        pin!(s2);
        while let Some(item) = s2.next().await {
            send!(item?);
        }
        send!(3);
        Ok(())
    };
    pin!(s);
    assert_eq!(block_on(s.next()), Some(Ok(1)));
    assert_eq!(block_on(s.next()), Some(Err(2)));
    assert_eq!(block_on(s.next()), None);
    assert_eq!(block_on(s.next()), None);
}

#[derive(Clone)]
struct CheckDrop(Arc<AtomicUsize>);

impl CheckDrop {
    pub fn new() -> Self {
        Self(Arc::new(AtomicUsize::new(0)))
    }

    pub fn count(&self) -> usize {
        self.0.load(Ordering::Relaxed)
    }
}

impl Drop for CheckDrop {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::Release);
    }
}

#[test]
fn test_drop_unpolled() {
    let chk = CheckDrop::new();
    {
        let chk2 = chk.clone();
        let s = stream! {
            send!(1i32);
            drop(chk2);
        };
        assert_eq!(s.is_terminated(), false);
        assert_eq!(chk.count(), 0);
    }
    assert_eq!(chk.count(), 1);
}

#[test]
fn test_drop_polled() {
    let chk = CheckDrop::new();
    {
        let chk2 = chk.clone();
        let s = stream! {
            send!(1i32);
            send!(2i32);
            drop(chk2);
        };
        pin!(s);
        assert_eq!(block_on(s.next()), Some(1));
        assert_eq!(s.is_terminated(), false);
        assert_eq!(chk.count(), 0);
    }
    assert_eq!(chk.count(), 1);
}

#[test]
fn test_drop_complete() {
    let chk = CheckDrop::new();
    {
        let chk2 = chk.clone();
        let s = stream! {
            send!(1i32);
            drop(chk2);
        };
        pin!(s);
        assert_eq!(block_on(s.next()), Some(1));
        assert_eq!(chk.count(), 0);
        assert_eq!(block_on(s.next()), None);
        assert_eq!(s.is_terminated(), true);
        assert_eq!(chk.count(), 1);
    }
    assert_eq!(chk.count(), 1);
}
