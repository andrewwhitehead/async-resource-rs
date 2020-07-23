mod pool;
pub use pool::{default_executor, Acquire, AcquireError, Executor, Pool, PoolConfig, PoolDrain};

mod resource;
pub use resource::Managed;

mod shared;

mod util;
pub use util::injector;
