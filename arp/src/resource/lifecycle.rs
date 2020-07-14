use super::lock::ResourceGuard;
use super::operation::{ResourceFuture, ResourceOperation};

pub struct Lifecycle<T, E> {
    pub create: Box<dyn ResourceOperation<T, E> + Send + Sync>,
    pub dispose: Option<Box<dyn ResourceOperation<T, E> + Send + Sync>>,
    pub handle_error: Option<Box<dyn Fn(E) + Send + Sync>>,
    pub keepalive: Option<Box<dyn ResourceOperation<T, E> + Send + Sync>>,
}

impl<T, E> Lifecycle<T, E> {
    pub fn new(create: Box<dyn ResourceOperation<T, E> + Send + Sync>) -> Self {
        Self {
            create,
            dispose: None,
            handle_error: None,
            keepalive: None,
        }
    }

    pub fn create(&self, target: ResourceGuard<T>) -> ResourceFuture<T, E> {
        self.create.apply(target)
    }

    pub fn keepalive(&self, target: ResourceGuard<T>) -> Option<ResourceFuture<T, E>> {
        println!("keepalive");
        if let Some(handler) = self.keepalive.as_ref() {
            Some(handler.apply(target))
        } else {
            self.dispose(target)
        }
    }

    pub fn handle_error(&self, err: E) {
        if let Some(handler) = self.handle_error.as_ref() {
            (handler)(err)
        }
    }

    // pub fn verify_acquire(&self, fut: ResourceFuture<R, E>) -> ResourceFuture<R, E> {
    //     fut
    // }

    // pub fn verify_release(&self, fut: ResourceFuture<R, E>) -> ResourceFuture<R, E> {
    //     fut
    // }

    pub fn dispose(&self, target: ResourceGuard<T>) -> Option<ResourceFuture<T, E>> {
        println!("dispose");
        if let Some(handler) = self.dispose.as_ref() {
            Some(handler.apply(target))
        } else {
            None
        }
    }
}
