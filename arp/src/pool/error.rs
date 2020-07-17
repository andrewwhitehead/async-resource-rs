use std::fmt::{self, Debug, Formatter};

pub enum AcquireError<E> {
    Busy,
    ResourceError(E),
    Stopped,
    Timeout,
}

impl<E: Debug> Debug for AcquireError<E> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match &self {
            Self::Busy => write!(f, "AcquireError::Busy"),
            Self::ResourceError(err) => write!(f, "AcquireError::ResourceError({:?})", err),
            Self::Stopped => write!(f, "AcquireError::Stopped"),
            Self::Timeout => write!(f, "AcquireError::Timeout"),
        }
    }
}
