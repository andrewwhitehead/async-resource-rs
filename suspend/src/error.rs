use std::error::Error;
use std::fmt::{self, Display, Formatter};

/// An error indicating that the [`TaskSender`](crate::task::TaskSender) side
/// of a one-shot channel was dropped without sending a result.
#[derive(Debug, PartialEq, Eq, Hash)]
pub struct Incomplete;

impl Display for Incomplete {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "Incomplete")
    }
}

impl Error for Incomplete {}

/// A timeout error which may be returned when waiting for a
/// [`Listener`](crate::Listener), [`Task`](crate::task::Task) or
/// [`Iter`](crate::iter::Iter) with a given expiry time.
#[derive(Debug, PartialEq, Eq, Hash)]
pub struct TimedOut;

impl Display for TimedOut {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "Timed out")
    }
}

impl Error for TimedOut {}
