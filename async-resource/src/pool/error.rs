use std::fmt::{self, Debug, Display, Formatter};

pub enum AcquireError<E> {
    Closed,
    ResourceError(E),
    Timeout,
}

impl<E: Debug> Debug for AcquireError<E> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match &self {
            Self::Closed => write!(f, "AcquireError::Closed"),
            Self::ResourceError(err) => write!(f, "AcquireError::ResourceError({:?})", err),
            Self::Timeout => write!(f, "AcquireError::Timeout"),
        }
    }
}

impl<E: Display> Display for AcquireError<E> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match &self {
            Self::Closed => write!(f, "The pool is closed"),
            Self::ResourceError(err) => write!(f, "Resource error: {}", err),
            Self::Timeout => write!(f, "The request timed out"),
        }
    }
}

impl<E: Debug + Display> std::error::Error for AcquireError<E> {}

#[derive(Debug)]
pub struct ConfigError(String);

impl Display for ConfigError {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "Config error: {}", &self.0)
    }
}

impl std::error::Error for ConfigError {}
