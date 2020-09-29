use std::cell::UnsafeCell;
use std::mem::MaybeUninit;
use std::ops::{Deref, DerefMut};
use std::ptr::{self, NonNull};

#[repr(transparent)]
pub struct BoxPtr<T>(NonNull<T>);

impl<T> BoxPtr<T> {
    #[inline]
    pub fn new(value: Box<T>) -> Self {
        unsafe { Self::new_unchecked(Box::into_raw(value)) }
        // unstable: Self(Box::into_raw_non_null(value))
    }

    #[inline]
    pub unsafe fn new_unchecked(ptr: *mut T) -> Self {
        debug_assert!(!ptr.is_null());
        Self(NonNull::new_unchecked(ptr))
    }

    #[inline]
    pub fn as_ptr(&self) -> *mut T {
        self.0.as_ptr()
    }

    pub fn into_box(self) -> Box<T> {
        unsafe { Box::from_raw(self.0.as_ptr()) }
    }
}

impl<T> Clone for BoxPtr<T> {
    fn clone(&self) -> Self {
        Self(self.0)
    }
}

impl<T> Copy for BoxPtr<T> {}

impl<T> Deref for BoxPtr<T> {
    type Target = T;

    #[inline]
    fn deref(&self) -> &Self::Target {
        unsafe { self.0.as_ref() }
    }
}

impl<T> DerefMut for BoxPtr<T> {
    #[inline]
    fn deref_mut(&mut self) -> &mut Self::Target {
        unsafe { self.0.as_mut() }
    }
}

#[cfg_attr(not(debug_assertions), repr(transparent))]
pub struct Maybe<T> {
    data: UnsafeCell<MaybeUninit<T>>,
    #[cfg(debug_assertions)]
    loaded: UnsafeCell<bool>,
}

impl<T> Maybe<T> {
    #[cfg(debug_assertions)]
    #[inline]
    pub const fn empty() -> Self {
        Self {
            data: UnsafeCell::new(MaybeUninit::uninit()),
            loaded: UnsafeCell::new(false),
        }
    }

    #[cfg(not(debug_assertions))]
    #[inline]
    pub const fn empty() -> Self {
        Self {
            data: UnsafeCell::new(MaybeUninit::uninit()),
        }
    }

    #[inline]
    pub unsafe fn as_ptr(&self) -> *const T {
        #[cfg(debug_assertions)]
        assert_eq!(*self.loaded.get(), true, "as_ptr for non-loaded value");
        (&mut *self.data.get()).as_ptr()
    }

    #[inline]
    pub fn clear(&self) {
        unsafe {
            #[cfg(debug_assertions)]
            {
                assert_eq!(*self.loaded.get(), true, "duplicate load");
                *self.loaded.get() = false;
            }
            ptr::drop_in_place((&mut *self.data.get()).as_mut_ptr())
        }
    }

    #[inline]
    pub fn load(&self) -> T {
        unsafe {
            #[cfg(debug_assertions)]
            {
                assert_eq!(*self.loaded.get(), true, "duplicate load");
                *self.loaded.get() = false;
            };
            ptr::read(self.data.get()).assume_init()
        }
    }

    #[inline]
    pub fn store(&mut self, value: T) {
        unsafe {
            #[cfg(debug_assertions)]
            {
                assert_eq!(*self.loaded.get(), false, "duplicate store");
                *self.loaded.get() = true;
            };
            self.data.get().write(MaybeUninit::new(value))
        }
    }
}
