mod pool;
pub use pool::{Acquire, AcquireError, Pool, PoolConfig, PoolDrain};

mod resource;
pub use resource::Managed;

mod shared;

mod wait;
