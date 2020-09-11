use std::fmt::{self, Debug, Display, Formatter};

/// An error during resource handle acquisition.
pub enum AcquireError<E> {
    /// There are too many waiters
    PoolBusy,
    /// The resource pool is closed
    PoolClosed,
    /// Wraps an error result from a pool's `create` or `verify` callback
    ResourceError(E),
    /// The acquire timed out
    Timeout,
}

impl<E: Debug> Debug for AcquireError<E> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match &self {
            Self::PoolBusy => write!(f, "AcquireError::PoolBusy"),
            Self::PoolClosed => write!(f, "AcquireError::PoolClosed"),
            Self::ResourceError(err) => f
                .debug_tuple("AcquireError::ResourceError")
                .field(err)
                .finish(),
            Self::Timeout => write!(f, "AcquireError::Timeout"),
        }
    }
}

impl<E: Display> Display for AcquireError<E> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match &self {
            Self::PoolBusy => write!(f, "The resource pool is occupied"),
            Self::PoolClosed => write!(f, "The resource pool is closed"),
            Self::ResourceError(err) => write!(f, "Resource error: {}", err),
            Self::Timeout => write!(f, "The request timed out"),
        }
    }
}

impl<E: Debug + Display> std::error::Error for AcquireError<E> {}

/// A configuration error.
#[derive(Debug)]
pub struct ConfigError(pub String);

impl Display for ConfigError {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "Config error: {}", &self.0)
    }
}

impl std::error::Error for ConfigError {}
