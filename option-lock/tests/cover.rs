use option_lock::*;

#[test]
fn option_lock_exclusive() {
    let a = OptionLock::from(1);
    assert!(!a.status().is_locked());
    let mut guard = a.try_lock().unwrap();
    assert!(a.status().is_locked());
    assert!(a.try_lock().is_err());
    assert_eq!(a.try_take(), Err(ReadError::Locked));
    assert_eq!(a.try_read(), Err(ReadError::Locked));

    assert_eq!(*guard, Some(1));
    guard.replace(2);
    drop(guard);
    assert_eq!(*a.try_lock().unwrap(), Some(2));
    assert!(!a.status().is_locked());
}

#[test]
fn option_lock_read() {
    let a = OptionLock::from(99);
    assert_eq!(a.status(), Status::Available);
    assert!(!a.status().is_locked());
    assert!(a.status().can_read());
    assert!(a.status().can_take());
    assert_eq!(a.status().readers(), 0);

    let read = a.try_read().unwrap();
    assert_eq!(a.status(), Status::ReadLock(1));
    assert!(!a.status().is_locked());
    assert!(a.status().can_read());
    assert!(!a.status().can_take());
    assert_eq!(a.status().readers(), 1);
    assert_eq!(*read, 99);

    assert!(a.try_lock().is_err());
    assert_eq!(a.try_take(), Err(ReadError::Locked));

    let read2 = a.try_read().unwrap();
    assert_eq!(a.status(), Status::ReadLock(2));
    assert!(!a.status().is_locked());
    assert!(a.status().can_read());
    assert!(!a.status().can_take());
    assert_eq!(a.status().readers(), 2);
    assert_eq!(*read2, 99);

    drop(read2);
    assert_eq!(a.status().readers(), 1);

    drop(read);
    assert_eq!(a.status(), Status::Available);

    assert_eq!(a.try_take(), Ok(99));
    assert_eq!(a.status(), Status::Empty);
    assert_eq!(a.try_read(), Err(ReadError::Empty));
}

#[test]
fn option_lock_read_write() {
    // test a successful write after a failed read
    let a = OptionLock::new();
    assert_eq!(a.try_read(), Err(ReadError::Empty));
    let mut write = a.try_lock().unwrap();
    write.replace(5);
    drop(write);
    assert_eq!(*a.try_read().unwrap(), 5);
}

#[test]
fn option_lock_upgrade() {
    let a = OptionLock::from(61);
    let read = a.try_read().unwrap();
    let write = OptionRead::try_lock(read).unwrap();
    assert!(a.status().is_locked());
    drop(write);
    assert_eq!(a.status(), Status::Available);
    drop(a);

    let b = OptionLock::from(5);
    let read1 = b.try_read().unwrap();
    let read2 = b.try_read().unwrap();
    assert_eq!(b.status().readers(), 2);
    let read3 = OptionRead::try_lock(read1).unwrap_err();
    assert!(!b.status().is_locked());
    assert_eq!(b.status().readers(), 2);
    drop(read3);
    assert_eq!(b.status().readers(), 1);
    drop(read2);
    assert_eq!(b.status(), Status::Available);
}

#[test]
fn option_lock_downgrade() {
    let a = OptionLock::<u32>::new();
    let write = a.try_lock().unwrap();
    assert_eq!(OptionGuard::downgrade(write), None);
    drop(a);

    let b = OptionLock::new();
    let mut write = b.try_lock().unwrap();
    write.replace(20);
    let read = OptionGuard::downgrade(write).unwrap();
    assert_eq!(b.status(), Status::ReadLock(1));
    assert_eq!(*read, 20);
    drop(read);
    assert_eq!(b.status(), Status::Available);
}

#[test]
fn option_lock_take() {
    let a = OptionLock::<u32>::new();
    assert_eq!(a.try_take(), Err(ReadError::Empty));
    drop(a);

    let b = OptionLock::from(101);
    assert_eq!(b.try_take(), Ok(101));
    drop(b);

    let c = OptionLock::from(42);
    let read = c.try_read().unwrap();
    assert_eq!(OptionRead::try_take(read), Ok(42));
    drop(c);

    let d = OptionLock::from(11);
    let read1 = d.try_read().unwrap();
    let read2 = d.try_read().unwrap();
    assert!(OptionRead::try_take(read2).is_err());
    assert_eq!(OptionRead::try_take(read1), Ok(11));
}

#[test]
fn option_lock_debug() {
    assert_eq!(
        format!("{:?}", &OptionLock::<i32>::new()),
        "OptionLock { status: Empty }"
    );
    assert_eq!(
        format!("{:?}", &OptionLock::from(1)),
        "OptionLock { status: Available }"
    );

    let lock = OptionLock::from(1);
    let read = lock.try_read().unwrap();
    assert_eq!(format!("{:?}", &read), "1");
    assert_eq!(
        format!("{:#?}", &read).replace('\n', "").replace(' ', ""),
        "OptionRead(1,)"
    );
    assert_eq!(format!("{:?}", &lock), "OptionLock { status: ReadLock(1) }");
    drop(read);

    let guard = lock.try_lock().unwrap();
    assert_eq!(format!("{:?}", &guard), "Some(1)");
    assert_eq!(
        format!("{:#?}", &guard).replace('\n', "").replace(' ', ""),
        "OptionGuard(Some(1,),)"
    );
    assert_eq!(
        format!("{:?}", &lock),
        "OptionLock { status: ExclusiveLock }"
    );
}
