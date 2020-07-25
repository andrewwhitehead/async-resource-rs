use std::cell::UnsafeCell;
use std::fmt::{self, Debug, Display, Formatter};
use std::future::Future;
use std::mem::MaybeUninit;
use std::pin::Pin;
use std::sync::{
    atomic::{AtomicU8, Ordering},
    Arc,
};
use std::task::{Context, Poll, Waker};
use std::thread;
use std::time::{Duration, Instant};

use futures_util::{
    future::BoxFuture,
    pin_mut,
    stream::{BoxStream, Stream},
    task::{waker, ArcWake},
    FutureExt, StreamExt,
};

#[cfg(feature = "oneshot")]
pub use oneshot_rs as oneshot;

// FIXME add Debug impl

const IDLE: u8 = 0x0;
const BUSY: u8 = 0x1;
const LISTEN: u8 = 0x2;

pub fn notify_once<'a>() -> (Notifier, Task<'a, ()>) {
    let inner = Arc::new(Inner::new(IDLE));
    let notifier = Notifier {
        inner: inner.clone(),
    };
    (notifier, Task::from_poll(move |cx| inner.poll(cx.waker())))
}

enum InnerNotify {
    Thread(thread::Thread),
    Waker(Waker),
}

enum ClearResult {
    Removed(InnerNotify),
    NoChange,
    Updated,
}

#[derive(Debug, PartialEq, Eq)]
pub struct TimeoutError;

impl Display for TimeoutError {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "Timed out")
    }
}

impl std::error::Error for TimeoutError {}

impl ClearResult {
    pub fn is_none(&self) -> bool {
        matches!(self, Self::NoChange | Self::Updated)
    }
}

struct Inner {
    notify: UnsafeCell<MaybeUninit<InnerNotify>>,
    state: AtomicU8,
}

impl Inner {
    pub const fn new(state: u8) -> Self {
        Self {
            notify: UnsafeCell::new(MaybeUninit::uninit()),
            state: AtomicU8::new(state),
        }
    }

    pub fn acquire<'a>(self: &'a Arc<Self>) -> Option<Listener<'a>> {
        if self.state.compare_and_swap(BUSY, IDLE, Ordering::AcqRel) == BUSY {
            Some(Listener { inner: self })
        } else {
            None
        }
    }

    pub fn clear(&self, idle: bool) -> ClearResult {
        let newval = if idle { IDLE } else { BUSY };
        match self.state.swap(newval, Ordering::Release) {
            LISTEN => ClearResult::Removed(unsafe { self.vacate() }),
            state => {
                if state == newval {
                    ClearResult::NoChange
                } else {
                    ClearResult::Updated
                }
            }
        }
    }

    pub fn listen(&self, notify: InnerNotify) -> bool {
        match self.state() {
            IDLE => (),
            BUSY => {
                return false;
            }
            LISTEN => {
                // try to reacquire idle state
                loop {
                    match self.state.compare_exchange_weak(
                        LISTEN,
                        IDLE,
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    ) {
                        Ok(_) => {
                            break unsafe {
                                self.vacate();
                            }
                        }
                        Err(IDLE) => {
                            // competition from another thread?
                            // should not happen with wrappers around this instance
                            panic!("Invalid state");
                        }
                        Err(LISTEN) => {
                            // retry
                        }
                        Err(BUSY) => {
                            // already notified
                            return false;
                        }
                        Err(_) => panic!("Invalid state"),
                    }
                }
            }
            _ => panic!("Invalid state"),
        }
        unsafe {
            self.update(notify);
        }
        match self.state.compare_and_swap(IDLE, LISTEN, Ordering::AcqRel) {
            IDLE => true,
            BUSY => {
                // notify was called while storing
                unsafe {
                    self.vacate();
                }
                false
            }
            _ => panic!("Invalid state"),
        }
    }

    pub fn notify(&self) -> bool {
        match self.clear(false) {
            ClearResult::Removed(InnerNotify::Thread(thread)) => {
                thread.unpark();
                true
            }
            ClearResult::Removed(InnerNotify::Waker(waker)) => {
                waker.wake();
                true
            }
            ClearResult::Updated => true,
            ClearResult::NoChange => false,
        }
    }

    pub fn poll(&self, waker: &Waker) -> Poll<()> {
        match self.state() {
            BUSY => {
                return Poll::Ready(());
            }
            LISTEN => {
                // try to clear existing waker and move back to idle state
                if self.clear(true).is_none() {
                    // already taken (thread was pre-empted)
                    return Poll::Ready(());
                }
            }
            _ => (),
        }

        if self.listen(InnerNotify::Waker(waker.clone())) {
            Poll::Pending
        } else {
            // already notified
            Poll::Ready(())
        }
    }

    pub fn state(&self) -> u8 {
        self.state.load(Ordering::Acquire)
    }

    pub unsafe fn update(&self, notify: InnerNotify) {
        self.notify.get().write(MaybeUninit::new(notify))
    }

    pub unsafe fn vacate(&self) -> InnerNotify {
        self.notify.get().read().assume_init()
    }

    pub fn wait(&self) {
        if !self.listen(InnerNotify::Thread(thread::current())) {
            // already notified
            return;
        }
        loop {
            thread::park();
            if self.state() == BUSY {
                break;
            }
        }
    }

    pub fn wait_deadline(&self, expire: Instant) -> bool {
        if !self.listen(InnerNotify::Thread(thread::current())) {
            // already notified
            return true;
        }
        while let Some(dur) = expire.checked_duration_since(Instant::now()) {
            thread::park_timeout(dur);
            if self.state() == BUSY {
                // notified
                return true;
            }
        }
        // timer expired
        false
    }

    pub fn wait_timeout(&self, timeout: Duration) -> bool {
        self.wait_deadline(Instant::now() + timeout)
    }
}

impl ArcWake for Inner {
    fn wake_by_ref(arc_self: &Arc<Self>) {
        arc_self.notify();
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        // vacate any registered listener
        self.clear(false);
    }
}

unsafe impl Sync for Inner {}

// pub struct NotifyOnce {
//     inner: Inner,
// }

// impl NotifyOnce {
//     pub const fn new() -> Self {
//         Self {
//             inner: Inner {
//                 notify: UnsafeCell::new(MaybeUninit::uninit()),
//                 state: AtomicU8::new(IDLE),
//             },
//         }
//     }

//     pub fn cancel(&self) -> bool {
//         match self.inner.clear(false) {
//             ClearResult::NoChange => false,
//             _ => true,
//         }
//     }

//     pub fn complete(&self) -> bool {
//         self.inner.notify()
//     }

//     pub fn is_complete(&self) -> bool {
//         self.inner.state() == BUSY
//     }

//     pub fn poll(&self, waker: &Waker) -> Poll<()> {
//         self.inner.poll(waker)
//     }

//     pub fn wait(&self) {
//         self.inner.wait()
//     }

//     pub fn wait_deadline(&self, expire: Instant) -> bool {
//         self.inner.wait_deadline(expire)
//     }

//     pub fn wait_timeout(&self, timeout: Duration) -> bool {
//         self.inner.wait_timeout(timeout)
//     }

//     pub fn waker(self: &Arc<Self>) -> Waker {
//         waker(self.clone())
//     }
// }

// impl ArcWake for NotifyOnce {
//     fn wake_by_ref(arc_self: &Arc<Self>) {
//         arc_self.complete();
//     }
// }

// impl Future for NotifyOnce {
//     type Output = ();

//     fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
//         self.inner.poll(cx.waker())
//     }
// }

pub struct Suspend {
    inner: Arc<Inner>,
}

impl Suspend {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Inner::new(BUSY)),
        }
    }

    pub fn block_on<F>(&mut self, fut: F) -> F::Output
    where
        F: Future,
    {
        pin_mut!(fut);
        loop {
            let waker = self.waker();
            let mut cx = Context::from_waker(&waker);
            let mut listen = self.listen();
            match fut.as_mut().poll(&mut cx) {
                Poll::Ready(result) => break result,
                Poll::Pending => listen.wait(),
            }
        }
    }

    pub fn notifier(&self) -> Notifier {
        Notifier {
            inner: self.inner.clone(),
        }
    }

    pub fn notify(&self) {
        self.inner.notify();
    }

    pub fn listen(&mut self) -> Listener<'_> {
        self.inner.acquire().expect("Invalid Suspend state")
    }

    pub fn try_listen(&self) -> Option<Listener<'_>> {
        self.inner.acquire()
    }

    pub fn waker(&self) -> Waker {
        waker(self.inner.clone())
    }
}

pub struct Listener<'a> {
    inner: &'a Arc<Inner>,
}

impl Listener<'_> {
    pub fn wait(&mut self) {
        self.inner.wait()
    }

    pub fn wait_deadline(&mut self, expire: Instant) -> bool {
        self.inner.wait_deadline(expire)
    }

    pub fn wait_timeout(&self, timeout: Duration) -> bool {
        self.inner.wait_timeout(timeout)
    }
}

impl Drop for Listener<'_> {
    fn drop(&mut self) {
        self.inner.clear(false);
    }
}

impl<'a> Future for Listener<'a> {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.inner.poll(cx.waker())
    }
}

pub struct Notifier {
    inner: Arc<Inner>,
}

impl Notifier {
    pub fn notify(&self) {
        self.inner.notify();
    }

    pub fn to_waker(self) -> Waker {
        waker(self.inner)
    }
}

impl Clone for Notifier {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

pub type PollFn<'a, T> = Box<dyn FnMut(&mut Context) -> Poll<T> + Send + 'a>;

#[must_use = "Task must be awaited"]
pub enum Task<'t, T> {
    Future(BoxFuture<'t, T>),
    Poll(PollFn<'t, T>),
}

impl<'t, T> Task<'t, T> {
    pub fn from_poll<F>(f: F) -> Self
    where
        F: FnMut(&mut Context) -> Poll<T> + Send + 't,
    {
        Self::Poll(Box::new(f))
    }

    pub fn from_future<F>(f: F) -> Self
    where
        F: Future<Output = T> + Send + 't,
    {
        Self::Future(f.boxed())
    }

    pub fn ready(result: T) -> Self
    where
        T: Send + 't,
    {
        let mut result = Some(result);
        Self::from_poll(move |_| {
            if let Some(result) = result.take() {
                Poll::Ready(result)
            } else {
                Poll::Pending
            }
        })
    }

    pub fn map<'m, F, R>(mut self, f: F) -> Task<'m, R>
    where
        F: Fn(T) -> R + Send + 'm,
        T: 'm,
        't: 'm,
    {
        Task::from_poll(move |cx| Pin::new(&mut self).poll(cx).map(&f))
    }

    fn poll_inner(&mut self, cx: &mut Context) -> Poll<T> {
        match &mut *self {
            Self::Future(fut) => fut.as_mut().poll(cx),
            Self::Poll(poll) => poll(cx),
        }
    }

    pub fn wait(self) -> T {
        if let Ok(result) = self.wait_while(|listen| {
            listen.wait();
            true
        }) {
            result
        } else {
            // should not be possible
            panic!("wait_while returned error");
        }
    }

    pub fn wait_deadline(self, expire: Instant) -> Result<T, Self> {
        self.wait_while(|listen| listen.wait_deadline(expire))
    }

    pub fn wait_timeout(self, timeout: Duration) -> Result<T, Self> {
        let expire = Instant::now() + timeout;
        self.wait_while(|listen| listen.wait_deadline(expire))
    }

    fn wait_while<F>(mut self, test: F) -> Result<T, Self>
    where
        F: Fn(&mut Listener) -> bool,
    {
        let mut sus = Suspend::new();
        let waker = sus.waker();
        let mut cx = Context::from_waker(&waker);
        let mut listen = sus.listen();
        loop {
            match self.poll_inner(&mut cx) {
                Poll::Ready(result) => break Ok(result),
                Poll::Pending => {
                    if !test(&mut listen) {
                        break Err(self);
                    }
                }
            }
        }
    }
}

impl<T> Debug for Task<'_, T> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "Task({:p}", self)
    }
}

impl<T> PartialEq for Task<'_, T> {
    fn eq(&self, other: &Self) -> bool {
        std::ptr::eq(self, other)
    }
}

impl<T> Eq for Task<'_, T> {}

impl<'t, T> From<BoxFuture<'t, T>> for Task<'t, T> {
    fn from(fut: BoxFuture<'t, T>) -> Self {
        Self::Future(fut)
    }
}

impl<'t, T> From<PollFn<'t, T>> for Task<'t, T> {
    fn from(poll: PollFn<'t, T>) -> Self {
        Self::Poll(poll)
    }
}

impl<'t, T> Future for Task<'t, T> {
    type Output = T;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.poll_inner(cx)
    }
}

#[cfg(feature = "oneshot")]
impl<'t, T: Send + 't> Task<'t, T> {
    pub fn oneshot() -> (oneshot::Sender<T>, Task<'t, Result<T, oneshot::RecvError>>) {
        let (sender, receiver) = oneshot::channel();
        (sender, Task::from(receiver))
    }
}

#[cfg(feature = "oneshot")]
impl<'t, T: Send + 't> From<oneshot::Receiver<T>> for Task<'t, Result<T, oneshot::RecvError>> {
    fn from(mut receiver: oneshot::Receiver<T>) -> Self {
        Task::from_poll(move |cx| Pin::new(&mut receiver).poll(cx))
    }
}

pub type PollNextFn<'a, T> = Box<dyn FnMut(&mut Context) -> Poll<Option<T>> + Send + 'a>;

enum IterState<'s, T> {
    Stream(BoxStream<'s, T>),
    PollNext(PollNextFn<'s, T>),
}

impl<'s, T> IterState<'s, T> {
    fn poll_next(&mut self, cx: &mut Context<'_>) -> Poll<Option<T>> {
        match self {
            IterState::Stream(stream) => stream.as_mut().poll_next(cx),
            IterState::PollNext(poll_next) => poll_next(cx),
        }
    }
}

#[must_use = "Iter must be awaited"]
pub struct Iter<'s, T> {
    suspend: Option<Suspend>,
    state: IterState<'s, T>,
}

impl<'s, T> Iter<'s, T> {
    pub fn from_stream<S>(s: S) -> Self
    where
        S: Stream<Item = T> + Send + 's,
    {
        Self {
            suspend: None,
            state: IterState::Stream(s.boxed()),
        }
    }

    pub fn from_poll_next<F>(f: F) -> Self
    where
        F: FnMut(&mut Context) -> Poll<Option<T>> + Send + 's,
    {
        Self {
            suspend: None,
            state: IterState::PollNext(Box::new(f)),
        }
    }

    pub fn map<'m, F, R>(mut self, f: F) -> Iter<'m, R>
    where
        F: Fn(T) -> R + Send + 'm,
        T: 'm,
        's: 'm,
    {
        Iter::from_poll_next(move |cx| Pin::new(&mut self).poll_next(cx).map(|opt| opt.map(&f)))
    }

    pub fn wait_next(&mut self) -> Option<T> {
        self.wait_while(|listen| {
            listen.wait();
            true
        })
        .unwrap()
    }

    pub fn wait_next_deadline(&mut self, expire: Instant) -> Result<Option<T>, TimeoutError> {
        self.wait_while(|listen| listen.wait_deadline(expire))
    }

    pub fn wait_next_timeout(&mut self, timeout: Duration) -> Result<Option<T>, TimeoutError> {
        let expire = Instant::now() + timeout;
        self.wait_while(|listen| listen.wait_deadline(expire))
    }

    fn wait_while<F>(&mut self, test: F) -> Result<Option<T>, TimeoutError>
    where
        F: Fn(&mut Listener) -> bool,
    {
        if self.suspend.is_none() {
            self.suspend.replace(Suspend::new());
        }
        let sus = self.suspend.as_mut().unwrap();
        let waker = sus.waker();
        let mut listener = sus.listen();
        let mut cx = Context::from_waker(&waker);
        loop {
            match self.state.poll_next(&mut cx) {
                Poll::Ready(ret) => break Ok(ret),
                Poll::Pending => {
                    if !test(&mut listener) {
                        break Err(TimeoutError);
                    }
                }
            }
        }
    }
}

impl<'s, T> From<PollNextFn<'s, T>> for Iter<'s, T> {
    fn from(poll: PollNextFn<'s, T>) -> Self {
        Self {
            suspend: None,
            state: IterState::PollNext(poll),
        }
    }
}

impl<'s, T> Iterator for Iter<'s, T> {
    type Item = T;

    fn next(&mut self) -> Option<Self::Item> {
        self.wait_next()
    }
}

impl<'s, T> Stream for Iter<'s, T> {
    type Item = T;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.state.poll_next(cx)
    }
}
