//! Async wrapper around a Xous server SID.
//!
//! [`AsyncServer`] registers its SID with the global reactor so that
//! incoming messages are buffered and the waiting task is woken
//! efficiently — no busy-poll, no extra threads.

use core::fmt;
use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll};

use xous::{MessageEnvelope, SID};

use crate::reactor;

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

/// Error returned by [`RecvFuture`] when the server is no longer available.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecvError {
    /// The server slot was unregistered (server destroyed or handle dropped
    /// while a `next()` future was still alive — shouldn't happen in
    /// normal use).
    ServerGone,
}

impl fmt::Display for RecvError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RecvError::ServerGone => f.write_str("server no longer registered with reactor"),
        }
    }
}

// ---------------------------------------------------------------------------
// AsyncServer
// ---------------------------------------------------------------------------

/// Async handle to a Xous server.
///
/// Instead of the traditional blocking loop:
///
/// ```rust,ignore
/// loop {
///     let msg = xous::receive_message(sid).unwrap();
///     handle(msg);
/// }
/// ```
///
/// Write:
///
/// ```rust,ignore
/// let mut server = AsyncServer::new(sid);
/// loop {
///     let msg = server.next().await?;
///     handle(msg);
/// }
/// ```
///
/// While `next()` is pending, other tasks on the same executor make
/// progress.
pub struct AsyncServer {
    sid: SID,
    slot: reactor::SlotId,
}

impl AsyncServer {
    /// Create a new async server handle and register with the reactor.
    pub fn new(sid: SID) -> Self {
        let slot = reactor::register_server(sid);
        Self { sid, slot }
    }

    /// Wait for the next message.
    ///
    /// Returns `Ok(envelope)` when a message arrives, or `Err(RecvError)`
    /// if the server slot is no longer active.
    pub fn next(&mut self) -> RecvFuture<'_> {
        RecvFuture { server: self }
    }

    /// Get the underlying server ID.
    pub fn sid(&self) -> SID {
        self.sid
    }
}

impl Drop for AsyncServer {
    fn drop(&mut self) {
        reactor::unregister_server(self.slot);
    }
}

// ---------------------------------------------------------------------------
// RecvFuture
// ---------------------------------------------------------------------------

/// Future returned by [`AsyncServer::next`].
pub struct RecvFuture<'a> {
    server: &'a mut AsyncServer,
}

impl Future for RecvFuture<'_> {
    type Output = Result<MessageEnvelope, RecvError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();

        // Check the slot is still alive.
        if !reactor::server_active(this.server.slot) {
            return Poll::Ready(Err(RecvError::ServerGone));
        }

        // Fast path: buffered message or direct non-blocking receive.
        if let Some(msg) = reactor::try_recv(this.server.slot) {
            return Poll::Ready(Ok(msg));
        }

        // Slow path: register waker so the reactor wakes us when a
        // message arrives during its poll_servers phase.
        reactor::set_server_waker(this.server.slot, cx.waker().clone());
        Poll::Pending
    }
}
