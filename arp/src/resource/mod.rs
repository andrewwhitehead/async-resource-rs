use std::time::Instant;

mod lock;
pub use lock::{ResourceGuard, ResourceLock};

mod operation;
pub use operation::{
    resource_create, resource_dispose, resource_update, ResourceFuture, ResourceOperation,
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
    pub last_verified: Option<Instant>,
    pub disposed_at: Option<Instant>,
    pub borrowed: bool,
}

impl Default for ResourceInfo {
    fn default() -> Self {
        Self {
            start: Instant::now(),
            created_at: None,
            borrow_count: 0,
            last_borrow: None,
            last_idle: None,
            last_verified: None,
            disposed_at: None,
            borrowed: false,
        }
    }
}
