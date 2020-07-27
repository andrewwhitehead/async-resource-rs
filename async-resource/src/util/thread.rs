use std::fmt::{self, Debug, Formatter};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::thread;

use option_lock::OptionLock;
use suspend::{message_task, Notifier, Suspend};

use super::sentinel::Sentinel;

enum Command<T> {
    Run(Box<dyn FnOnce(&mut T) + Send>),
    Extract(Box<dyn FnOnce(T) + Send>),
    Stop,
}

/// The error returned when a `Task` fails to complete.
pub type Canceled = suspend::Incomplete;

/// The error returned when a `Task` fails to complete.
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

/// A dedicated thread managing interaction with a resource. It can serve as
/// a handle for working with a `!Send` resource.
pub struct ThreadResource<T> {
    handle: Option<thread::JoinHandle<()>>,
    notifier: Notifier,
    state: Arc<State<T>>,
}

impl<T> ThreadResource<T> {
    /// Create a new instance from a constructor.
    pub fn create<F>(ctor: F) -> Self
    where
        F: FnOnce() -> T + Send + 'static,
        T: 'static,
    {
        Self::try_create(|| Result::<T, ()>::Ok(ctor())).unwrap()
    }

    /// Create a new instance from a fallible constructor, returning the
    /// resulting error if the constructor fails.
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
        let (send_start, recv_start) = message_task();
        let handle = Some(thread::spawn(move || {
            let state = Sentinel::new(int_state, |state, _| {
                state.running.store(false, Ordering::Release);
            });
            let (res, start) = match ctor() {
                Ok(res) => (Some(res), Ok(())),
                Err(err) => (None, Err(err)),
            };
            if send_start.send(start).is_err() {
                panic!("ThreadResource::try_create() receiver dropped");
            }
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
        recv_start.wait().unwrap()?;
        Ok(Self {
            handle,
            notifier,
            state,
        })
    }

    /// Perform an action on the contained resource, returning a `Task`
    /// representing the result of the computation. This result may be
    /// awaited as a `Future` or resolved using `wait` or `wait_timeout`.
    #[must_use]
    pub fn enter<F, R>(&mut self, f: F) -> Task<'_, R>
    where
        F: FnOnce(&mut T) -> R + Send + 'static,
        R: Send + 'static,
    {
        let (sender, task) = message_task();
        self.run_command(Command::Run(Box::new(|res| {
            sender.send(f(res)).unwrap_or(())
        })));
        task
    }

    /// Consume the contained resource and shut down the thread.
    #[must_use]
    pub fn extract<'a, F, R>(mut self, f: F) -> Task<'a, R>
    where
        F: FnOnce(T) -> R + Send + 'static,
        R: Send + 'static,
    {
        let (sender, receiver) = message_task();
        self.run_command(Command::Extract(Box::new(|res| {
            if sender.send(f(res)).is_err() {
                panic!("ThreadResource::extract() receiver dropped");
            }
        })));
        self.handle.take(); // skip Stop procedure
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
    use std::rc::Rc;

    #[test]
    fn thread_resource_basic() {
        let mut res = ThreadResource::create(|| 100u32);
        res.enter(|i| *i += 1).wait().unwrap();
        assert_eq!(res.extract(|res| res).wait(), Ok(101));
    }

    #[test]
    fn thread_resource_not_send() {
        let mut res = ThreadResource::create(|| Rc::new(100u32));
        res.enter(|i| *Rc::get_mut(i).unwrap() += 1).wait().unwrap();
        assert_eq!(res.extract(|i| Rc::try_unwrap(i).unwrap()).wait(), Ok(101));
    }
}
