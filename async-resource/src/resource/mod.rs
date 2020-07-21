use std::time::Instant;

mod lock;
pub use lock::{ResourceGuard, ResourceLock};

mod managed;
pub use managed::Managed;

#[derive(Copy, Clone, Debug)]
pub struct ResourceInfo {
    pub start: Instant,
    pub created_at: Option<Instant>,
    pub acquire_count: usize,
    pub last_acquire: Option<Instant>,
    pub last_idle: Option<Instant>,
    pub expired: bool,
    pub reusable: bool,
    pub verify_at: Option<Instant>,
}

impl Default for ResourceInfo {
    fn default() -> Self {
        Self {
            start: Instant::now(),
            created_at: None,
            acquire_count: 0,
            last_acquire: None,
            last_idle: None,
            expired: false,
            reusable: true,
            verify_at: None,
        }
    }
}
