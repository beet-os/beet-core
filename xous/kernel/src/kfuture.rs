// SPDX-License-Identifier: Apache-2.0
//
//! Kernel futures — compiler-sized async state for in-flight syscalls.
//!
//! Each variant represents a blocking syscall that has been suspended.
//! Instead of encoding the blocked state manually in [`ThreadState`],
//! the kernel stores a `KernelFuture` on the thread.  When the thread
//! is woken (via notification bits), [`activate_current`] polls the
//! future.  If it returns `Ready`, the syscall result is delivered to
//! the thread and execution returns to userspace.
//!
//! This is the BeetOS equivalent of moss-kernel's "kernel work" futures,
//! but without heap allocation — all variants are concrete enum members
//! sized at compile time.
//!
//! ## Safety property
//!
//! Because `KernelFuture` is `Send`, holding a `!Send` spinlock guard
//! across a "yield point" (storing a future) is a compile-time error.
//! This eliminates an entire class of kernel deadlocks.

use xous::{Result, SysCallResult};

use crate::services::SystemServices;

/// Notification bit posted by `send_message_inner` when a message is
/// queued to a server.  Threads waiting on `ReceiveMessage` via a
/// kernel future use this bit as their wakeup signal.
pub const EVENT_SERVER_MSG: usize = 0x1;

/// A suspended kernel operation, stored per-thread.
///
/// This enum is `Send` — any type held across a suspension point must
/// also be `Send`.  This gives us the compile-time guarantee that
/// spinlock guards (which are `!Send`) cannot be held across yield
/// points.
#[derive(Debug)]
pub enum KernelFuture {
    /// Waiting for a message on server index `sidx`.
    ///
    /// Replaces `ThreadState::WaitReceive { sidx }`.
    ReceiveMessage {
        sidx: usize,
    },
}

// SAFETY: All fields are plain data (usize).  The Send bound is the
// key architectural invariant — it prevents !Send types (like spinlock
// guards) from being captured in a kernel future.
unsafe impl Send for KernelFuture {}

/// Result of polling a kernel future.
pub enum PollResult {
    /// The operation completed.  The contained result should be
    /// delivered to the thread via `set_thread_result`.
    Ready(SysCallResult),
    /// The operation is not yet complete.  The thread should be
    /// put back to sleep.
    Pending,
}

impl KernelFuture {
    /// Poll this future against the current kernel state.
    ///
    /// This is NOT `core::future::Future::poll` — there is no `Waker`
    /// or `Pin`.  The kernel's scheduling and notification mechanisms
    /// serve as the waker.
    ///
    /// # Arguments
    ///
    /// * `ss` — mutable access to system services (for server queues)
    /// * `pid` — the PID of the process that owns this future
    pub fn poll(&self, ss: &mut SystemServices, pid: xous::PID) -> PollResult {
        match self {
            KernelFuture::ReceiveMessage { sidx } => {
                let server = match ss.server_from_sidx_mut(*sidx) {
                    Some(s) => s,
                    None => return PollResult::Ready(Err(xous::Error::ServerNotFound)),
                };

                // Ensure the server belongs to this process.
                if server.pid != pid {
                    return PollResult::Ready(Err(xous::Error::ServerNotFound));
                }

                let queue_was_full = server.is_queue_full();

                if let Some(msg) = server.take_next_message(*sidx) {
                    // If the queue was full and now has space, wake any
                    // threads that were blocked on a full queue.
                    if queue_was_full && !server.is_queue_full() {
                        use crate::process::ThreadState;
                        ss.wake_threads_with_state(
                            ThreadState::RetryQueueFull { sidx: *sidx },
                            usize::MAX,
                        );
                    }
                    PollResult::Ready(Ok(Result::MessageEnvelope(msg)))
                } else {
                    PollResult::Pending
                }
            }
        }
    }
}
