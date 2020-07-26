use std::fmt::{self, Debug, Formatter};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::thread;

use oneshot::{channel, Receiver};
use option_lock::OptionLock;
use suspend::{sender_task, Notifier, Suspend};

use super::sentinel::Sentinel;

pub enum Command<T> {
    Run(Box<dyn FnOnce(&mut T) + Send>),
    Extract(Box<dyn FnOnce(T) + Send>),
    Stop,
}

pub type Canceled = oneshot::RecvError;

pub type Task<'t, R> = suspend::Task<'t, Result<R, Canceled>>;

struct State<T> {
    pub next: OptionLock<Command<T>>,
    pub running: AtomicBool,
}

impl<T> State<T> {
    const fn new() -> Self {
        Self {
            next: OptionLock::new(),
            running: AtomicBool::new(true),
        }
    }
}

pub struct ThreadResource<T> {
    handle: Option<thread::JoinHandle<()>>,
    notifier: Notifier,
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
        let mut suspend = Suspend::new();
        let notifier = suspend.notifier();
        let state = Arc::new(State::new());
        let int_state = state.clone();
        let (send_start, recv_start) = channel();
        let handle = Some(thread::spawn(move || {
            let state = Sentinel::new(int_state, |state, _| {
                state.running.store(false, Ordering::Release);
            });
            let (res, start) = match ctor() {
                Ok(res) => (Some(res), Ok(())),
                Err(err) => (None, Err(err)),
            };
            send_start
                .send(start)
                .expect("ThreadResource::try_create() receiver dropped");
            let mut res = match res {
                Some(res) => res,
                None => return,
            };
            loop {
                let mut listen = suspend.listen();
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
                        listen.wait();
                    }
                }
            }
        }));
        recv_start.recv().unwrap()?;
        Ok(Self {
            handle,
            notifier,
            state,
        })
    }

    #[must_use]
    pub fn enter<F, R>(&mut self, f: F) -> Task<'_, R>
    where
        F: FnOnce(&mut T) -> R + Send + 'static,
        R: Send + 'static,
    {
        let (sender, task) = sender_task();
        self.run_command(Command::Run(Box::new(|res| {
            sender.send(f(res)).unwrap_or(())
        })));
        task
    }

    #[must_use]
    pub fn extract(mut self) -> Receiver<T>
    where
        T: Send + 'static,
    {
        let (sender, receiver) = channel();
        self.run_command(Command::Extract(Box::new(|res| {
            sender
                .send(res)
                .expect("ThreadResource::extract() receiver dropped");
        })));
        // skip Stop procedure
        self.handle.take();
        receiver
    }

    fn run_command(&mut self, cmd: Command<T>) -> bool {
        // only contention should be with the executor thread, which locks
        // just long enough to retrieve the value
        let mut guard = self.state.next.spin_lock();
        let ret = if self.state.running.load(Ordering::Acquire) && guard.is_none() {
            guard.replace(cmd);
            true
        } else {
            // if the thread stopped unexpectedly then the task is dropped,
            // which generally causes a Sender to be cancelled.
            // if the previous Task is not consumed then there could also be
            // a Command in the queue. return false but wake the executor
            // thread in case it failed to acquire the lock while we were
            // holding it.
            false
        };
        drop(guard);
        self.notifier.notify();
        ret
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
