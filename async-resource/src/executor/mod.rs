use futures_lite::future::Boxed as BoxFuture;

#[cfg(feature = "bounded-exec")]
mod bounded;

#[cfg(feature = "bounded-exec")]
pub use self::bounded::BoundedExecutor;

#[cfg(feature = "bounded-exec")]
/// Returns a default [`Executor`] instance to use when constructing a resource
/// pool.
pub fn default_executor() -> Result<Box<dyn Executor>, crate::pool::ConfigError> {
    Ok(Box::new(self::bounded::BoundedExecutor::global()))
}

#[cfg(not(any(feature = "bounded-exec")))]
/// Returns a default [`Executor`] instance to use when constructing a resource
/// pool.
pub fn default_executor() -> Result<Box<dyn Executor>, ConfigError> {
    Err(ConfigError("No default executor is provided".to_owned()))
}

/// Defines a pluggable executor for Futures evaluated within the context of the
/// resource pool.
pub trait Executor: Send + Sync {
    /// Spawn a static, boxed Future with no return value
    fn spawn_ok(&self, task: BoxFuture<()>);
}
