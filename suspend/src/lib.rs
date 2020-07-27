// Copyright 2020 Province of British Columbia
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// 	http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! This crate provides various utilities for moving between async and
//! synchronous contexts.
//!
//! # Examples
//!
//! The [`Task`] structure allows a `Future` or a polling function to be
//! evaluated in a blocking manner, as well as transform the result to produce
//! a new Task:
//!
//! ```
//! let task = suspend::Task::from_fut(async { 100 }).map(|val| val * 2);
//! assert_eq!(task.wait(), 200);
//! ```
//!
//! Similarly, the [`Iter`] structure allows a `Stream` instance to be consumed
//! in an async or blocking manner:
//!
//! ```
//! let mut values = suspend::Iter::from_iter(1..);
//! assert_eq!(suspend::block_on(async { values.next().await }), Some(1));
//! assert_eq!(values.take(3).collect::<Vec<_>>(), vec![2, 3, 4]);
//! ```
//!
//! The [`Suspend`] structure may be used to coordinate between threads and
//! `Futures`, allowing either to act as a waiter or notifier:
//!
//! ```
//! use std::time::Duration;
//!
//! let mut susp = suspend::Suspend::new();
//! let notifier = susp.notifier();
//!
//! // start listening for notifications
//! let mut listener = susp.listen();
//! // send a notification (satisfies the current listener)
//! notifier.notify();
//! // wait for notification (already sent) with a timeout
//! assert_eq!(listener.wait_timeout(Duration::from_secs(1)), true);
//! drop(listener);
//!
//! let mut listener = susp.listen();
//! notifier.notify();
//! // the listener is also a Future
//! suspend::block_on(async { listener.await });
//! ```
//!
//! It can also be used to directly poll a `Future`:
//!
//! ```
//! let mut susp = suspend::Suspend::new();
//! let mut task = suspend::Task::from_fut(async { 99 });
//! assert_eq!(susp.poll_unpin(&mut task), Ok(99));
//! ```

#[doc(hidden)]
pub mod re_export {
    pub use std::pin::Pin;
}

mod core;
pub use self::core::{Listener, Notifier, Suspend};

mod error;
pub use self::error::{Incomplete, TimedOut};

/// Providing a convenient wrapper around `Stream` types.
pub mod iter;
#[doc(hidden)]
pub use self::iter::{iter_stream, Iter};

mod helpers;
pub use self::helpers::{block_on, block_on_deadline};

mod oneshot;

/// Support for creating pollable [`Task`] instances, implementing `Future`
/// and having additional flexibility.
pub mod task;
#[doc(hidden)]
pub use self::task::{message_task, notify_once, ready};

#[doc(hidden)]
pub use self::task::{Task, TaskSender};

/// Utilities for creating [`Waker`](waker::Waker) instances.
#[macro_use]
pub mod waker;
