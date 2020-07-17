mod pool;
pub use pool::{Acquire, AcquireError, Pool, PoolConfig, PoolShutdown};

mod resource;
pub use resource::Managed;

mod shared;

mod wait;
