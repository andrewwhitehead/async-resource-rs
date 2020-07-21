use option_lock::OptionLock;
use std::task::Waker;

enum ResultState<T, E> {
    Ready(Result<T, E>),
    Wake(Waker),
}

pub struct AsyncResult<T, E> {
    state: OptionLock<ResultState<T, E>>,
}

impl<T, E> AsyncResult<T, E> {
    pub const fn new() -> Self {
        Self {
            state: OptionLock::new(),
        }
    }

    pub fn poll(&self, waker: Option<Waker>) -> Option<Result<T, E>> {
        match self.state.try_lock() {
            Ok(mut guard) => match guard.take() {
                Some(ResultState::Ready(result)) => Some(result),
                Some(ResultState::Wake(_)) | None => {
                    waker.map(|waker| guard.replace(ResultState::Wake(waker)));
                    None
                }
            },
            Err(_) => {
                waker.map(Waker::wake); // result is currently being stored
                None
            }
        }
    }

    pub fn fulfill(&self, result: Result<T, E>) -> Result<(), Result<T, E>> {
        // retry method is left up to the caller (spin, yield thread, etc)
        if let Ok(mut guard) = self.state.try_lock() {
            let prev = guard.replace(ResultState::Ready(result));
            drop(guard);
            if let Some(ResultState::Wake(waker)) = prev {
                waker.wake();
            }
            Ok(())
        } else {
            Err(result)
        }
    }
}

fn main() {
    let fut = AsyncResult::<u32, ()>::new();
    fut.fulfill(Ok(100)).unwrap();
    assert_eq!(fut.poll(None), Some(Ok(100)));
}
