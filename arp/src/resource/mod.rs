use std::time::Instant;

mod lock;
pub use lock::{ResourceGuard, ResourceLock};

mod operation;
pub use operation::{
    resource_create, resource_dispose, resource_update, ResourceFuture, ResourceOperation,
    ResourceResolve,
};

mod lifecycle;
pub use lifecycle::Lifecycle;

#[derive(Copy, Clone, Debug)]
pub struct ResourceInfo {
    pub start: Instant,
    pub created_at: Option<Instant>,
    pub borrow_count: usize,
    pub last_borrow: Option<Instant>,
    pub last_idle: Option<Instant>,
    pub verify_at: Option<Instant>,
}

impl Default for ResourceInfo {
    fn default() -> Self {
        Self {
            start: Instant::now(),
            created_at: None,
            borrow_count: 0,
            last_borrow: None,
            last_idle: None,
            verify_at: None,
        }
    }
}
