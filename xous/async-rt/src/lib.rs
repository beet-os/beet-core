//! Minimal async runtime for Xous services.
//!
//! Provides a single-threaded cooperative executor that lets services
//! use `async`/`await` instead of blocking `receive_message` loops.
//!
//! # Example
//!
//! ```rust,no_run
//! use xous_async_rt::{Executor, AsyncServer};
//!
//! let mut executor = Executor::new();
//! let sid = xous::create_server().unwrap();
//!
//! executor.spawn(async move {
//!     let mut server = AsyncServer::new(sid);
//!     loop {
//!         let envelope = server.next().await;
//!         // handle message...
//!     }
//! });
//!
//! executor.run();
//! ```

#![cfg_attr(any(target_os = "none", target_os = "beetos", beetos), no_std)]

extern crate alloc;

use alloc::boxed::Box;
use alloc::vec::Vec;
use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};

use xous::{MessageEnvelope, SID};

// ---------------------------------------------------------------------------
// Noop waker — safe for single-threaded cooperative scheduling.
// The executor polls all non-completed tasks each iteration, so we don't
// need waker-based wake-up tracking. A production version would add
// per-task wakers and a reactor for efficient scheduling.
// ---------------------------------------------------------------------------

fn noop_raw_waker() -> RawWaker {
    fn no_op(_: *const ()) {}
    fn clone(_: *const ()) -> RawWaker {
        noop_raw_waker()
    }
    RawWaker::new(
        core::ptr::null(),
        &RawWakerVTable::new(clone, no_op, no_op, no_op),
    )
}

fn noop_waker() -> Waker {
    // SAFETY: The noop vtable is valid — all operations are no-ops.
    unsafe { Waker::from_raw(noop_raw_waker()) }
}

// ---------------------------------------------------------------------------
// Executor
// ---------------------------------------------------------------------------

struct Task {
    future: Pin<Box<dyn Future<Output = ()>>>,
    completed: bool,
}

/// Single-threaded cooperative async executor for Xous services.
///
/// Spawns tasks with [`spawn`] and drives them to completion with [`run`].
/// Between poll rounds, yields the CPU via `xous::yield_slice()` so other
/// Xous processes can run.
pub struct Executor {
    tasks: Vec<Task>,
}

impl Executor {
    pub fn new() -> Self {
        Self {
            tasks: Vec::new(),
        }
    }

    /// Spawn a new async task on this executor.
    pub fn spawn(&mut self, future: impl Future<Output = ()> + 'static) {
        self.tasks.push(Task {
            future: Box::pin(future),
            completed: false,
        });
    }

    /// Run all spawned tasks to completion.
    ///
    /// This is the main event loop. It polls all tasks, yields when idle,
    /// and returns when every task has resolved.
    pub fn run(&mut self) {
        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);

        loop {
            let mut any_pending = false;
            let mut made_progress = false;

            for task in self.tasks.iter_mut() {
                if task.completed {
                    continue;
                }
                any_pending = true;

                if let Poll::Ready(()) = task.future.as_mut().poll(&mut cx) {
                    task.completed = true;
                    made_progress = true;
                }
            }

            if !any_pending {
                // All tasks completed.
                break;
            }

            if !made_progress {
                // No task made progress — yield CPU to other Xous processes
                // so we don't spin-burn. On next iteration, the poll inside
                // AsyncServer::next() will call try_receive_message again.
                xous::yield_slice();
            }
        }
    }
}

impl Default for Executor {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// AsyncServer — async wrapper around a Xous server SID
// ---------------------------------------------------------------------------

/// Async handle to a Xous server. Yields messages without blocking the thread.
///
/// Instead of the traditional blocking loop:
/// ```rust,ignore
/// loop {
///     let msg = xous::receive_message(sid).unwrap();
///     // ...
/// }
/// ```
///
/// You can write:
/// ```rust,ignore
/// let mut server = AsyncServer::new(sid);
/// loop {
///     let msg = server.next().await;
///     // ...
/// }
/// ```
///
/// The key difference: while `next()` is pending, other tasks on the same
/// executor can make progress.
pub struct AsyncServer {
    sid: SID,
}

impl AsyncServer {
    pub fn new(sid: SID) -> Self {
        Self { sid }
    }

    /// Wait for the next message on this server.
    ///
    /// Returns a future that polls `try_receive_message` (non-blocking) and
    /// yields `Pending` when no message is available.
    pub fn next(&mut self) -> RecvFuture<'_> {
        RecvFuture { server: self }
    }

    /// Get the underlying server ID.
    pub fn sid(&self) -> SID {
        self.sid
    }
}

/// Future returned by [`AsyncServer::next`].
pub struct RecvFuture<'a> {
    server: &'a mut AsyncServer,
}

impl Future for RecvFuture<'_> {
    type Output = MessageEnvelope;

    fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        match xous::try_receive_message(this.server.sid) {
            Ok(Some(envelope)) => Poll::Ready(envelope),
            Ok(None) => Poll::Pending,
            Err(e) => {
                log::warn!("async-rt: try_receive_message error: {:?}", e);
                Poll::Pending
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Async timer — delay that yields while waiting
// ---------------------------------------------------------------------------

/// A future that completes after a given number of milliseconds.
///
/// Uses the system tick count (via `xous::current_time()` if available),
/// falling back to a simple yield-based delay.
pub struct Timer {
    target_ms: u64,
    started: bool,
    start_ms: u64,
}

impl Timer {
    /// Create a timer that completes after `ms` milliseconds.
    pub fn after_ms(ms: u64) -> Self {
        Self {
            target_ms: ms,
            started: false,
            start_ms: 0,
        }
    }
}

impl Future for Timer {
    type Output = ();

    fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<()> {
        let this = self.get_mut();

        if !this.started {
            // Use a simple yield-count approximation.
            // A real implementation would use the ticktimer service.
            this.start_ms = 0;
            this.started = true;
        }

        // Approximate: each poll round after a yield_slice is ~1ms on QEMU.
        // This is intentionally imprecise — a production version would query
        // the ticktimer service for real wall-clock time.
        this.start_ms += 1;
        if this.start_ms >= this.target_ms {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    }
}

// ---------------------------------------------------------------------------
// Utility: select-like combinator for two futures
// ---------------------------------------------------------------------------

/// Drive two futures concurrently, returning whichever completes first.
///
/// This is a minimal `select!` — useful for combining message handling with
/// timeouts or multiple servers.
pub async fn select<A, B, FA, FB>(a: FA, b: FB) -> Either<A, B>
where
    FA: Future<Output = A> + Unpin,
    FB: Future<Output = B> + Unpin,
{
    Select { a, b }.await
}

/// Result of [`select`]: which branch completed first.
pub enum Either<A, B> {
    Left(A),
    Right(B),
}

struct Select<FA, FB> {
    a: FA,
    b: FB,
}

impl<A, B, FA: Future<Output = A> + Unpin, FB: Future<Output = B> + Unpin> Future
    for Select<FA, FB>
{
    type Output = Either<A, B>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();

        if let Poll::Ready(val) = Pin::new(&mut this.a).poll(cx) {
            return Poll::Ready(Either::Left(val));
        }
        if let Poll::Ready(val) = Pin::new(&mut this.b).poll(cx) {
            return Poll::Ready(Either::Right(val));
        }
        Poll::Pending
    }
}
