// SPDX-FileCopyrightText: 2025 BeetOS contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! TCP for BeetOS (QEMU virt).
//!
//! Two coexisting datapaths share the same low-level segment builder:
//!
//!   1. **Kernel-managed remote console** on the well-known
//!      [`LISTEN_PORT`] (2323). Passive-open only. The single
//!      [`CONN`] static + 4 KiB [`RING`] back the M7 remote shell —
//!      receives bytes go straight into the shell input dispatcher,
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
//! ## Deliberate limitations (v1)
//!
//! - **No retransmission / no RTO.** QEMU user-mode networking is a
//!   loopback-quality link; segments don't get lost between host and
//!   guest. A real NIC port will need a real retransmit timer.
//! - **No out-of-order reassembly, no window scaling.** Fixed receive
//!   window, in-order delivery assumed.
//! - **No SACK, no Nagle, no delayed ACK.**
//! - **Single pending accept per listener.** A second SYN while one
//!   is still in handshake gets RST.
//! - **`MAX_USERSPACE_SOCKETS` (= 4) sockets** in the static table.

use super::{net, net_stack};

/// Port the kernel remote console listens on. 2323 is unprivileged
/// and avoids clashing with a host telnet on 23.
pub const LISTEN_PORT: u16 = 2323;

/// Receive window advertised by the kernel console. One frame's worth
/// is plenty for a line-oriented console.
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
pub const MAX_USERSPACE_SOCKETS: usize = 4;

const SOCK_RX_BUF: usize = 1024;
const SOCK_TX_BUF: usize = 1024;

/// Sentinel returned from `accept` when no connection is ready.
pub const NO_PENDING: u32 = u32::MAX;

/// Ephemeral local port allocator. Starts in the IANA dynamic range.
static mut NEXT_EPHEMERAL_PORT: u16 = 49152;

fn alloc_ephemeral_port() -> u16 {
    unsafe {
        let p = NEXT_EPHEMERAL_PORT;
        NEXT_EPHEMERAL_PORT = NEXT_EPHEMERAL_PORT.wrapping_add(1);
        if NEXT_EPHEMERAL_PORT < 49152 {
            NEXT_EPHEMERAL_PORT = 49152;
        }
        p
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

pub struct UserSocket {
    pub state: SockState,
    /// Process that created this socket. Sockets are closed if the
    /// process exits (not enforced yet, but reserved for cleanup).
    pub owner_pid: u32,
    pub local_port: u16,
    pub peer_mac: [u8; 6],
    pub peer_ip: [u8; 4],
    pub peer_port: u16,
    pub snd_nxt: u32,
    pub rcv_nxt: u32,
    /// Listener bookkeeping: slot index of an in-progress or accepted
    /// child connection. `NO_PENDING` if no pending child. Only one
    /// pending child per listener for V1 (no backlog).
    pub pending_child: u32,
    /// Set when `connect` has been called but we don't yet have the
    /// peer's MAC (ARP request outstanding). The state stays at
    /// SynSent until ARP resolves and we send the SYN.
    pub awaiting_arp: bool,
    pub rx_buf: [u8; SOCK_RX_BUF],
    pub rx_head: usize,
    pub rx_tail: usize,
    pub tx_buf: [u8; SOCK_TX_BUF],
    pub tx_head: usize,
    pub tx_tail: usize,
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
            rx_buf: [0; SOCK_RX_BUF],
            rx_head: 0,
            rx_tail: 0,
            tx_buf: [0; SOCK_TX_BUF],
            tx_head: 0,
            tx_tail: 0,
        }
    }

    fn rx_push(&mut self, b: u8) {
        let next = (self.rx_head + 1) % SOCK_RX_BUF;
        if next == self.rx_tail {
            // Buffer full; drop oldest so newest is preserved (best-
            // effort — apps that need backpressure must drain faster).
            self.rx_tail = (self.rx_tail + 1) % SOCK_RX_BUF;
        }
        self.rx_buf[self.rx_head] = b;
        self.rx_head = next;
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
/// nothing inside the kernel itself).
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
        let socks = sockets_mut();
        let s = &mut socks[idx];

        if flags & RST != 0 {
            s.state = SockState::Closed;
            return;
        }

        match s.state {
            SockState::SynSent => {
                // We sent SYN, expect SYN+ACK.
                if flags & (SYN | ACK) == (SYN | ACK) && ack_num == s.snd_nxt {
                    s.rcv_nxt = seq.wrapping_add(1);
                    // ACK the SYN.
                    send_user_segment(idx, our_ip, ACK, &[]);
                    s.state = SockState::Established;
                    flush_user_pending(idx, our_ip);
                }
            }
            SockState::SynReceived => {
                // Listener child waiting for handshake-completing ACK.
                if flags & ACK != 0 && ack_num == s.snd_nxt {
                    s.state = SockState::Established;
                    if !payload.is_empty() && seq == s.rcv_nxt {
                        s.rcv_nxt = s.rcv_nxt.wrapping_add(payload.len() as u32);
                        for &b in payload {
                            s.rx_push(b);
                        }
                    }
                    flush_user_pending(idx, our_ip);
                }
            }
            SockState::Established => {
                if !payload.is_empty() && seq == s.rcv_nxt {
                    s.rcv_nxt = s.rcv_nxt.wrapping_add(payload.len() as u32);
                    for &b in payload {
                        s.rx_push(b);
                    }
                    flush_user_pending(idx, our_ip);
                } else if !payload.is_empty() {
                    send_user_segment(idx, our_ip, ACK, &[]);
                }

                if flags & FIN != 0 {
                    s.rcv_nxt = s.rcv_nxt.wrapping_add(1);
                    send_user_segment(idx, our_ip, ACK, &[]);
                    // Move to CloseWait — app must call close() to send our own FIN.
                    s.state = SockState::CloseWait;
                }
            }
            SockState::FinWait1 => {
                // We sent FIN. Could see ACK of our FIN, FIN of peer, or both.
                let our_fin_acked = flags & ACK != 0 && ack_num == s.snd_nxt;
                let peer_fin = flags & FIN != 0;
                if !payload.is_empty() && seq == s.rcv_nxt {
                    s.rcv_nxt = s.rcv_nxt.wrapping_add(payload.len() as u32);
                    for &b in payload {
                        s.rx_push(b);
                    }
                }
                if peer_fin {
                    s.rcv_nxt = s.rcv_nxt.wrapping_add(1);
                }
                match (our_fin_acked, peer_fin) {
                    (true, true) => {
                        send_user_segment(idx, our_ip, ACK, &[]);
                        s.state = SockState::TimeWait;
                    }
                    (true, false) => {
                        s.state = SockState::FinWait2;
                    }
                    (false, true) => {
                        send_user_segment(idx, our_ip, ACK, &[]);
                        s.state = SockState::Closing;
                    }
                    _ => {}
                }
            }
            SockState::FinWait2 => {
                if flags & FIN != 0 {
                    s.rcv_nxt = s.rcv_nxt.wrapping_add(1);
                    send_user_segment(idx, our_ip, ACK, &[]);
                    s.state = SockState::TimeWait;
                }
            }
            SockState::Closing => {
                if flags & ACK != 0 && ack_num == s.snd_nxt {
                    s.state = SockState::TimeWait;
                }
            }
            SockState::LastAck => {
                if flags & ACK != 0 && ack_num == s.snd_nxt {
                    s.state = SockState::Closed;
                }
            }
            _ => {}
        }
        return;
    }

    // Not a known connection. Maybe a SYN to a listener?
    if flags & SYN != 0 && flags & ACK == 0 {
        if let Some(listener_idx) = find_listener(dst_port) {
            // Allocate a child socket. The listener must not already
            // have a pending child (V1: no backlog).
            let socks = sockets_mut();
            if socks[listener_idx].pending_child != NO_PENDING {
                // Already mid-handshake on another peer — refuse.
                send_rst_for(src_mac, our_ip, src_ip, dst_port, src_port, seq, ack_num, flags, payload.len());
                return;
            }
            let owner_pid = socks[listener_idx].owner_pid;
            let child = match alloc_socket(owner_pid) {
                Some(i) => i,
                None => {
                    send_rst_for(src_mac, our_ip, src_ip, dst_port, src_port, seq, ack_num, flags, payload.len());
                    return;
                }
            };
            let iss = next_iss();
            let socks = sockets_mut();
            socks[listener_idx].pending_child = child as u32;
            let c = &mut socks[child];
            c.local_port = dst_port;
            c.peer_mac = src_mac;
            c.peer_ip = src_ip;
            c.peer_port = src_port;
            c.rcv_nxt = seq.wrapping_add(1);
            c.snd_nxt = iss;
            c.state = SockState::SynReceived;
            send_user_segment(child, our_ip, SYN | ACK, &[]);
            socks[child].snd_nxt = socks[child].snd_nxt.wrapping_add(1);
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
        c.snd_nxt, c.rcv_nxt, flags, payload,
    );
    c.snd_nxt = c.snd_nxt.wrapping_add(payload.len() as u32);
}

fn send_user_segment(idx: usize, our_ip: [u8; 4], flags: u8, payload: &[u8]) {
    let our_mac = match net::get_mac() {
        Some(m) => m,
        None => return,
    };
    let socks = sockets_mut();
    let s = &mut socks[idx];
    build_and_send(
        s.peer_mac, our_mac, our_ip, s.peer_ip,
        s.local_port, s.peer_port,
        s.snd_nxt, s.rcv_nxt, flags, payload,
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
        seq, ack, flags, &[],
    );
}

/// Build one ETH+IPv4+TCP frame and hand it to the NIC. All fields
/// computed here; the caller decides what window/sequence numbers
/// were used (so per-socket bookkeeping stays at the call site).
fn build_and_send(
    dst_mac: [u8; 6],
    src_mac: [u8; 6],
    our_ip: [u8; 4],
    peer_ip: [u8; 4],
    our_port: u16,
    peer_port: u16,
    seq: u32,
    ack: u32,
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
    pkt[t + 14..t + 16].copy_from_slice(&RCV_WINDOW.to_be_bytes());
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
// Userspace flush + ARP-deferred SYN
// ============================================================================

/// Drain pending TX bytes from a userspace socket into one segment.
/// Sends a bare ACK if no payload is queued — used after an inbound
/// segment to acknowledge while folding in any queued reply.
fn flush_user_pending(idx: usize, our_ip: [u8; 4]) {
    let mut buf = [0u8; MAX_SEG_PAYLOAD];
    let n;
    {
        let s = &mut sockets_mut()[idx];
        n = s.tx_drain(&mut buf);
    }
    if n > 0 {
        send_user_segment(idx, our_ip, PSH | ACK, &buf[..n]);
    } else {
        send_user_segment(idx, our_ip, ACK, &[]);
    }
}

/// Called every timer tick to drain queued TX on Established userspace
/// sockets when no inbound segment has arrived to piggy-back the ACK.
pub fn tick_flush_user() {
    let our_ip = net_stack::get_ip();
    if our_ip == [0, 0, 0, 0] {
        return;
    }
    for idx in 0..MAX_USERSPACE_SOCKETS {
        let needs_flush;
        let needs_arp_retry;
        {
            let s = &sockets()[idx];
            needs_flush = matches!(s.state, SockState::Established) && s.tx_has_data();
            needs_arp_retry = matches!(s.state, SockState::SynSent) && s.awaiting_arp;
        }
        if needs_flush {
            let mut buf = [0u8; MAX_SEG_PAYLOAD];
            let n = sockets_mut()[idx].tx_drain(&mut buf);
            if n > 0 {
                send_user_segment(idx, our_ip, PSH | ACK, &buf[..n]);
            }
        }
        if needs_arp_retry {
            try_resume_connect(idx, our_ip);
        }
    }
}

/// If a socket is stuck in SynSent waiting for ARP, check whether the
/// MAC is now cached and (if so) actually send the SYN.
fn try_resume_connect(idx: usize, our_ip: [u8; 4]) {
    let next_hop = {
        let s = &sockets()[idx];
        next_hop_for(s.peer_ip)
    };
    if let Some(mac) = net_stack::arp_lookup(next_hop) {
        let socks = sockets_mut();
        socks[idx].peer_mac = mac;
        socks[idx].awaiting_arp = false;
        let iss = next_iss();
        socks[idx].snd_nxt = iss;
        send_user_segment(idx, our_ip, SYN, &[]);
        socks[idx].snd_nxt = socks[idx].snd_nxt.wrapping_add(1);
    } else {
        // Re-issue the ARP request periodically. The net_stack does
        // its own dedup so spamming is cheap.
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

/// Allocate a fresh userspace socket. Returns its index in the table.
pub fn user_create(owner_pid: u32) -> Option<usize> {
    alloc_socket(owner_pid)
}

/// Put a Closed socket into Listen on `port`. Returns `Err` if the
/// socket isn't allocated, isn't Closed, or the port is reserved
/// (port 2323 is the kernel console) or already in use.
pub fn user_listen(idx: usize, port: u16) -> Result<(), &'static str> {
    if idx >= MAX_USERSPACE_SOCKETS {
        return Err("bad sock");
    }
    if port == 0 || port == LISTEN_PORT {
        return Err("reserved port");
    }
    if find_listener(port).is_some() {
        return Err("port in use");
    }
    let s = &mut sockets_mut()[idx];
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
/// `None` (= NO_PENDING) otherwise.
pub fn user_accept(idx: usize) -> Option<usize> {
    if idx >= MAX_USERSPACE_SOCKETS {
        return None;
    }
    let socks = sockets_mut();
    let listener = &socks[idx];
    if !matches!(listener.state, SockState::Listen) {
        return None;
    }
    let child = listener.pending_child;
    if child == NO_PENDING {
        return None;
    }
    if !matches!(socks[child as usize].state, SockState::Established) {
        return None;
    }
    socks[idx].pending_child = NO_PENDING;
    Some(child as usize)
}

/// Initiate an active open. Returns `Ok(())` on success (SYN sent or
/// ARP request issued). Caller then polls the socket state.
pub fn user_connect(idx: usize, peer_ip: [u8; 4], peer_port: u16) -> Result<(), &'static str> {
    if idx >= MAX_USERSPACE_SOCKETS {
        return Err("bad sock");
    }
    {
        let s = &sockets()[idx];
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
        // No IP yet — let the timer tick retry once DHCP binds.
        return Ok(());
    }
    try_resume_connect(idx, our_ip);
    Ok(())
}

/// Queue bytes for transmission. Returns the number actually buffered
/// (may be less than `bytes.len()` if the TX ring is full).
pub fn user_send(idx: usize, bytes: &[u8]) -> Option<usize> {
    if idx >= MAX_USERSPACE_SOCKETS {
        return None;
    }
    let our_ip = net_stack::get_ip();
    let needs_flush;
    let n;
    {
        let s = &mut sockets_mut()[idx];
        if !matches!(s.state, SockState::Established | SockState::CloseWait) {
            return None;
        }
        n = s.tx_push(bytes);
        needs_flush = matches!(s.state, SockState::Established) && s.tx_has_data();
    }
    if needs_flush && our_ip != [0, 0, 0, 0] {
        // Drain immediately so the segment goes out without waiting
        // for the next timer tick.
        let mut buf = [0u8; MAX_SEG_PAYLOAD];
        let drained = sockets_mut()[idx].tx_drain(&mut buf);
        if drained > 0 {
            send_user_segment(idx, our_ip, PSH | ACK, &buf[..drained]);
        }
    }
    Some(n)
}

/// Drain received bytes into `out`. Returns the number copied.
pub fn user_recv(idx: usize, out: &mut [u8]) -> Option<usize> {
    if idx >= MAX_USERSPACE_SOCKETS {
        return None;
    }
    let s = &mut sockets_mut()[idx];
    if matches!(s.state, SockState::Free) {
        return None;
    }
    Some(s.rx_drain(out))
}

/// Initiate close. Sends FIN, transitions to FinWait1 (or LastAck if
/// we were in CloseWait). A listener simply goes back to Closed.
pub fn user_close(idx: usize) -> Result<(), &'static str> {
    if idx >= MAX_USERSPACE_SOCKETS {
        return Err("bad sock");
    }
    let our_ip = net_stack::get_ip();
    let socks = sockets_mut();
    let s = &mut socks[idx];
    match s.state {
        SockState::Free => return Err("not allocated"),
        SockState::Listen | SockState::Closed => {
            s.state = SockState::Free;
            return Ok(());
        }
        SockState::Established => {
            // Flush any pending bytes then send FIN.
            // Drop borrow so flush_user_pending can re-take it.
        }
        SockState::CloseWait => {}
        SockState::SynSent | SockState::SynReceived => {
            // Half-open: just mark free, no segment.
            s.state = SockState::Free;
            return Ok(());
        }
        _ => return Ok(()),
    }
    let old_state = s.state;
    if our_ip != [0, 0, 0, 0] {
        // Drain queued TX into a final data segment.
        let mut buf = [0u8; MAX_SEG_PAYLOAD];
        let n = sockets_mut()[idx].tx_drain(&mut buf);
        if n > 0 {
            send_user_segment(idx, our_ip, PSH | ACK, &buf[..n]);
        }
        send_user_segment(idx, our_ip, FIN | ACK, &[]);
        sockets_mut()[idx].snd_nxt = sockets_mut()[idx].snd_nxt.wrapping_add(1);
    }
    let s = &mut sockets_mut()[idx];
    s.state = match old_state {
        SockState::Established => SockState::FinWait1,
        SockState::CloseWait => SockState::LastAck,
        _ => SockState::Closed,
    };
    Ok(())
}

/// Read-only state query for syscall handlers.
pub fn user_state(idx: usize) -> Option<SockState> {
    if idx >= MAX_USERSPACE_SOCKETS {
        return None;
    }
    Some(sockets()[idx].state)
}

/// Read-only RX queue length query (for select-style polling).
pub fn user_rx_len(idx: usize) -> usize {
    if idx >= MAX_USERSPACE_SOCKETS {
        return 0;
    }
    let s = &sockets()[idx];
    if s.rx_head >= s.rx_tail {
        s.rx_head - s.rx_tail
    } else {
        SOCK_RX_BUF - s.rx_tail + s.rx_head
    }
}
