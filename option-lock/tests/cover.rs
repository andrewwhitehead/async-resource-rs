use option_lock::*;

#[test]
fn option_lock_exclusive() {
    let a = OptionLock::from(1);
    assert!(!a.status().is_locked());
    let mut guard = a.try_lock().unwrap();
    assert!(a.status().is_locked());
    assert!(a.try_lock().is_err());
    assert_eq!(a.try_take(), None);
    assert_eq!(a.read(), Err(ReadError::Locked));

    assert_eq!(*guard, Some(1));
    guard.replace(2);
    drop(guard);
    assert_eq!(*a.try_lock().unwrap(), Some(2));
    assert!(!a.status().is_locked());
}

#[test]
fn option_lock_read() {
    let a = OptionLock::from(99);
    assert_eq!(a.status(), Status::Some);
    assert!(!a.status().is_locked());
    assert!(a.status().is_some());
    assert_eq!(a.status().readers(), 0);

    let read = a.read().unwrap();
    assert_eq!(a.status(), Status::ReadLock(1));
    assert!(!a.status().is_locked());
    assert!(a.status().is_some());
    assert_eq!(a.status().readers(), 1);
    assert_eq!(*read, 99);

    assert!(a.try_lock().is_err());
    assert_eq!(a.try_take(), None);

    let read2 = a.read().unwrap();
    assert_eq!(a.status(), Status::ReadLock(2));
    assert!(!a.status().is_locked());
    assert!(a.status().is_some());
    assert_eq!(a.status().readers(), 2);
    assert_eq!(*read2, 99);

    drop(read2);
    assert_eq!(a.status().readers(), 1);

    drop(read);
    assert_eq!(a.status(), Status::Some);

    assert_eq!(a.try_take(), Some(99));
    assert_eq!(a.status(), Status::None);
    assert_eq!(a.read(), Err(ReadError::Empty));
}

#[test]
fn option_lock_read_write() {
    // test a successful write after a failed read
    let a = OptionLock::new();
    assert_eq!(a.read(), Err(ReadError::Empty));
    let mut write = a.try_lock().unwrap();
    write.replace(5);
    drop(write);
    assert_eq!(*a.read().unwrap(), 5);
}

#[test]
fn option_lock_upgrade() {
    let a = OptionLock::from(61);
    let read = a.read().unwrap();
    let write = a.try_upgrade(read).unwrap();
    assert!(a.status().is_locked());
    drop(write);
    assert_eq!(a.status(), Status::Some);
    drop(a);

    let b = OptionLock::from(5);
    let read1 = b.read().unwrap();
    let read2 = b.read().unwrap();
    assert_eq!(b.status().readers(), 2);
    let read3 = b.try_upgrade(read1).unwrap_err();
    assert!(!b.status().is_locked());
    assert_eq!(b.status().readers(), 2);
    drop(read3);
    assert_eq!(b.status().readers(), 1);
    drop(read2);
    assert_eq!(b.status(), Status::Some);
}

#[test]
fn option_lock_downgrade() {
    let a = OptionLock::<u32>::new();
    let write = a.try_lock().unwrap();
    assert_eq!(a.downgrade(write), None);
    drop(a);

    let b = OptionLock::new();
    let mut write = b.try_lock().unwrap();
    write.replace(20);
    let read = b.downgrade(write).unwrap();
    assert_eq!(b.status(), Status::ReadLock(1));
    assert_eq!(*read, 20);
    drop(read);
    assert_eq!(b.status(), Status::Some);
}

#[test]
fn option_lock_debug() {
    assert_eq!(
        format!("{:?}", &OptionLock::<i32>::new()),
        "OptionLock { status: None }"
    );
    assert_eq!(
        format!("{:?}", &OptionLock::from(1)),
        "OptionLock { status: Some }"
    );

    let lock = OptionLock::from(1);
    let read = lock.read().unwrap();
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
