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
//!
//! ## Two polling strategies
//!
//! **Queue-based** (`ReceiveMessage`): the future polls an external data
//! structure (server message queue) to find the result.
//!
//! **Mailbox-based** (`WaitBlocking`, `WaitProcess`, `WaitJoin`): the
//! waker deposits the result in the thread's `result_mailbox`; the
//! future simply checks the mailbox.

use xous::{Result, SysCallResult, PID, TID};

use crate::services::SystemServices;

// ── Notification bit constants ────────────────────────────────────────

/// A message was queued to one of this process's servers.
pub const EVENT_SERVER_MSG: usize = 0x1;

/// A kernel event completed (process exit, join, blocking reply, futex).
pub const EVENT_KERNEL: usize = 0x2;

// ── KernelFuture ─────────────────────────────────────────────────────

/// A suspended kernel operation, stored per-thread.
///
/// This enum is `Send` — any type held across a suspension point must
/// also be `Send`.  This gives us the compile-time guarantee that
/// spinlock guards (which are `!Send`) cannot be held across yield
/// points.
#[derive(Debug)]
pub enum KernelFuture {
    /// Waiting for a message on server index `sidx`.
    /// Polls the server queue.  Replaces `ThreadState::WaitReceive`.
    ReceiveMessage { sidx: usize },

    /// Waiting for a blocking IPC reply.
    /// Mailbox-based.  Replaces `ThreadState::WaitBlocking`.
    /// No scan key needed — woken directly by return_result/return_memory
    /// which already know the (pid, tid) from the WaitingMessage.
    WaitBlocking,

    /// Waiting for a process to exit.
    /// Mailbox-based.  Replaces `ThreadState::WaitProcess`.
    /// `target_pid` is the scan key: terminate_current_process scans
    /// all kernel futures for matching target_pid.
    WaitProcessExit { target_pid: xous::PID },

    /// Waiting for a thread to finish (join).
    /// Mailbox-based.  Replaces `ThreadState::WaitJoin`.
    /// `target_tid` is the scan key: thread_exited scans the current
    /// process's kernel futures for matching target_tid.
    WaitJoin { target_tid: usize },

    /// Waiting on a futex.
    /// Mailbox-based.  Replaces `ThreadState::WaitFutex`.
    /// `addr` is the scan key: FutexWake scans the current process's
    /// kernel futures for matching addr.
    WaitFutex { addr: usize },
}

// All fields are plain data (PID, TID, usize) which are Send.
// The compiler derives Send automatically.  If a future variant ever
// captures a !Send type (e.g. a spinlock guard), the compiler will
// refuse — that's the whole point.
const _: () = { fn _assert_send<T: Send>() {} fn _check() { _assert_send::<KernelFuture>(); } };

// ── PollResult ───────────────────────────────────────────────────────

/// Result of polling a kernel future.
pub enum PollResult {
    /// The operation completed.  The contained result should be
    /// delivered to the thread via `set_thread_result`.
    Ready(SysCallResult),
    /// The operation is not yet complete.  The thread should be
    /// put back to sleep.
    Pending,
}

// ── Poll implementation ──────────────────────────────────────────────

impl KernelFuture {
    /// Return the `WaitEvent` mask that should be used when suspending
    /// a thread for this future, and restored when re-parking after a
    /// `Pending` poll result.
    pub fn suspension_mask(&self) -> usize {
        match self {
            // Woken when a message arrives in the server's queue.
            KernelFuture::ReceiveMessage { .. } => EVENT_SERVER_MSG,
            // Woken by a kernel event: blocking-IPC reply, process/thread
            // exit, or futex wake.  Must match the mask passed to
            // suspend_with_future (EVENT_KERNEL) so re-parking after a
            // Pending poll uses the same mask and avoids spurious wakeups
            // from unrelated server-message notifications.
            KernelFuture::WaitBlocking
            | KernelFuture::WaitProcessExit { .. }
            | KernelFuture::WaitJoin { .. }
            | KernelFuture::WaitFutex { .. } => EVENT_KERNEL,
        }
    }

    /// Poll this future against the current kernel state.
    ///
    /// This is NOT `core::future::Future::poll` — there is no `Waker`
    /// or `Pin`.  The kernel's scheduling and notification mechanisms
    /// serve as the waker.
    ///
    /// For queue-based futures, `poll` checks the relevant data
    /// structure.  For mailbox-based futures, it checks the thread's
    /// `result_mailbox`.
    pub fn poll(
        &self,
        ss: &mut SystemServices,
        pid: PID,
        tid: TID,
    ) -> PollResult {
        match self {
            // ── Queue-based ──────────────────────────────────────
            KernelFuture::ReceiveMessage { sidx } => {
                poll_receive_message(ss, pid, *sidx)
            }

            // ── Mailbox-based ────────────────────────────────────
            KernelFuture::WaitBlocking
            | KernelFuture::WaitProcessExit { .. }
            | KernelFuture::WaitJoin { .. }
            | KernelFuture::WaitFutex { .. } => {
                poll_mailbox(ss, pid, tid)
            }
        }
    }
}

/// Poll the server message queue for a pending message.
fn poll_receive_message(
    ss: &mut SystemServices,
    pid: PID,
    sidx: usize,
) -> PollResult {
    let server = match ss.server_from_sidx_mut(sidx) {
        Some(s) => s,
        None => return PollResult::Ready(Err(xous::Error::ServerNotFound)),
    };

    if server.pid != pid {
        return PollResult::Ready(Err(xous::Error::ServerNotFound));
    }

    let queue_was_full = server.is_queue_full();

    if let Some(msg) = server.take_next_message(sidx) {
        if queue_was_full && !server.is_queue_full() {
            use crate::process::ThreadState;
            ss.wake_threads_with_state(
                ThreadState::RetryQueueFull { sidx },
                usize::MAX,
            );
        }
        PollResult::Ready(Ok(Result::MessageEnvelope(msg)))
    } else {
        PollResult::Pending
    }
}

/// Check the thread's result mailbox.
fn poll_mailbox(
    ss: &mut SystemServices,
    pid: PID,
    tid: TID,
) -> PollResult {
    match ss.process_mut(pid) {
        Ok(process) => match process.take_mailbox(tid) {
            Some(result) => PollResult::Ready(Ok(result)),
            None => PollResult::Pending,
        },
        Err(e) => PollResult::Ready(Err(e)),
    }
}
