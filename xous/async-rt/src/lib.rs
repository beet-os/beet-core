//! Minimal async runtime for Xous services.
//!
//! Provides a single-threaded cooperative executor that lets services
//! use `async`/`await` instead of blocking `receive_message` loops.
//!
//! # Architecture
//!
//! ```text
//! ┌──────────────────────────────────────────────┐
//! │                  Executor                     │
//! │  ┌────────┐ ┌────────┐ ┌────────┐            │
//! │  │ Task 1 │ │ Task 2 │ │ Task 3 │  ...       │
//! │  │(waker) │ │(waker) │ │(waker) │            │
//! │  └───┬────┘ └───┬────┘ └───┬────┘            │
//! │      │          │          │                  │
//! │  ┌───▼──────────▼──────────▼────────────────┐│
//! │  │              Reactor                      ││
//! │  │  servers: [SID_A, SID_B, ...]             ││
//! │  │  timers:  [deadline_1, deadline_2, ...]   ││
//! │  │                                           ││
//! │  │  poll_all():                              ││
//! │  │    try_receive_message(SID_A) → wake T1   ││
//! │  │    try_receive_message(SID_B) → wake T2   ││
//! │  │    tick++ → check deadlines   → wake T3   ││
//! │  └───────────────────────────────────────────┘│
//! └──────────────────────────────────────────────┘
//! ```
//!
//! # Example
//!
//! ```rust,no_run
//! use xous_async_rt::{Executor, AsyncServer, Timer, select, Either};
//!
//! let mut exec = Executor::new();
//! let sid = xous::create_server().unwrap();
//!
//! exec.spawn(async move {
//!     let mut server = AsyncServer::new(sid);
//!     loop {
//!         match select(server.next(), Timer::after(1000)).await {
//!             Either::Left(msg) => { /* handle message */ }
//!             Either::Right(()) => { /* timeout */ }
//!         }
//!     }
//! });
//!
//! exec.run();
//! ```

#![cfg_attr(any(target_os = "none", target_os = "beetos", beetos), no_std)]

extern crate alloc;

mod combinators;
mod executor;
mod reactor;
mod server;
mod timer;
mod waker;

pub use combinators::{join, join3, select, Either, Join, Join3, Select};
pub use executor::Executor;
pub use server::{AsyncServer, RecvFuture};
pub use timer::Timer;
