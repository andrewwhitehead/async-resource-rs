use option_lock::*;

#[test]
fn option_lock_guard() {
    let a = OptionLock::from(1);
    assert!(!a.is_locked());
    let mut guard = a.try_lock().unwrap();
    assert!(a.is_locked());
    assert!(a.try_lock().is_none());
    assert_eq!(a.try_take(), None);

    assert_eq!(guard.as_ref(), Some(&1));
    guard.replace(2);
    drop(guard);
    assert_eq!(a.try_lock().unwrap().as_ref(), Some(&2));
    assert!(!a.is_locked());
}

#[test]
fn option_lock_take() {
    let a = OptionLock::<u32>::empty();
    assert_eq!(a.try_take(), None);
    drop(a);

    let b = OptionLock::from(101);
    assert_eq!(b.try_take(), Some(101));
    drop(b);
}

#[test]
fn option_lock_debug() {
    assert_eq!(
        format!("{:?}", &OptionLock::<i32>::empty()),
        "OptionLock(None)"
    );
    assert_eq!(format!("{:?}", &OptionLock::from(1)), "OptionLock(Some)");

    let lock = OptionLock::from(1);
    let guard = lock.try_lock().unwrap();
    assert_eq!(format!("{:?}", &guard), "OptionGuard(Some(1))");
    assert_eq!(format!("{:?}", &lock), "OptionLock(Locked)");
}

#[test]
fn option_lock_drop() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static DROPPED: AtomicUsize = AtomicUsize::new(0);

    struct DropCheck;

    impl DropCheck {
        fn count() -> usize {
            DROPPED.load(Ordering::Relaxed)
        }
    }

    impl Drop for DropCheck {
        fn drop(&mut self) {
            DROPPED.fetch_add(1, Ordering::Release);
        }
    }

    let lock = OptionLock::<DropCheck>::empty();
    drop(lock);
    assert_eq!(DropCheck::count(), 0);

    let lock = OptionLock::from(DropCheck);
    drop(lock);
    assert_eq!(DropCheck::count(), 1);

    let lock = OptionLock::empty();
    let mut guard = lock.try_lock().unwrap();
    assert_eq!(DropCheck::count(), 1);
    guard.replace(DropCheck);
    drop(guard);
    assert_eq!(DropCheck::count(), 1);
    drop(lock);
    assert_eq!(DropCheck::count(), 2);
}

#[test]
fn owned_take() {
    let mut lock1 = OptionLock::<()>::empty();
    assert_eq!(lock1.take(), None);

    let mut lock2 = OptionLock::new(98u32);
    assert_eq!(lock2.take(), Some(98u32));

    let lock3 = OptionLock::new(99u32);
    let val: Option<u32> = lock3.into();
    assert_eq!(val, Some(99u32));
}

#[test]
fn owned_replace() {
    let mut lock1 = OptionLock::<()>::empty();
    assert_eq!(lock1.replace(()), None);
    assert_eq!(lock1.replace(()), Some(()));

    let mut lock2 = OptionLock::new(18);
    assert_eq!(lock2.replace(19), Some(18));
    assert_eq!(lock2.replace(20), Some(19));
}

#[test]
fn once_cell() {
    struct MyStruct {
        param: OnceCell<i32>,
    }

    impl MyStruct {
        pub fn new(value: i32) -> Self {
            let slf = Self {
                param: Default::default(),
            };
            slf.param.set(value);
            slf
        }

        pub fn get(&self) -> &i32 {
            self.param.get().unwrap()
        }
    }

    let s = MyStruct::new(156);
    assert_eq!(*s.get(), 156);
    assert_eq!(format!("{:?}", &s.param), "OnceCell(Some(156))");
}
