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
//! │  │  spawn_queue: [new tasks from Spawner]    ││
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
//! // Spawn a task that can itself spawn more tasks
//! let spawner = exec.spawner();
//! exec.spawn(async move {
//!     let mut server = AsyncServer::new(sid);
//!     loop {
//!         match select(server.next(), Timer::after(1000)).await {
//!             Either::Left(Ok(msg))  => { /* handle message */ }
//!             Either::Left(Err(_))   => break, // server gone
//!             Either::Right(())      => { /* timeout */ }
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

#[cfg(test)]
mod tests;

pub use combinators::{join, join3, join_all, select, BoxFuture, Either, Join, Join3, JoinAll, Select};
pub use executor::{Executor, Spawner};
pub use server::{AsyncServer, RecvError, RecvFuture};
pub use timer::Timer;
