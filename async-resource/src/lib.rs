mod pool;
pub use pool::{default_executor, Acquire, AcquireError, Executor, Pool, PoolConfig, PoolDrain};

mod resource;
pub use resource::Managed;

mod shared;

pub mod util;