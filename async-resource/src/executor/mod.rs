use futures_lite::future::Boxed as BoxFuture;

#[cfg(feature = "global-exec")]
mod global;

#[cfg(feature = "global-exec")]
pub use self::global::GlobalExecutor;

#[cfg(feature = "global-exec")]
/// Returns a default [`Executor`] instance to use when constructing a resource
/// pool.
pub fn default_executor() -> Result<Box<dyn Executor>, crate::pool::ConfigError> {
    Ok(Box::new(self::global::GlobalExecutor))
}

#[cfg(not(any(feature = "global-exec")))]
/// Returns a default [`Executor`] instance to use when constructing a resource
/// pool.
pub fn default_executor() -> Result<Box<dyn Executor>, ConfigError> {
    Err(ConfigError("No default executor is provided".to_owned()))
}

/// Defines a pluggable executor for Futures evaluated within the context of the
/// resource pool.
pub trait Executor: Send + Sync {
    /// Spawn a static, boxed Future with no return value
    fn spawn_obj(&self, task: BoxFuture<()>);
}
