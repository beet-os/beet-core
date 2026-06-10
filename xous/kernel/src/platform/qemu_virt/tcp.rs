// SPDX-FileCopyrightText: 2025 BeetOS contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! TCP for BeetOS (QEMU virt).
//!
//! Two coexisting datapaths share the same low-level segment builder:
//!
//!   1. **Kernel-managed remote console** on the well-known
//!      [`LISTEN_PORT`] (2323). Passive-open only. The single
//!      [`CONN`] static + 4 KiB [`RING`] back the M7 remote shell —
//!      received bytes go straight into the shell input dispatcher,
//!      writes come from the [`SysCall::NetConsolePush`] syscall.
//!      The smoke test `qemu-smoke-net` exercises this path end-to-end.
//!
//!   2. **Userspace sockets** ([`UserSocket`] table). Apps create
//!      sockets via syscalls and drive listen / accept / connect /
//!      send / recv / close themselves. Per-socket RX and TX rings,
//!      explicit ephemeral local port allocation, full active-open
//!      ARP-then-SYN dance for outbound connections.
//!
//! [`handle_segment`] dispatches an inbound TCP segment to whichever
//! datapath claims the destination port: port 2323 → console;
//! anything else → userspace table lookup. The two paths share
//! [`build_and_send`] (frame assembly + TCP checksum) and
//! [`internet_checksum`] from `net_stack`.
//!
//! ## Hardening invariants (userspace sockets)
//!
//! - **Ownership**: every syscall-facing operation takes the caller's
//!   PID and refuses to touch a socket another process created.
//!   Listener children inherit the listener's owner.
//! - **No slot leaks**: sockets parked in FinWait1/2, Closing,
//!   LastAck, TimeWait, an embryonic SynReceived, or a Closed slot
//!   nobody can re-observe are reaped [`REAP_TICKS`] after entering
//!   the state (we have no retransmit, so a lost final ACK must not
//!   pin a slot forever). Process exit reclaims everything the dead
//!   PID owned via [`cleanup_for_pid`].
//! - **Honest flow control**: the advertised window is the actual
//!   free space in the per-socket RX ring (capped at [`RCV_WINDOW`]).
//!   Inbound payload is accepted only up to capacity and the ACK
//!   covers exactly what was stored — the peer's retransmit fills the
//!   rest once `recv` reopens the window (a window-update ACK is sent
//!   when a previously-starved ring is drained).
//! - **Sequence-checked FIN**: a FIN only counts when its sequence
//!   slot lines up with `rcv_nxt`, so retransmitted or out-of-order
//!   FINs can't corrupt the stream position. Retransmitted FINs in
//!   CloseWait/TimeWait get a fresh ACK so the peer stops resending.
//!
//! ## Deliberate limitations (v1)
//!
//! - **No retransmission / no RTO.** QEMU user-mode networking is a
//!   loopback-quality link; segments don't get lost between host and
//!   guest. A real NIC port will need a real retransmit timer.
//! - **No out-of-order reassembly, no window scaling.** In-order
//!   delivery assumed; anything else gets a duplicate ACK.
//! - **No SACK, no Nagle, no delayed ACK.**
//! - **No RFC 5961 RST validation.** Blind-RST injection isn't in the
//!   threat model for a slirp loopback link.
//! - **Single pending accept per listener.** A second SYN while one
//!   is still in handshake gets RST (host connect() fails fast
//!   instead of hanging on a backlog we don't have).
//! - **`MAX_USERSPACE_SOCKETS` (= 4) sockets** in the static table.

use super::{net, net_stack};

/// Port the kernel remote console listens on. 2323 is unprivileged
/// and avoids clashing with a host telnet on 23.
pub const LISTEN_PORT: u16 = 2323;

/// Ceiling on the receive window we advertise. The console always
/// advertises this; userspace sockets advertise the *actual* free
/// space of their RX ring capped here.
const RCV_WINDOW: u16 = 1400;

/// Largest payload we put in a single outbound segment. Keeps
/// ETH+IP+TCP+payload under the 1518-byte frame ceiling with margin.
const MAX_SEG_PAYLOAD: usize = 1200;

// TCP flag bits (byte 13 of the TCP header).
const FIN: u8 = 0x01;
const SYN: u8 = 0x02;
const RST: u8 = 0x04;
const PSH: u8 = 0x08;
const ACK: u8 = 0x10;

// ============================================================================
// Kernel remote console (port 2323)
// ============================================================================

#[derive(Clone, Copy, PartialEq)]
enum ConsoleState {
    Listen,
    SynReceived,
    Established,
    LastAck,
}

struct ConsoleConn {
    state: ConsoleState,
    peer_mac: [u8; 6],
    peer_ip: [u8; 4],
    peer_port: u16,
    snd_nxt: u32,
    rcv_nxt: u32,
}

static mut CONN: ConsoleConn = ConsoleConn {
    state: ConsoleState::Listen,
    peer_mac: [0; 6],
    peer_ip: [0; 4],
    peer_port: 0,
    snd_nxt: 0,
    rcv_nxt: 0,
};

/// A small, deterministic Initial Send Sequence. RFC 6528 says
/// randomise; QEMU loopback doesn't care and we'd have to pull in
/// the RNG here.
static mut ISS_COUNTER: u32 = 0x1000;

const RING_SIZE: usize = 4096;

struct ConsoleRing {
    buf: [u8; RING_SIZE],
    head: usize,
    tail: usize,
}

static mut RING: ConsoleRing = ConsoleRing {
    buf: [0u8; RING_SIZE],
    head: 0,
    tail: 0,
};

fn next_iss() -> u32 {
    unsafe {
        ISS_COUNTER = ISS_COUNTER.wrapping_add(0x4000);
        ISS_COUNTER
    }
}

// ============================================================================
// Userspace socket table
// ============================================================================

/// Cap on userspace sockets. ~2 KiB each → 8 KiB total in BSS.
const MAX_USERSPACE_SOCKETS: usize = 4;

const SOCK_RX_BUF: usize = 1024;
const SOCK_TX_BUF: usize = 1024;

/// Sentinel returned from `accept` when no connection is ready.
pub const NO_PENDING: u32 = u32::MAX;

/// Ticks (100 Hz) before a socket stuck in a terminal-ish state
/// (FinWait/Closing/LastAck/TimeWait/embryonic SynReceived) is
/// reclaimed. 5 s is generous for a loopback link where every
/// handshake completes in milliseconds.
const REAP_TICKS: u64 = 500;

/// Ticks between ARP retries for a `connect` whose next hop hasn't
/// resolved yet: 4 broadcasts/s instead of one per tick.
const ARP_RETRY_TICKS: u64 = 25;

/// Coarse current-tick mirror, refreshed by [`tick_flush_user`]
/// (100 Hz). Segment handlers run from the same timer IRQ a few calls
/// earlier in the chain, so reads are at most one tick (10 ms) stale —
/// irrelevant against the multi-second reap deadlines it feeds.
static mut NOW_TICK: u64 = 0;

fn now_tick() -> u64 {
    unsafe { NOW_TICK }
}

/// Ephemeral local port allocator. Starts in the IANA dynamic range.
static mut NEXT_EPHEMERAL_PORT: u16 = 49152;

/// True if any live socket already claims `port` as its local port.
fn port_in_use(port: u16) -> bool {
    sockets().iter().any(|s| !matches!(s.state, SockState::Free) && s.local_port == port)
}

fn alloc_ephemeral_port() -> u16 {
    // With at most MAX_USERSPACE_SOCKETS ports busy, a handful of
    // probes always finds a free one.
    unsafe {
        for _ in 0..(MAX_USERSPACE_SOCKETS + 2) {
            let p = NEXT_EPHEMERAL_PORT;
            NEXT_EPHEMERAL_PORT = NEXT_EPHEMERAL_PORT.wrapping_add(1);
            if NEXT_EPHEMERAL_PORT < 49152 {
                NEXT_EPHEMERAL_PORT = 49152;
            }
            if !port_in_use(p) {
                return p;
            }
        }
        NEXT_EPHEMERAL_PORT // unreachable with a 16k range and 4 sockets
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum SockState {
    Free,
    Listen,
    SynSent,
    SynReceived,
    Established,
    /// Peer sent FIN; app hasn't called close yet. Can still send.
    CloseWait,
    FinWait1,
    FinWait2,
    Closing,
    LastAck,
    TimeWait,
    Closed,
}

impl SockState {
    /// Stable numeric code for the `NetSocketStatus` syscall.
    /// Userspace decodes via `api::net::SockState::from_code`.
    pub fn code(self) -> usize {
        match self {
            SockState::Free => 0,
            SockState::Closed => 1,
            SockState::Listen => 2,
            SockState::SynSent => 3,
            SockState::SynReceived => 4,
            SockState::Established => 5,
            SockState::CloseWait => 11,
            SockState::FinWait1 => 6,
            SockState::FinWait2 => 7,
            SockState::Closing => 8,
            SockState::LastAck => 9,
            SockState::TimeWait => 10,
        }
    }
}

struct UserSocket {
    state: SockState,
    /// Process that created this socket (children inherit it from
    /// their listener). Every syscall operation checks the caller
    /// against this; [`cleanup_for_pid`] reclaims on process exit.
    owner_pid: u32,
    local_port: u16,
    peer_mac: [u8; 6],
    peer_ip: [u8; 4],
    peer_port: u16,
    snd_nxt: u32,
    rcv_nxt: u32,
    /// Listener bookkeeping: slot index of an in-progress or accepted
    /// child connection. `NO_PENDING` if no pending child. Only one
    /// pending child per listener for V1 (no backlog).
    pending_child: u32,
    /// Set when `connect` has been called but we don't yet have the
    /// peer's MAC (ARP request outstanding). The state stays at
    /// SynSent until ARP resolves and we send the SYN.
    awaiting_arp: bool,
    /// Tick at which the reaper may reclaim this slot; 0 = no
    /// deadline. Set on entry into the wind-down states and on
    /// embryonic (SynReceived) children; cleared on Established.
    expiry_tick: u64,
    rx_buf: [u8; SOCK_RX_BUF],
    rx_head: usize,
    rx_tail: usize,
    tx_buf: [u8; SOCK_TX_BUF],
    tx_head: usize,
    tx_tail: usize,
}

impl UserSocket {
    const fn empty() -> Self {
        Self {
            state: SockState::Free,
            owner_pid: 0,
            local_port: 0,
            peer_mac: [0; 6],
            peer_ip: [0; 4],
            peer_port: 0,
            snd_nxt: 0,
            rcv_nxt: 0,
            pending_child: NO_PENDING,
            awaiting_arp: false,
            expiry_tick: 0,
            rx_buf: [0; SOCK_RX_BUF],
            rx_head: 0,
            rx_tail: 0,
            tx_buf: [0; SOCK_TX_BUF],
            tx_head: 0,
            tx_tail: 0,
        }
    }

    fn rx_len(&self) -> usize {
        (self.rx_head + SOCK_RX_BUF - self.rx_tail) % SOCK_RX_BUF
    }

    fn rx_free(&self) -> usize {
        SOCK_RX_BUF - 1 - self.rx_len()
    }

    /// Append as much of `payload` as fits. Returns the byte count
    /// actually stored — the caller advances `rcv_nxt` by exactly
    /// this much, so the ACK never covers bytes we dropped. The
    /// peer's retransmit delivers the remainder once the app drains
    /// the ring and the window reopens.
    fn rx_accept(&mut self, payload: &[u8]) -> usize {
        let mut n = 0;
        for &b in payload {
            let next = (self.rx_head + 1) % SOCK_RX_BUF;
            if next == self.rx_tail {
                break;
            }
            self.rx_buf[self.rx_head] = b;
            self.rx_head = next;
            n += 1;
        }
        n
    }

    fn rx_drain(&mut self, out: &mut [u8]) -> usize {
        let mut n = 0;
        while n < out.len() && self.rx_head != self.rx_tail {
            out[n] = self.rx_buf[self.rx_tail];
            self.rx_tail = (self.rx_tail + 1) % SOCK_RX_BUF;
            n += 1;
        }
        n
    }

    fn tx_push(&mut self, bytes: &[u8]) -> usize {
        let mut n = 0;
        for &b in bytes {
            let next = (self.tx_head + 1) % SOCK_TX_BUF;
            if next == self.tx_tail {
                // TX ring full; stop here and let the caller retry.
                break;
            }
            self.tx_buf[self.tx_head] = b;
            self.tx_head = next;
            n += 1;
        }
        n
    }

    fn tx_drain(&mut self, out: &mut [u8]) -> usize {
        let mut n = 0;
        while n < out.len() && self.tx_head != self.tx_tail {
            out[n] = self.tx_buf[self.tx_tail];
            self.tx_tail = (self.tx_tail + 1) % SOCK_TX_BUF;
            n += 1;
        }
        n
    }

    fn tx_has_data(&self) -> bool {
        self.tx_head != self.tx_tail
    }
}

static mut USER_SOCKETS: [UserSocket; MAX_USERSPACE_SOCKETS] = [
    UserSocket::empty(),
    UserSocket::empty(),
    UserSocket::empty(),
    UserSocket::empty(),
];

/// SAFETY: single-threaded kernel; all socket operations happen with
/// IRQs masked (syscall entry) or from the timer IRQ (which preempts
/// nothing inside the kernel itself). Each call derives a fresh
/// reference from the raw pointer; callers scope their borrows so
/// uses never interleave.
fn sockets_mut() -> &'static mut [UserSocket; MAX_USERSPACE_SOCKETS] {
    unsafe { &mut *(&raw mut USER_SOCKETS) }
}

fn sockets() -> &'static [UserSocket; MAX_USERSPACE_SOCKETS] {
    unsafe { &*(&raw const USER_SOCKETS) }
}

fn alloc_socket(owner_pid: u32) -> Option<usize> {
    let socks = sockets_mut();
    for (i, s) in socks.iter_mut().enumerate() {
        if matches!(s.state, SockState::Free) {
            *s = UserSocket::empty();
            s.state = SockState::Closed;
            s.owner_pid = owner_pid;
            return Some(i);
        }
    }
    None
}

fn find_listener(port: u16) -> Option<usize> {
    sockets().iter().position(|s| {
        matches!(s.state, SockState::Listen) && s.local_port == port
    })
}

fn find_matching(local_port: u16, peer_ip: [u8; 4], peer_port: u16) -> Option<usize> {
    sockets().iter().position(|s| {
        s.local_port == local_port
            && s.peer_port == peer_port
            && s.peer_ip == peer_ip
            && !matches!(s.state, SockState::Free | SockState::Listen)
    })
}

fn mark_expiry(s: &mut UserSocket) {
    s.expiry_tick = now_tick() + REAP_TICKS;
}

/// If a listener's pending child died mid-handshake (RST, reap), clear
/// the stale reference so the listener can serve the next SYN instead
/// of being wedged forever.
fn reclaim_stale_child(listener_idx: usize) {
    let pc = sockets()[listener_idx].pending_child;
    if pc == NO_PENDING {
        return;
    }
    let ci = pc as usize;
    if ci >= MAX_USERSPACE_SOCKETS {
        // Corrupt bookkeeping — drop the reference rather than index
        // out of bounds (and panic) on the next accept.
        sockets_mut()[listener_idx].pending_child = NO_PENDING;
        return;
    }
    match sockets()[ci].state {
        SockState::Free => {
            sockets_mut()[listener_idx].pending_child = NO_PENDING;
        }
        SockState::Closed => {
            sockets_mut()[ci] = UserSocket::empty();
            sockets_mut()[listener_idx].pending_child = NO_PENDING;
        }
        _ => {}
    }
}

// ============================================================================
// Inbound segment handling (dispatch console vs userspace)
// ============================================================================

/// Entry point called by `net_stack::handle_ipv4` for IP protocol 6.
pub fn handle_segment(frame: &[u8], src_ip: [u8; 4], dst_ip: [u8; 4], segment: &[u8]) {
    let our_ip = net_stack::get_ip();
    if our_ip == [0, 0, 0, 0] || dst_ip != our_ip {
        return;
    }
    if segment.len() < 20 {
        return;
    }

    let src_port = u16::from_be_bytes([segment[0], segment[1]]);
    let dst_port = u16::from_be_bytes([segment[2], segment[3]]);
    let seq = u32::from_be_bytes([segment[4], segment[5], segment[6], segment[7]]);
    let ack = u32::from_be_bytes([segment[8], segment[9], segment[10], segment[11]]);
    let data_off = ((segment[12] >> 4) as usize) * 4;
    if data_off < 20 || data_off > segment.len() {
        return;
    }
    let flags = segment[13];
    let payload = &segment[data_off..];

    let src_mac: [u8; 6] = match frame[6..12].try_into() {
        Ok(v) => v,
        Err(_) => return,
    };

    if dst_port == LISTEN_PORT {
        handle_console_segment(src_mac, src_ip, src_port, seq, ack, flags, payload, our_ip);
        return;
    }

    handle_user_segment(src_mac, src_ip, src_port, dst_port, seq, ack, flags, payload, our_ip);
}

fn handle_console_segment(
    src_mac: [u8; 6],
    src_ip: [u8; 4],
    src_port: u16,
    seq: u32,
    ack: u32,
    flags: u8,
    payload: &[u8],
    our_ip: [u8; 4],
) {
    // SAFETY: single-threaded kernel.
    let c = unsafe { &mut *(&raw mut CONN) };

    if flags & RST != 0 {
        c.state = ConsoleState::Listen;
        return;
    }

    match c.state {
        ConsoleState::Listen => {
            if flags & SYN != 0 {
                let iss = next_iss();
                c.peer_mac = src_mac;
                c.peer_ip = src_ip;
                c.peer_port = src_port;
                c.rcv_nxt = seq.wrapping_add(1);
                c.snd_nxt = iss;
                send_console_segment(c, our_ip, SYN | ACK, &[]);
                c.snd_nxt = c.snd_nxt.wrapping_add(1);
                c.state = ConsoleState::SynReceived;
            } else {
                send_rst_for(src_mac, our_ip, src_ip, LISTEN_PORT, src_port, seq, ack, flags, payload.len());
            }
        }
        ConsoleState::SynReceived => {
            if flags & ACK != 0 && ack == c.snd_nxt && src_port == c.peer_port {
                c.state = ConsoleState::Established;
                let banner = b"BeetOS remote console (M7)\r\n";
                send_console_segment(c, our_ip, PSH | ACK, banner);
                flush_console_pending(c, our_ip);
            }
        }
        ConsoleState::Established => {
            if src_port != c.peer_port {
                return;
            }
            if !payload.is_empty() && seq == c.rcv_nxt {
                c.rcv_nxt = c.rcv_nxt.wrapping_add(payload.len() as u32);
                for &b in payload {
                    crate::arch::irq::dispatch_input_char_public(b);
                }
                flush_console_pending(c, our_ip);
            } else if !payload.is_empty() {
                send_console_segment(c, our_ip, ACK, &[]);
            }

            if flags & FIN != 0 {
                c.rcv_nxt = c.rcv_nxt.wrapping_add(1);
                send_console_segment(c, our_ip, ACK, &[]);
                send_console_segment(c, our_ip, FIN | ACK, &[]);
                c.snd_nxt = c.snd_nxt.wrapping_add(1);
                c.state = ConsoleState::LastAck;
            }
        }
        ConsoleState::LastAck => {
            if flags & ACK != 0 && ack == c.snd_nxt {
                c.state = ConsoleState::Listen;
            }
        }
    }
}

fn handle_user_segment(
    src_mac: [u8; 6],
    src_ip: [u8; 4],
    src_port: u16,
    dst_port: u16,
    seq: u32,
    ack_num: u32,
    flags: u8,
    payload: &[u8],
    our_ip: [u8; 4],
) {
    // First: is this segment for an existing connection?
    if let Some(idx) = find_matching(dst_port, src_ip, src_port) {
        let s = &mut sockets_mut()[idx];

        if flags & RST != 0 {
            // No RFC 5961 sequence validation — see module docs.
            if matches!(
                s.state,
                SockState::FinWait1 | SockState::FinWait2 | SockState::Closing
                    | SockState::LastAck | SockState::TimeWait
            ) {
                // App already released its handle; nobody is left to
                // observe the reset — reclaim the slot immediately.
                *s = UserSocket::empty();
            } else {
                // App still holds the handle: park the socket in
                // Closed so state() reports the reset; the slot frees
                // on the app's close() (or PID cleanup).
                s.state = SockState::Closed;
            }
            return;
        }

        // Where the peer's FIN would sit if this segment carries one:
        // right after its payload.
        let fin_pos = seq.wrapping_add(payload.len() as u32);

        match s.state {
            SockState::SynSent => {
                // We sent SYN, expect SYN+ACK.
                if flags & (SYN | ACK) == (SYN | ACK) && ack_num == s.snd_nxt {
                    s.rcv_nxt = seq.wrapping_add(1);
                    s.state = SockState::Established;
                    s.expiry_tick = 0;
                    // Always ACKs (completing the handshake); any bytes
                    // queued by send() before the connect finished ride
                    // along in the same segment.
                    flush_user_pending(s, our_ip);
                }
            }
            SockState::SynReceived => {
                // Listener child waiting for the handshake-completing ACK.
                if flags & ACK != 0 && ack_num == s.snd_nxt {
                    s.state = SockState::Established;
                    s.expiry_tick = 0;
                    if !payload.is_empty() && seq == s.rcv_nxt {
                        let accepted = s.rx_accept(payload);
                        s.rcv_nxt = s.rcv_nxt.wrapping_add(accepted as u32);
                        flush_user_pending(s, our_ip);
                    }
                    // A bare handshake ACK needs no reply.
                }
            }
            SockState::Established => {
                if !payload.is_empty() {
                    if seq == s.rcv_nxt {
                        let accepted = s.rx_accept(payload);
                        s.rcv_nxt = s.rcv_nxt.wrapping_add(accepted as u32);
                        flush_user_pending(s, our_ip);
                    } else {
                        // Out-of-order / retransmit — re-ACK our
                        // cumulative position.
                        send_user_segment(s, our_ip, ACK, &[]);
                    }
                }
                // Only honour the FIN if its sequence slot is exactly
                // where we are — a retransmitted FIN or one beyond a
                // partially-accepted payload waits for the peer's
                // retransmit (it must not double-advance rcv_nxt).
                if flags & FIN != 0 && fin_pos == s.rcv_nxt {
                    s.rcv_nxt = s.rcv_nxt.wrapping_add(1);
                    s.state = SockState::CloseWait;
                    send_user_segment(s, our_ip, ACK, &[]);
                }
            }
            SockState::CloseWait => {
                // Retransmitted FIN — our ACK was lost or delayed.
                // Re-ACK so the peer stops resending.
                if flags & FIN != 0 {
                    send_user_segment(s, our_ip, ACK, &[]);
                }
            }
            SockState::FinWait1 => {
                // Our FIN is out and the app handle is gone: inbound
                // data is discarded, but it still consumes sequence
                // space and must be ACKed or the peer retransmits it
                // forever.
                let had_payload = !payload.is_empty();
                if had_payload && seq == s.rcv_nxt {
                    s.rcv_nxt = s.rcv_nxt.wrapping_add(payload.len() as u32);
                }
                let our_fin_acked = flags & ACK != 0 && ack_num == s.snd_nxt;
                let peer_fin = flags & FIN != 0 && fin_pos == s.rcv_nxt;
                if peer_fin {
                    s.rcv_nxt = s.rcv_nxt.wrapping_add(1);
                }
                match (our_fin_acked, peer_fin) {
                    (true, true) => {
                        send_user_segment(s, our_ip, ACK, &[]);
                        s.state = SockState::TimeWait;
                        mark_expiry(s);
                    }
                    (true, false) => {
                        if had_payload {
                            send_user_segment(s, our_ip, ACK, &[]);
                        }
                        s.state = SockState::FinWait2;
                        mark_expiry(s);
                    }
                    (false, true) => {
                        send_user_segment(s, our_ip, ACK, &[]);
                        s.state = SockState::Closing;
                        mark_expiry(s);
                    }
                    (false, false) => {
                        if had_payload {
                            send_user_segment(s, our_ip, ACK, &[]);
                        }
                    }
                }
            }
            SockState::FinWait2 => {
                let had_payload = !payload.is_empty();
                if had_payload && seq == s.rcv_nxt {
                    s.rcv_nxt = s.rcv_nxt.wrapping_add(payload.len() as u32);
                }
                if flags & FIN != 0 && fin_pos == s.rcv_nxt {
                    s.rcv_nxt = s.rcv_nxt.wrapping_add(1);
                    send_user_segment(s, our_ip, ACK, &[]);
                    s.state = SockState::TimeWait;
                    mark_expiry(s);
                } else if had_payload {
                    send_user_segment(s, our_ip, ACK, &[]);
                }
            }
            SockState::Closing => {
                if flags & ACK != 0 && ack_num == s.snd_nxt {
                    s.state = SockState::TimeWait;
                    mark_expiry(s);
                }
            }
            SockState::LastAck => {
                if flags & ACK != 0 && ack_num == s.snd_nxt {
                    // close() already consumed the app handle — nobody
                    // can observe this socket again, free it now.
                    *s = UserSocket::empty();
                }
            }
            SockState::TimeWait => {
                // Retransmitted final FIN: our last ACK was lost.
                if flags & FIN != 0 {
                    send_user_segment(s, our_ip, ACK, &[]);
                }
            }
            // Closed swallows stragglers without a reply (the peer is
            // being wound down by RST/cleanup paths elsewhere); Free
            // and Listen are unreachable via find_matching.
            _ => {}
        }
        return;
    }

    // Not a known connection. Maybe a SYN to a listener?
    if flags & SYN != 0 && flags & ACK == 0 {
        if let Some(listener_idx) = find_listener(dst_port) {
            reclaim_stale_child(listener_idx);
            if sockets()[listener_idx].pending_child != NO_PENDING {
                // One pending handshake at a time (no backlog): refuse
                // so the host's connect() fails fast instead of hanging.
                send_rst_for(src_mac, our_ip, src_ip, dst_port, src_port, seq, ack_num, flags, payload.len());
                return;
            }
            let owner_pid = sockets()[listener_idx].owner_pid;
            let child = match alloc_socket(owner_pid) {
                Some(i) => i,
                None => {
                    send_rst_for(src_mac, our_ip, src_ip, dst_port, src_port, seq, ack_num, flags, payload.len());
                    return;
                }
            };
            let iss = next_iss();
            sockets_mut()[listener_idx].pending_child = child as u32;
            let c = &mut sockets_mut()[child];
            c.local_port = dst_port;
            c.peer_mac = src_mac;
            c.peer_ip = src_ip;
            c.peer_port = src_port;
            c.rcv_nxt = seq.wrapping_add(1);
            c.snd_nxt = iss;
            c.state = SockState::SynReceived;
            // Embryonic-connection guard: if the handshake ACK never
            // arrives, the reaper reclaims the slot.
            mark_expiry(c);
            send_user_segment(c, our_ip, SYN | ACK, &[]);
            c.snd_nxt = c.snd_nxt.wrapping_add(1);
            return;
        }
    }

    // Nothing claimed it — RST politely.
    send_rst_for(src_mac, our_ip, src_ip, dst_port, src_port, seq, ack_num, flags, payload.len());
}

// ============================================================================
// Outbound segment builders (shared)
// ============================================================================

fn send_console_segment(c: &mut ConsoleConn, our_ip: [u8; 4], flags: u8, payload: &[u8]) {
    let our_mac = match net::get_mac() {
        Some(m) => m,
        None => return,
    };
    build_and_send(
        c.peer_mac, our_mac, our_ip, c.peer_ip,
        LISTEN_PORT, c.peer_port,
        c.snd_nxt, c.rcv_nxt, RCV_WINDOW, flags, payload,
    );
    c.snd_nxt = c.snd_nxt.wrapping_add(payload.len() as u32);
}

/// Advertise the real free space of the socket's RX ring (capped at
/// [`RCV_WINDOW`]) so the peer can never legally send more than we
/// can buffer.
fn sock_window(s: &UserSocket) -> u16 {
    s.rx_free().min(RCV_WINDOW as usize) as u16
}

fn send_user_segment(s: &mut UserSocket, our_ip: [u8; 4], flags: u8, payload: &[u8]) {
    let our_mac = match net::get_mac() {
        Some(m) => m,
        None => return,
    };
    build_and_send(
        s.peer_mac, our_mac, our_ip, s.peer_ip,
        s.local_port, s.peer_port,
        s.snd_nxt, s.rcv_nxt, sock_window(s), flags, payload,
    );
    s.snd_nxt = s.snd_nxt.wrapping_add(payload.len() as u32);
}

fn send_rst_for(
    dst_mac: [u8; 6],
    our_ip: [u8; 4],
    peer_ip: [u8; 4],
    our_port: u16,
    peer_port: u16,
    their_seq: u32,
    their_ack: u32,
    their_flags: u8,
    payload_len: usize,
) {
    let our_mac = match net::get_mac() {
        Some(m) => m,
        None => return,
    };
    // RFC 793: if their segment had ACK, our RST seq is their ack;
    // otherwise seq=0 and we ACK their seq + (payload + 1 if FIN/SYN).
    let (seq, ack, flags) = if their_flags & ACK != 0 {
        (their_ack, 0, RST)
    } else {
        let mut adv = payload_len as u32;
        if their_flags & SYN != 0 { adv += 1; }
        if their_flags & FIN != 0 { adv += 1; }
        if adv == 0 { adv = 1; }
        (0, their_seq.wrapping_add(adv), RST | ACK)
    };
    build_and_send(
        dst_mac, our_mac, our_ip, peer_ip,
        our_port, peer_port,
        seq, ack, 0, flags, &[],
    );
}

/// Build one ETH+IPv4+TCP frame and hand it to the NIC. All fields
/// computed here; the caller decides what window/sequence numbers
/// were used (so per-socket bookkeeping stays at the call site).
#[allow(clippy::too_many_arguments)]
fn build_and_send(
    dst_mac: [u8; 6],
    src_mac: [u8; 6],
    our_ip: [u8; 4],
    peer_ip: [u8; 4],
    our_port: u16,
    peer_port: u16,
    seq: u32,
    ack: u32,
    window: u16,
    flags: u8,
    payload: &[u8],
) {
    let payload = &payload[..payload.len().min(MAX_SEG_PAYLOAD)];

    let tcp_len = 20 + payload.len();
    let ip_total = 20 + tcp_len;
    let total = 14 + ip_total;
    let mut pkt = [0u8; 14 + 20 + 20 + MAX_SEG_PAYLOAD];

    pkt[0..6].copy_from_slice(&dst_mac);
    pkt[6..12].copy_from_slice(&src_mac);
    pkt[12..14].copy_from_slice(&[0x08, 0x00]);

    pkt[14] = 0x45;
    pkt[15] = 0x00;
    pkt[16..18].copy_from_slice(&(ip_total as u16).to_be_bytes());
    pkt[18..20].copy_from_slice(&[0x00, 0x00]);
    pkt[20..22].copy_from_slice(&[0x40, 0x00]);
    pkt[22] = 64;
    pkt[23] = 6;
    pkt[24..26].copy_from_slice(&[0x00, 0x00]);
    pkt[26..30].copy_from_slice(&our_ip);
    pkt[30..34].copy_from_slice(&peer_ip);
    let ip_csum = net_stack::internet_checksum(&pkt[14..34]);
    pkt[24..26].copy_from_slice(&ip_csum.to_be_bytes());

    let t = 34;
    pkt[t..t + 2].copy_from_slice(&our_port.to_be_bytes());
    pkt[t + 2..t + 4].copy_from_slice(&peer_port.to_be_bytes());
    pkt[t + 4..t + 8].copy_from_slice(&seq.to_be_bytes());
    pkt[t + 8..t + 12].copy_from_slice(&ack.to_be_bytes());
    pkt[t + 12] = 0x50;
    pkt[t + 13] = flags;
    pkt[t + 14..t + 16].copy_from_slice(&window.to_be_bytes());
    pkt[t + 16..t + 18].copy_from_slice(&[0x00, 0x00]);
    pkt[t + 18..t + 20].copy_from_slice(&[0x00, 0x00]);
    pkt[t + 20..t + 20 + payload.len()].copy_from_slice(payload);

    let csum = tcp_checksum(our_ip, peer_ip, &pkt[t..t + tcp_len]);
    pkt[t + 16..t + 18].copy_from_slice(&csum.to_be_bytes());

    net::send_packet(&pkt[..total]);
}

fn tcp_checksum(src_ip: [u8; 4], dst_ip: [u8; 4], tcp: &[u8]) -> u16 {
    let mut buf = [0u8; 12 + 20 + MAX_SEG_PAYLOAD];
    buf[0..4].copy_from_slice(&src_ip);
    buf[4..8].copy_from_slice(&dst_ip);
    buf[8] = 0;
    buf[9] = 6;
    buf[10..12].copy_from_slice(&(tcp.len() as u16).to_be_bytes());
    buf[12..12 + tcp.len()].copy_from_slice(tcp);
    net_stack::internet_checksum(&buf[..12 + tcp.len()])
}

// ============================================================================
// Console RING push / flush (NetConsolePush syscall + timer)
// ============================================================================

pub fn console_push(bytes: &[u8]) {
    unsafe {
        let r = &mut *(&raw mut RING);
        for &b in bytes {
            let next = (r.head + 1) % RING_SIZE;
            if next == r.tail {
                r.tail = (r.tail + 1) % RING_SIZE;
            }
            r.buf[r.head] = b;
            r.head = next;
        }
    }
}

fn ring_drain(out: &mut [u8]) -> usize {
    unsafe {
        let r = &mut *(&raw mut RING);
        let mut n = 0;
        while n < out.len() && r.head != r.tail {
            out[n] = r.buf[r.tail];
            r.tail = (r.tail + 1) % RING_SIZE;
            n += 1;
        }
        n
    }
}

fn flush_console_pending(c: &mut ConsoleConn, our_ip: [u8; 4]) {
    let mut buf = [0u8; MAX_SEG_PAYLOAD];
    let n = ring_drain(&mut buf);
    if n > 0 {
        send_console_segment(c, our_ip, PSH | ACK, &buf[..n]);
    } else {
        send_console_segment(c, our_ip, ACK, &[]);
    }
}

/// Called from the 100 Hz timer to drain queued console bytes into a
/// segment even when the client is silent. No-op unless Established
/// and the ring has bytes.
pub fn tick_flush_console() {
    unsafe {
        let c = &mut *(&raw mut CONN);
        if c.state != ConsoleState::Established {
            return;
        }
        let r = &*(&raw const RING);
        if r.head == r.tail {
            return;
        }
        let our_ip = net_stack::get_ip();
        if our_ip == [0, 0, 0, 0] {
            return;
        }
        let mut buf = [0u8; MAX_SEG_PAYLOAD];
        let n = ring_drain(&mut buf);
        if n > 0 {
            send_console_segment(c, our_ip, PSH | ACK, &buf[..n]);
        }
    }
}

// ============================================================================
// Userspace flush, reaper + ARP-deferred SYN
// ============================================================================

/// Drain pending TX bytes from a userspace socket into one segment.
/// Sends a bare ACK if no payload is queued — used after an inbound
/// segment to acknowledge while folding in any queued reply.
fn flush_user_pending(s: &mut UserSocket, our_ip: [u8; 4]) {
    let mut buf = [0u8; MAX_SEG_PAYLOAD];
    let n = s.tx_drain(&mut buf);
    if n > 0 {
        send_user_segment(s, our_ip, PSH | ACK, &buf[..n]);
    } else {
        send_user_segment(s, our_ip, ACK, &[]);
    }
}

/// Timer-tick driver for userspace sockets (100 Hz):
///
/// 1. **Reap** slots whose wind-down (or embryonic handshake) ran out
///    of time — without retransmit, a lost final ACK must not pin a
///    slot forever.
/// 2. **Flush** queued TX on Established/CloseWait sockets when no
///    inbound segment arrived to piggy-back the ACK.
/// 3. **Retry ARP** for SynSent sockets still waiting on the next
///    hop's MAC (rate-limited to every [`ARP_RETRY_TICKS`]).
pub fn tick_flush_user(now: u64) {
    unsafe {
        NOW_TICK = now;
    }
    let our_ip = net_stack::get_ip();
    for idx in 0..MAX_USERSPACE_SOCKETS {
        let s = &mut sockets_mut()[idx];

        if s.expiry_tick != 0
            && now >= s.expiry_tick
            && matches!(
                s.state,
                SockState::FinWait1 | SockState::FinWait2 | SockState::Closing
                    | SockState::LastAck | SockState::TimeWait
                    | SockState::SynReceived | SockState::Closed
            )
        {
            *s = UserSocket::empty();
            continue;
        }

        if our_ip == [0, 0, 0, 0] {
            continue;
        }

        match s.state {
            SockState::Established | SockState::CloseWait if s.tx_has_data() => {
                let mut buf = [0u8; MAX_SEG_PAYLOAD];
                let n = s.tx_drain(&mut buf);
                if n > 0 {
                    send_user_segment(s, our_ip, PSH | ACK, &buf[..n]);
                }
            }
            SockState::SynSent if s.awaiting_arp && now % ARP_RETRY_TICKS == 0 => {
                try_resume_connect(s, our_ip);
            }
            _ => {}
        }
    }
}

/// If a socket is stuck in SynSent waiting for ARP, check whether the
/// MAC is now cached and (if so) actually send the SYN.
fn try_resume_connect(s: &mut UserSocket, our_ip: [u8; 4]) {
    let next_hop = next_hop_for(s.peer_ip);
    if let Some(mac) = net_stack::arp_lookup(next_hop) {
        s.peer_mac = mac;
        s.awaiting_arp = false;
        s.snd_nxt = next_iss();
        send_user_segment(s, our_ip, SYN, &[]);
        s.snd_nxt = s.snd_nxt.wrapping_add(1);
    } else {
        // No cache entry yet — (re-)issue the ARP request. The caller
        // rate-limits us to every ARP_RETRY_TICKS, so an unreachable
        // next hop costs 4 broadcasts/s, not 100.
        net_stack::arp_request(next_hop);
    }
}

/// Pick the next-hop IP for a destination: same-subnet → peer directly,
/// otherwise → gateway. Subnet mask is hardcoded /24 for QEMU's
/// 10.0.2.0/24; real-NIC will need to read the mask from DHCP.
fn next_hop_for(peer_ip: [u8; 4]) -> [u8; 4] {
    let our = net_stack::get_ip();
    if our[..3] == peer_ip[..3] {
        peer_ip
    } else {
        net_stack::get_gateway()
    }
}

// ============================================================================
// Public API called from syscall handlers
// ============================================================================
//
// Every function takes the calling process's PID and refuses to act on
// a socket it doesn't own. The kernel console path has no PID — it
// never touches this table.

/// Allocate a fresh userspace socket. Returns its index in the table.
pub fn user_create(owner_pid: u32) -> Option<usize> {
    alloc_socket(owner_pid)
}

/// Put a Closed socket into Listen on `port`. Returns `Err` if the
/// socket isn't the caller's, isn't Closed, or the port is reserved
/// (2323 = kernel console) or already claimed by any live socket.
pub fn user_listen(idx: usize, port: u16, caller_pid: u32) -> Result<(), &'static str> {
    if idx >= MAX_USERSPACE_SOCKETS {
        return Err("bad sock");
    }
    if port == 0 || port == LISTEN_PORT {
        return Err("reserved port");
    }
    if port_in_use(port) {
        return Err("port in use");
    }
    let s = &mut sockets_mut()[idx];
    if s.owner_pid != caller_pid {
        return Err("not owner");
    }
    if !matches!(s.state, SockState::Closed) {
        return Err("not Closed");
    }
    s.local_port = port;
    s.state = SockState::Listen;
    s.pending_child = NO_PENDING;
    Ok(())
}

/// Try to accept a pending connection on a listener. Returns
/// `Some(child_idx)` once a child has reached `Established`,
/// `None` otherwise (mapped to `NO_PENDING` by the syscall layer).
pub fn user_accept(idx: usize, caller_pid: u32) -> Option<usize> {
    if idx >= MAX_USERSPACE_SOCKETS {
        return None;
    }
    {
        let l = &sockets()[idx];
        if !matches!(l.state, SockState::Listen) || l.owner_pid != caller_pid {
            return None;
        }
    }
    // A child that died mid-handshake must not wedge the listener.
    reclaim_stale_child(idx);

    let pc = sockets()[idx].pending_child;
    if pc == NO_PENDING {
        return None;
    }
    let ci = pc as usize;
    if ci >= MAX_USERSPACE_SOCKETS
        || !matches!(sockets()[ci].state, SockState::Established)
    {
        return None; // still handshaking (or bookkeeping just reset)
    }
    sockets_mut()[idx].pending_child = NO_PENDING;
    Some(ci)
}

/// Initiate an active open. Returns `Ok(())` on success (SYN sent or
/// ARP request issued). Caller then polls the socket state.
pub fn user_connect(
    idx: usize,
    peer_ip: [u8; 4],
    peer_port: u16,
    caller_pid: u32,
) -> Result<(), &'static str> {
    if idx >= MAX_USERSPACE_SOCKETS {
        return Err("bad sock");
    }
    if peer_port == 0 {
        return Err("bad port");
    }
    {
        let s = &sockets()[idx];
        if s.owner_pid != caller_pid {
            return Err("not owner");
        }
        if !matches!(s.state, SockState::Closed) {
            return Err("not Closed");
        }
    }
    let local_port = alloc_ephemeral_port();
    {
        let s = &mut sockets_mut()[idx];
        s.local_port = local_port;
        s.peer_ip = peer_ip;
        s.peer_port = peer_port;
        s.state = SockState::SynSent;
        s.awaiting_arp = true;
    }
    let our_ip = net_stack::get_ip();
    if our_ip == [0, 0, 0, 0] {
        // No IP yet — the timer tick retries once DHCP binds.
        return Ok(());
    }
    try_resume_connect(&mut sockets_mut()[idx], our_ip);
    Ok(())
}

/// Queue bytes for transmission. Returns the number actually buffered
/// (may be less than `bytes.len()` if the TX ring is full).
pub fn user_send(idx: usize, bytes: &[u8], caller_pid: u32) -> Option<usize> {
    if idx >= MAX_USERSPACE_SOCKETS {
        return None;
    }
    let our_ip = net_stack::get_ip();
    let s = &mut sockets_mut()[idx];
    if s.owner_pid != caller_pid {
        return None;
    }
    // CloseWait: the peer half-closed but is still reading — sending
    // stays legal until our own close().
    if !matches!(s.state, SockState::Established | SockState::CloseWait) {
        return None;
    }
    let n = s.tx_push(bytes);
    if our_ip != [0, 0, 0, 0] && s.tx_has_data() {
        // Drain immediately so the segment goes out without waiting
        // for the next timer tick.
        let mut buf = [0u8; MAX_SEG_PAYLOAD];
        let drained = s.tx_drain(&mut buf);
        if drained > 0 {
            send_user_segment(s, our_ip, PSH | ACK, &buf[..drained]);
        }
    }
    Some(n)
}

/// Drain received bytes into `out`. Returns the number copied.
pub fn user_recv(idx: usize, out: &mut [u8], caller_pid: u32) -> Option<usize> {
    if idx >= MAX_USERSPACE_SOCKETS {
        return None;
    }
    let our_ip = net_stack::get_ip();
    let s = &mut sockets_mut()[idx];
    if matches!(s.state, SockState::Free) || s.owner_pid != caller_pid {
        return None;
    }
    // If the ring was (nearly) full we have been advertising a closed
    // window; once the app drains it, tell the peer it reopened or it
    // will sit on slow zero-window probes.
    let was_starved = s.rx_free() < 64;
    let n = s.rx_drain(out);
    if n > 0
        && was_starved
        && matches!(s.state, SockState::Established)
        && our_ip != [0, 0, 0, 0]
    {
        send_user_segment(s, our_ip, ACK, &[]);
    }
    Some(n)
}

/// Initiate close on the caller's socket. See [`close_internal`] for
/// the per-state behaviour.
pub fn user_close(idx: usize, caller_pid: u32) -> Result<(), &'static str> {
    if idx >= MAX_USERSPACE_SOCKETS {
        return Err("bad sock");
    }
    {
        let s = &sockets()[idx];
        if matches!(s.state, SockState::Free) {
            return Err("not allocated");
        }
        if s.owner_pid != caller_pid {
            return Err("not owner");
        }
    }
    close_internal(idx);
    Ok(())
}

/// Owner-agnostic close, shared by [`user_close`], [`cleanup_for_pid`]
/// and listener teardown. Established/CloseWait get a graceful FIN
/// (with queued TX flushed first); half-open and inert sockets are
/// dropped on the spot; sockets already winding down are left to the
/// reaper.
fn close_internal(idx: usize) {
    let our_ip = net_stack::get_ip();
    let state = sockets()[idx].state;
    match state {
        SockState::Free => {}
        SockState::Listen => {
            // A child that was never accepted can't ever be accepted
            // once the listener is gone — tear it down too.
            let pc = sockets()[idx].pending_child;
            if pc != NO_PENDING && (pc as usize) < MAX_USERSPACE_SOCKETS {
                let ci = pc as usize;
                match sockets()[ci].state {
                    SockState::Established | SockState::CloseWait => close_internal(ci),
                    SockState::SynReceived | SockState::Closed => {
                        sockets_mut()[ci] = UserSocket::empty();
                    }
                    // Free, or already winding down (reaper handles it).
                    _ => {}
                }
            }
            sockets_mut()[idx] = UserSocket::empty();
        }
        SockState::Closed | SockState::SynSent | SockState::SynReceived => {
            // Nothing (or only half a handshake) committed on the wire:
            // drop the slot. A peer that completed its side will get a
            // RST from the no-match path when it next sends.
            sockets_mut()[idx] = UserSocket::empty();
        }
        SockState::Established | SockState::CloseWait => {
            if our_ip == [0, 0, 0, 0] {
                sockets_mut()[idx] = UserSocket::empty();
                return;
            }
            let s = &mut sockets_mut()[idx];
            // Flush queued TX as a final data segment, then FIN.
            let mut buf = [0u8; MAX_SEG_PAYLOAD];
            let n = s.tx_drain(&mut buf);
            if n > 0 {
                send_user_segment(s, our_ip, PSH | ACK, &buf[..n]);
            }
            send_user_segment(s, our_ip, FIN | ACK, &[]);
            s.snd_nxt = s.snd_nxt.wrapping_add(1);
            s.state = if state == SockState::Established {
                SockState::FinWait1
            } else {
                SockState::LastAck
            };
            mark_expiry(s);
        }
        // FinWait1/2, Closing, LastAck, TimeWait: a close is already in
        // flight; the reaper frees the slot when its deadline passes.
        _ => {}
    }
}

/// Reclaim every socket owned by a terminating process. Called from
/// the TerminateProcess/TerminatePid syscall handlers so a crashing
/// (or just sloppy) app can't exhaust the socket table.
pub fn cleanup_for_pid(pid: u32) {
    for idx in 0..MAX_USERSPACE_SOCKETS {
        let owned = {
            let s = &sockets()[idx];
            !matches!(s.state, SockState::Free) && s.owner_pid == pid
        };
        if owned {
            close_internal(idx);
        }
    }
}

/// Read-only state query. Returns `None` for an out-of-range index or
/// a socket owned by someone else (the caller can't distinguish that
/// from a recycled slot — by design).
pub fn user_state(idx: usize, caller_pid: u32) -> Option<SockState> {
    if idx >= MAX_USERSPACE_SOCKETS {
        return None;
    }
    let s = &sockets()[idx];
    if !matches!(s.state, SockState::Free) && s.owner_pid != caller_pid {
        return None;
    }
    Some(s.state)
}

/// Read-only RX queue length query (for select-style polling).
pub fn user_rx_len(idx: usize, caller_pid: u32) -> usize {
    if idx >= MAX_USERSPACE_SOCKETS {
        return 0;
    }
    let s = &sockets()[idx];
    if s.owner_pid != caller_pid {
        return 0;
    }
    s.rx_len()
}
