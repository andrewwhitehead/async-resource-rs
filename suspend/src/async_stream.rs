use std::cell::UnsafeCell;
use std::future::Future;
use std::marker::PhantomData;
use std::pin::Pin;
use std::ptr::drop_in_place;
use std::task::{Context, Poll};

use futures_core::{FusedStream, Stream};

#[inline]
pub(crate) unsafe fn channel_send<T>(channel: *mut Option<T>, value: T) {
    if (&mut *channel).replace(value).is_some() {
        panic!("Invalid use of stream sender");
    }
}

#[inline]
pub(crate) unsafe fn channel_recv<T>(channel: *mut Option<T>) -> Option<T> {
    (&mut *channel).take()
}

pub struct AsyncStreamSender<'a, T> {
    channel: *mut Option<T>,
    _pd: PhantomData<&'a ()>,
}

impl<T> AsyncStreamSender<'_, T> {
    pub(crate) fn new(channel: *mut Option<T>) -> Self {
        Self {
            channel,
            _pd: PhantomData,
        }
    }

    pub fn send<'a, 'b>(&'b mut self, value: T) -> AsyncStreamSend<'a, T>
    where
        'b: 'a,
    {
        unsafe {
            channel_send(self.channel, value);
            AsyncStreamSend {
                channel: &mut *self.channel,
                first: true,
            }
        }
    }
}

impl<T> Clone for AsyncStreamSender<'_, T> {
    fn clone(&self) -> Self {
        Self {
            channel: self.channel,
            _pd: PhantomData,
        }
    }
}

pub struct AsyncStreamSend<'a, T> {
    channel: &'a mut Option<T>,
    first: bool,
}

impl<T> Future for AsyncStreamSend<'_, T> {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Self::Output> {
        if self.first || self.channel.is_some() {
            // always wait on first poll - the sender has just been filled
            // remain pending while the lock is occupied
            self.first = false;
            Poll::Pending
        } else {
            Poll::Ready(())
        }
    }
}

pub struct AsyncStream<'a, T, I, F> {
    state: UnsafeCell<AsyncStreamState<I, F>>,
    channel: UnsafeCell<Option<T>>,
    _pd: PhantomData<&'a ()>,
}

enum AsyncStreamState<I, F> {
    Init(I),
    Poll(F),
    Complete,
}

pub fn make_stream<'a, T, I, F>(init: I) -> AsyncStream<'a, T, I, F>
where
    I: FnOnce(AsyncStreamSender<'a, T>) -> F + 'a,
    F: Future<Output = ()> + 'a,
{
    AsyncStream {
        state: AsyncStreamState::Init(init).into(),
        channel: None.into(),
        _pd: PhantomData,
    }
}

impl<'a, T, I, F> Stream for AsyncStream<'a, T, I, F>
where
    I: FnOnce(AsyncStreamSender<'a, T>) -> F,
    F: Future<Output = ()>,
{
    type Item = T;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        loop {
            unsafe {
                match &mut *self.state.get() {
                    state @ AsyncStreamState::Init(_) => {
                        let mut copy = AsyncStreamState::Complete;
                        std::mem::swap(&mut copy, state);
                        if let AsyncStreamState::Init(init) = copy {
                            let fut = init(AsyncStreamSender::new(self.channel.get()));
                            self.state.get().write(AsyncStreamState::Poll(fut));
                        } else {
                            unreachable!();
                        }
                    }
                    AsyncStreamState::Poll(fut) => {
                        if let Poll::Ready(_) = Pin::new_unchecked(&mut *fut).poll(cx) {
                            drop_in_place(fut);
                            self.state.get().write(AsyncStreamState::Complete);
                        } else {
                            break if let Some(val) = channel_recv(self.channel.get()) {
                                Poll::Ready(Some(val))
                            } else {
                                Poll::Pending
                            };
                        }
                    }
                    AsyncStreamState::Complete => {
                        break Poll::Ready(channel_recv(self.channel.get()));
                    }
                }
            }
        }
    }
}

impl<'a, T, I, F> FusedStream for AsyncStream<'a, T, I, F>
where
    I: FnOnce(AsyncStreamSender<'a, T>) -> F,
    F: Future<Output = ()>,
{
    fn is_terminated(&self) -> bool {
        matches!(unsafe { &*self.state.get() }, AsyncStreamState::Complete)
    }
}

pub struct TryStreamFut<'a, T, E, F> {
    channel: *mut Option<Result<T, E>>,
    fut: F,
    _pd: PhantomData<&'a ()>,
}

impl<'a, T, E, F> TryStreamFut<'a, T, E, F> {
    pub fn new(sender: AsyncStreamSender<'a, Result<T, E>>, fut: F) -> Self {
        Self {
            channel: sender.channel,
            fut,
            _pd: PhantomData,
        }
    }
}

impl<'a, T, E, F> Future for TryStreamFut<'a, T, E, F>
where
    F: Future<Output = Result<(), E>>,
{
    type Output = ();
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        unsafe {
            let channel = &mut *self.channel;
            let fut = self.map_unchecked_mut(|s| &mut s.fut);
            fut.poll(cx).map(|result| {
                if let Err(err) = result {
                    channel_send(channel, Err(err));
                }
            })
        }
    }
}

#[macro_export]
macro_rules! stream {
    {$($block:tt)*} => {
        $crate::async_stream::make_stream(move |mut __sender| async move {
            #[allow(unused)]
            macro_rules! send {
                ($v:expr) => {
                    __sender.send($v).await;
                }
            }
            $($block)*
        })
    }
}

#[macro_export]
macro_rules! try_stream {
    {$($block:tt)*} => {
        $crate::async_stream::make_stream(move |mut __sender| {
            $crate::async_stream::TryStreamFut::new(__sender.clone(), async move {
                    macro_rules! send {
                        ($v:expr) => {
                            __sender.send(Ok($v)).await;
                        }
                    }
                    $($block)*
                }
            )
        })
    }
}
