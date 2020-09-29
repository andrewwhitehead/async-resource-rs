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
//! The [`Suspend`] structure may be used to coordinate between threads and
//! `Futures`, allowing either to act as a listener or notifier:
//!
//! ```
//! use std::time::Duration;
//!
//! let mut susp = suspend::Suspend::new();
//! let notifier = susp.notifier();
//!
//! // start listening for notifications
//! let listener = susp.listen();
//! // send a notification (satisfies the current listener)
//! notifier.notify();
//! // wait for notification (already sent) with a timeout
//! assert!(listener.wait_timeout(Duration::from_secs(1)).is_ok());
//!
//! let listener = susp.listen();
//! notifier.notify();
//! // the listener is also a Future
//! suspend::block_on(async { listener.await });
//! ```
//!
//! It can also be used to directly poll a `Future`. If the future does
//! not return immediately, then the `Suspend` will be notified when
//! the future is ready to be polled again:
//!
//! ```
//! use futures_lite::pin;
//! let mut susp = suspend::Suspend::new();
//! let mut task = async { 99 };
//! pin!(task);
//! assert_eq!(susp.poll_future(task), Ok(99));
//! ```

#[doc(hidden)]
pub mod re_export {
    pub use std::pin::Pin;
}

pub mod async_stream;

mod channel;

pub use self::channel::{channel, send_once, ReceiveOnce, Receiver, SendOnce, Sender};

mod core;
pub use self::core::Suspend;

mod error;
pub use self::error::{Incomplete, TimedOut};

/// Providing a convenient wrapper around `Stream` types.
mod iter;
pub use self::iter::{iter_poll_fn, iter_stream, iter_stream_unpin, IterStream};

mod helpers;
pub use self::helpers::{block_on, block_on_deadline};

mod notify;

pub use self::notify::{notify_once, ListenOnce, Listener, Notifier, NotifyOnce};

pub mod thread;

mod util;

/// Utilities for creating [`Waker`](waker::Waker) instances.
#[macro_use]
pub mod waker;
