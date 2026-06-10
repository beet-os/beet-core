// SPDX-FileCopyrightText: 2025 BeetOS contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! BeetOS userspace TCP socket API.
//!
//! Mirrors the rough shape of `std::net::{TcpListener, TcpStream}`,
//! but every operation is non-blocking: the kernel runs the TCP
//! state machine in the background and userspace polls for state.
//!
//! Each function is a thin wrapper over a `SysCall::NetSocket*`
//! variant. Bytes ride inline in the syscall args (max 32 bytes per
//! send/recv call) so no user pointer crosses the EL0→EL1 boundary
//! — keeps the kernel handler PAN-safe without any copy-from-user
//! helper.
//!
//! # Example
//!
//! ```no_run
//! use beetos_api_net::{SockState, TcpListener};
//!
//! let listener = TcpListener::bind(7000).expect("listen");
//! loop {
//!     if let Some(mut stream) = listener.try_accept() {
//!         let mut buf = [0u8; 32];
//!         while stream.state() == SockState::Established {
//!             let n = stream.recv(&mut buf);
//!             if n > 0 {
//!                 stream.send(&buf[..n]);
//!             }
//!             xous::yield_slice();
//!         }
//!         stream.close();
//!     }
//!     xous::yield_slice();
//! }
//! ```

#![no_std]
// pack/unpack helpers and syscall imports are only used in the
// `cfg(beetos)` syscall branches; on hosted builds (used by tests)
// the syscalls are stubbed out so they look unused.
#![cfg_attr(not(beetos), allow(dead_code, unused_imports))]

use xous::{Result, SysCall};

/// Sentinel returned by `accept` when no connection is ready.
/// Matches `tcp::NO_PENDING` on the kernel side (u32::MAX, sign-extended
/// to usize on 64-bit). Encoding the sentinel as u32 keeps it the same
/// numeric value whether userspace is built for 32- or 64-bit.
pub const NO_PENDING: usize = u32::MAX as usize;

/// Maximum bytes per `send` / `recv` call (limited by the four
/// scalar-arg payload slots).
pub const CHUNK: usize = 32;

/// Mirror of the kernel's `SockState`. Decoded from the numeric
/// code returned by `NetSocketStatus`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SockState {
    Free,
    Closed,
    Listen,
    SynSent,
    SynReceived,
    Established,
    FinWait1,
    FinWait2,
    Closing,
    LastAck,
    TimeWait,
    CloseWait,
    Unknown(usize),
}

impl SockState {
    pub fn from_code(code: usize) -> Self {
        match code {
            0 => SockState::Free,
            1 => SockState::Closed,
            2 => SockState::Listen,
            3 => SockState::SynSent,
            4 => SockState::SynReceived,
            5 => SockState::Established,
            6 => SockState::FinWait1,
            7 => SockState::FinWait2,
            8 => SockState::Closing,
            9 => SockState::LastAck,
            10 => SockState::TimeWait,
            11 => SockState::CloseWait,
            other => SockState::Unknown(other),
        }
    }
}

fn pack_chunk(s: &[u8]) -> ([usize; 4], usize) {
    let n = s.len().min(CHUNK);
    let mut words = [0u8; CHUNK];
    words[..n].copy_from_slice(&s[..n]);
    let mut out = [0usize; 4];
    for i in 0..4 {
        let mut word = 0u64;
        for j in 0..8 {
            word |= (words[i * 8 + j] as u64) << (j * 8);
        }
        out[i] = word as usize;
    }
    (out, n)
}

fn unpack_chunk(args: [usize; 4], out: &mut [u8]) {
    let n = out.len().min(CHUNK);
    let mut all = [0u8; CHUNK];
    for i in 0..4 {
        let word = args[i] as u64;
        for j in 0..8 {
            all[i * 8 + j] = ((word >> (j * 8)) & 0xff) as u8;
        }
    }
    out[..n].copy_from_slice(&all[..n]);
}

fn raw_create() -> Option<usize> {
    #[cfg(beetos)]
    {
        match xous::rsyscall(SysCall::NetSocketCreate) {
            Ok(Result::Scalar1(sock)) => Some(sock),
            _ => None,
        }
    }
    #[cfg(not(beetos))]
    {
        None
    }
}

fn raw_listen(sock: usize, port: u16) -> bool {
    #[cfg(beetos)]
    {
        matches!(
            xous::rsyscall(SysCall::NetSocketListen(sock, port as usize)),
            Ok(Result::Ok)
        )
    }
    #[cfg(not(beetos))]
    {
        let _ = (sock, port);
        false
    }
}

fn raw_accept(sock: usize) -> Option<usize> {
    #[cfg(beetos)]
    {
        match xous::rsyscall(SysCall::NetSocketAccept(sock)) {
            Ok(Result::Scalar1(child)) if child != NO_PENDING => Some(child),
            _ => None,
        }
    }
    #[cfg(not(beetos))]
    {
        let _ = sock;
        None
    }
}

fn raw_connect(sock: usize, peer_ip: [u8; 4], peer_port: u16) -> bool {
    #[cfg(beetos)]
    {
        let ip_be = u32::from_be_bytes(peer_ip) as usize;
        matches!(
            xous::rsyscall(SysCall::NetSocketConnect(sock, ip_be, peer_port as usize)),
            Ok(Result::Ok)
        )
    }
    #[cfg(not(beetos))]
    {
        let _ = (sock, peer_ip, peer_port);
        false
    }
}

fn raw_status(sock: usize) -> (SockState, usize) {
    #[cfg(beetos)]
    {
        match xous::rsyscall(SysCall::NetSocketStatus(sock)) {
            Ok(Result::Scalar2(code, rx)) => (SockState::from_code(code), rx),
            _ => (SockState::Unknown(usize::MAX), 0),
        }
    }
    #[cfg(not(beetos))]
    {
        let _ = sock;
        (SockState::Unknown(usize::MAX), 0)
    }
}

fn raw_send(sock: usize, bytes: &[u8]) -> usize {
    #[cfg(beetos)]
    {
        let (args, len) = pack_chunk(bytes);
        match xous::rsyscall(SysCall::NetSocketSend(
            sock, len, args[0], args[1], args[2], args[3],
        )) {
            Ok(Result::Scalar1(n)) => n,
            _ => 0,
        }
    }
    #[cfg(not(beetos))]
    {
        let _ = (sock, bytes);
        0
    }
}

fn raw_recv(sock: usize, out: &mut [u8]) -> usize {
    #[cfg(beetos)]
    {
        let max = out.len().min(CHUNK);
        match xous::rsyscall(SysCall::NetSocketRecv(sock, max)) {
            Ok(Result::Scalar5(n, w0, w1, w2, w3)) => {
                let copy = n.min(out.len());
                unpack_chunk([w0, w1, w2, w3], &mut out[..copy]);
                copy
            }
            _ => 0,
        }
    }
    #[cfg(not(beetos))]
    {
        let _ = (sock, out);
        0
    }
}

fn raw_close(sock: usize) {
    #[cfg(beetos)]
    {
        let _ = xous::rsyscall(SysCall::NetSocketClose(sock));
    }
    #[cfg(not(beetos))]
    {
        let _ = sock;
    }
}

// ============================================================================
// TcpListener
// ============================================================================

/// A TCP listener bound to a local port. Created via [`TcpListener::bind`].
pub struct TcpListener {
    sock: usize,
}

impl TcpListener {
    /// Bind a fresh socket to `port` and put it in LISTEN. Returns
    /// `None` if the socket table is full or the port is reserved.
    pub fn bind(port: u16) -> Option<Self> {
        let sock = raw_create()?;
        if !raw_listen(sock, port) {
            raw_close(sock);
            return None;
        }
        Some(Self { sock })
    }

    /// Non-blocking accept. Returns `Some(TcpStream)` only when the
    /// three-way handshake has completed for a queued peer.
    pub fn try_accept(&self) -> Option<TcpStream> {
        raw_accept(self.sock).map(|child| TcpStream { sock: child })
    }

    pub fn state(&self) -> SockState {
        raw_status(self.sock).0
    }
}

impl Drop for TcpListener {
    fn drop(&mut self) {
        raw_close(self.sock);
    }
}

// ============================================================================
// TcpStream
// ============================================================================

/// A connected (or being-connected) TCP socket. Returned by
/// `TcpListener::try_accept` for inbound, or [`TcpStream::connect`]
/// for outbound.
pub struct TcpStream {
    sock: usize,
}

impl TcpStream {
    /// Initiate an active open to `(peer_ip, peer_port)`. Returns
    /// immediately — the SYN may not yet have been transmitted (the
    /// kernel still has to ARP the next hop). Poll [`Self::state`]
    /// until it reaches `Established`.
    pub fn connect(peer_ip: [u8; 4], peer_port: u16) -> Option<Self> {
        let sock = raw_create()?;
        if !raw_connect(sock, peer_ip, peer_port) {
            raw_close(sock);
            return None;
        }
        Some(Self { sock })
    }

    /// Current state of the underlying socket.
    pub fn state(&self) -> SockState {
        raw_status(self.sock).0
    }

    /// Number of bytes currently buffered for `recv`.
    pub fn available(&self) -> usize {
        raw_status(self.sock).1
    }

    /// Queue up to `CHUNK` (32) bytes for transmission. Returns the
    /// number actually buffered.
    pub fn send(&mut self, bytes: &[u8]) -> usize {
        raw_send(self.sock, bytes)
    }

    /// Send all of `bytes`, looping over chunks. Returns the total
    /// sent (may be less than `bytes.len()` if the socket closes
    /// mid-write).
    pub fn send_all(&mut self, bytes: &[u8]) -> usize {
        let mut sent = 0;
        while sent < bytes.len() {
            let chunk = &bytes[sent..(sent + CHUNK).min(bytes.len())];
            let n = self.send(chunk);
            if n == 0 {
                // TX ring full or socket closed; yield and let it
                // drain on the next timer tick.
                xous::yield_slice();
                if !matches!(self.state(), SockState::Established | SockState::CloseWait) {
                    break;
                }
            } else {
                sent += n;
            }
        }
        sent
    }

    /// Drain up to `CHUNK` bytes from the RX queue. Returns 0 if no
    /// data is available — call [`Self::state`] to distinguish
    /// "no bytes yet" from "peer closed".
    pub fn recv(&mut self, out: &mut [u8]) -> usize {
        raw_recv(self.sock, out)
    }

    /// Initiate FIN. The socket transitions through FinWait/TimeWait;
    /// caller can `drop` immediately or keep the handle to observe.
    pub fn close(self) {
        raw_close(self.sock);
        core::mem::forget(self); // suppress Drop's redundant close
    }
}

impl Drop for TcpStream {
    fn drop(&mut self) {
        raw_close(self.sock);
    }
}
