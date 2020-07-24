use std::fmt::{self, Debug, Formatter};
use std::future::Future;
use std::marker::PhantomData;
use std::pin::Pin;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::task::{Context, Poll};
use std::thread;

use dropshot::{channel, Receiver};
use option_lock::OptionLock;
use suspend::Suspend;

use super::sentinel::Sentinel;

pub enum Command<T> {
    Run(Box<dyn FnOnce(&mut T) + Send>),
    Extract(Box<dyn FnOnce(T) + Send>),
    Stop,
}

pub type Canceled = dropshot::Canceled;

pub struct Task<'t, R> {
    receiver: Receiver<R>,
    _pd: PhantomData<&'t ()>,
}

impl<R> Task<'_, R> {
    pub fn wait(mut self) -> Result<R, Canceled> {
        self.receiver.recv()
    }
}

impl<R> Future for Task<'_, R> {
    type Output = Result<R, Canceled>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        Pin::new(&mut self.receiver).poll(cx)
    }
}

struct State<T> {
    pub next: OptionLock<Command<T>>,
    pub suspend: Suspend,
    pub running: AtomicBool,
}

impl<T> State<T> {
    fn new() -> Self {
        Self {
            next: OptionLock::new(),
            suspend: Suspend::new(),
            running: AtomicBool::new(true),
        }
    }
}

pub struct ThreadResource<T> {
    handle: Option<thread::JoinHandle<()>>,
    state: Arc<State<T>>,
}

impl<T> ThreadResource<T> {
    pub fn create<F>(ctor: F) -> Self
    where
        F: FnOnce() -> T + Send + 'static,
        T: 'static,
    {
        Self::try_create(|| Result::<T, ()>::Ok(ctor())).unwrap()
    }

    pub fn try_create<F, E>(ctor: F) -> Result<Self, E>
    where
        F: FnOnce() -> Result<T, E> + Send + 'static,
        E: Send + 'static,
        T: 'static,
    {
        let state = Arc::new(State::new());
        let int_state = state.clone();
        let (send_start, mut recv_start) = channel();
        let handle = Some(thread::spawn(move || {
            let state = Sentinel::new(int_state, |state, _| {
                state.running.store(false, Ordering::Release);
            });
            let (res, start) = match ctor() {
                Ok(res) => (Some(res), Ok(())),
                Err(err) => (None, Err(err)),
            };
            match send_start.send(start) {
                Ok(()) => (),
                Err(_) => return,
            }
            let mut res = match res {
                Some(res) => res,
                None => return,
            };
            loop {
                let mut listen = state.suspend.try_listen().unwrap();
                match state.next.try_take() {
                    Ok(Command::Run(f)) => {
                        f(&mut res);
                    }
                    Ok(Command::Extract(f)) => {
                        f(res);
                        break;
                    }
                    Ok(Command::Stop) => {
                        break;
                    }
                    Err(_) => {
                        listen.park();
                    }
                }
            }
        }));
        recv_start.recv().unwrap()?;
        Ok(Self { handle, state })
    }

    #[must_use]
    pub fn enter<F, R>(&mut self, f: F) -> Task<'_, R>
    where
        F: FnOnce(&mut T) -> R + Send + 'static,
        R: Send + 'static,
    {
        let (sender, receiver) = channel();
        self.run_command(Command::Run(Box::new(|res| {
            sender.send(f(res)).unwrap_or(())
        })));
        Task {
            receiver,
            _pd: PhantomData,
        }
    }

    pub fn extract(mut self) -> Receiver<T>
    where
        T: Send + 'static,
    {
        let (sender, recv) = channel();
        self.run_command(Command::Extract(Box::new(|res| {
            sender.send(res).unwrap_or(())
        })));
        recv
    }

    fn run_command(&mut self, cmd: Command<T>) {
        // only contention is with the command thread, which only locks
        // long enough to retrieve the value
        let mut guard = self.state.next.spin_lock();
        if self.state.running.load(Ordering::Acquire) {
            guard.replace(cmd);
        }
        // if the thread stopped unexpectedly then the task is dropped,
        // which generally causes a Sender to be cancelled
        drop(guard);
        self.state.suspend.notify();
    }
}

impl<T: Debug> Debug for ThreadResource<T> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "ThreadResource({:p})", self)
    }
}

impl<T> Drop for ThreadResource<T> {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            self.run_command(Command::Stop);
            handle.join().unwrap();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thread_resource_basic() {
        let mut res = ThreadResource::create(|| 100u32);
        res.enter(|i| *i += 1).wait().unwrap();
        assert_eq!(res.extract().recv(), Ok(101));
    }
}
