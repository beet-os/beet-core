// SPDX-FileCopyrightText: 2025 BeetOS contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Minimal passive-open TCP server for BeetOS (QEMU virt).
//!
//! This is the transport that turns the existing L2/L3 stack
//! (`net_stack.rs`: ARP + DHCP + ICMP) into something a remote client
//! can actually talk to. It implements just enough of RFC 793 to:
//!
//!   - LISTEN on one well-known port ([`LISTEN_PORT`]).
//!   - Complete the three-way handshake (SYN → SYN/ACK → ACK).
//!   - Receive a data segment, hand its payload to the [`console`]
//!     line interpreter, and send the reply back.
//!   - Tear the connection down cleanly (FIN → ACK / FIN → ACK).
//!
//! ## Deliberate limitations (single-connection, no_alloc, v1)
//!
//! - **One connection at a time.** A second SYN while a connection is
//!   live is answered with RST. State lives in a single `static`.
//! - **No retransmission / no RTO.** QEMU user-mode networking is a
//!   loopback-quality link; segments don't get lost between the host
//!   and the guest. A real NIC port (M7 follow-up) will need a real
//!   retransmit timer.
//! - **No out-of-order reassembly, no window scaling.** We advertise a
//!   fixed receive window and assume in-order delivery.
//! - **No Nagle, no delayed ACK.** Every received data segment is
//!   answered immediately with the reply payload piggy-backed on the
//!   ACK.
//!
//! These are honest constraints for a first cut; they're enough to
//! prove the datapath end-to-end and serve the remote console. The
//! follow-up that bridges this to the *real* userspace shell (capturing
//! its stdout over the socket) is tracked in plan.md under M7.

use super::{net, net_stack};

/// Port the remote console listens on. 2323 is unprivileged and avoids
/// clashing with a host telnet on 23.
pub const LISTEN_PORT: u16 = 2323;

/// Receive window we advertise. One frame's worth is plenty for a
/// line-oriented console and keeps the peer from bursting more than we
/// copy per segment.
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

#[derive(Clone, Copy, PartialEq)]
enum State {
    Listen,
    SynReceived,
    Established,
    /// We've received the peer's FIN and sent our own; waiting for the
    /// final ACK before returning to LISTEN.
    LastAck,
}

struct Conn {
    state: State,
    peer_mac: [u8; 6],
    peer_ip: [u8; 4],
    peer_port: u16,
    /// Next sequence number we will send.
    snd_nxt: u32,
    /// Next sequence number we expect to receive (ACK we send back).
    rcv_nxt: u32,
}

static mut CONN: Conn = Conn {
    state: State::Listen,
    peer_mac: [0; 6],
    peer_ip: [0; 4],
    peer_port: 0,
    snd_nxt: 0,
    rcv_nxt: 0,
};

/// A small, deterministic Initial Send Sequence. A real stack would
/// randomise this (RFC 6528); for a loopback-quality QEMU link a fixed
/// base advanced per connection avoids pulling in the RNG here.
static mut ISS_COUNTER: u32 = 0x1000;

// ============================================================================
// Inbound segment handling
// ============================================================================

/// Entry point called by `net_stack::handle_ipv4` for IP protocol 6.
///
/// `frame` is the full Ethernet frame (we need the source MAC to reply);
/// `segment` is the TCP header + payload.
pub fn handle_segment(frame: &[u8], src_ip: [u8; 4], dst_ip: [u8; 4], segment: &[u8]) {
    // Only serve traffic addressed to us.
    let our_ip = net_stack::get_ip();
    if our_ip == [0, 0, 0, 0] || dst_ip != our_ip {
        return;
    }
    if segment.len() < 20 {
        return;
    }

    let src_port = u16::from_be_bytes([segment[0], segment[1]]);
    let dst_port = u16::from_be_bytes([segment[2], segment[3]]);
    if dst_port != LISTEN_PORT {
        return;
    }

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

    // SAFETY: single-threaded kernel; the connection state is only ever
    // touched from the timer-IRQ RX drain (net_stack::tick).
    let c = unsafe { &mut *(&raw mut CONN) };

    // A RST always drops us back to LISTEN.
    if flags & RST != 0 {
        c.state = State::Listen;
        return;
    }

    match c.state {
        State::Listen => {
            if flags & SYN != 0 {
                // Passive open: latch the peer and answer SYN/ACK.
                let iss = unsafe {
                    ISS_COUNTER = ISS_COUNTER.wrapping_add(0x4000);
                    ISS_COUNTER
                };
                c.peer_mac = src_mac;
                c.peer_ip = src_ip;
                c.peer_port = src_port;
                c.rcv_nxt = seq.wrapping_add(1); // SYN consumes one seq
                c.snd_nxt = iss;
                send_segment(c, our_ip, SYN | ACK, &[]);
                c.snd_nxt = c.snd_nxt.wrapping_add(1); // our SYN consumes one
                c.state = State::SynReceived;
            } else {
                // Stray segment to a listening port — refuse politely.
                send_rst(src_mac, our_ip, src_ip, src_port, seq, ack, flags);
            }
        }

        State::SynReceived => {
            // Expect the handshake-completing ACK.
            if flags & ACK != 0 && ack == c.snd_nxt && src_port == c.peer_port {
                c.state = State::Established;
                // Greet the client the moment the channel is open.
                console::on_connect(c, our_ip);
            }
        }

        State::Established => {
            if src_port != c.peer_port {
                return;
            }
            // Accept in-order data only; anything else just gets a
            // duplicate ACK of where we are.
            if !payload.is_empty() && seq == c.rcv_nxt {
                c.rcv_nxt = c.rcv_nxt.wrapping_add(payload.len() as u32);
                console::on_data(c, our_ip, payload);
                // console::on_data already piggy-backed any reply +
                // current ACK; if it produced no output it still must
                // ACK the data, which it does via send_segment(ACK).
            } else if !payload.is_empty() {
                // Out-of-order / retransmit — re-ACK our cumulative point.
                send_segment(c, our_ip, ACK, &[]);
            }

            if flags & FIN != 0 {
                // Peer is closing. ACK their FIN, then send ours.
                c.rcv_nxt = c.rcv_nxt.wrapping_add(1); // FIN consumes one seq
                send_segment(c, our_ip, ACK, &[]);
                send_segment(c, our_ip, FIN | ACK, &[]);
                c.snd_nxt = c.snd_nxt.wrapping_add(1);
                c.state = State::LastAck;
            }
        }

        State::LastAck => {
            // Final ACK of our FIN closes the books.
            if flags & ACK != 0 && ack == c.snd_nxt {
                c.state = State::Listen;
            }
        }
    }
}

/// Send a bare RST for an unexpected segment (no live connection).
fn send_rst(
    dst_mac: [u8; 6],
    our_ip: [u8; 4],
    peer_ip: [u8; 4],
    peer_port: u16,
    their_seq: u32,
    their_ack: u32,
    their_flags: u8,
) {
    // Per RFC 793: if the incoming segment carried an ACK, our RST seq
    // is their ack; otherwise seq=0 and we ACK their seq+payload.
    let (seq, ack, flags) = if their_flags & ACK != 0 {
        (their_ack, 0, RST)
    } else {
        (0, their_seq.wrapping_add(1), RST | ACK)
    };
    let mut tmp = Conn {
        state: State::Listen,
        peer_mac: dst_mac,
        peer_ip,
        peer_port,
        snd_nxt: seq,
        rcv_nxt: ack,
    };
    send_segment(&mut tmp, our_ip, flags, &[]);
}

// ============================================================================
// Outbound segment builder
// ============================================================================

/// Build and transmit one ETH+IPv4+TCP frame. `flags` are the TCP
/// control bits; `payload` is the (possibly empty) data. Uses
/// `conn.snd_nxt` as the sequence number and `conn.rcv_nxt` as the ACK.
fn send_segment(conn: &mut Conn, our_ip: [u8; 4], flags: u8, payload: &[u8]) {
    let our_mac = match net::get_mac() {
        Some(m) => m,
        None => return,
    };
    let payload = &payload[..payload.len().min(MAX_SEG_PAYLOAD)];

    let tcp_len = 20 + payload.len();
    let ip_total = 20 + tcp_len;
    let total = 14 + ip_total;
    let mut pkt = [0u8; 14 + 20 + 20 + MAX_SEG_PAYLOAD];

    // --- Ethernet ---
    pkt[0..6].copy_from_slice(&conn.peer_mac);
    pkt[6..12].copy_from_slice(&our_mac);
    pkt[12..14].copy_from_slice(&[0x08, 0x00]);

    // --- IPv4 ---
    pkt[14] = 0x45;
    pkt[15] = 0x00;
    pkt[16..18].copy_from_slice(&(ip_total as u16).to_be_bytes());
    pkt[18..20].copy_from_slice(&[0x00, 0x00]); // ID
    pkt[20..22].copy_from_slice(&[0x40, 0x00]); // DF
    pkt[22] = 64; // TTL
    pkt[23] = 6;  // protocol TCP
    pkt[24..26].copy_from_slice(&[0x00, 0x00]); // checksum (filled below)
    pkt[26..30].copy_from_slice(&our_ip);
    pkt[30..34].copy_from_slice(&conn.peer_ip);
    let ip_csum = net_stack::internet_checksum(&pkt[14..34]);
    pkt[24..26].copy_from_slice(&ip_csum.to_be_bytes());

    // --- TCP ---
    let t = 34;
    pkt[t..t + 2].copy_from_slice(&LISTEN_PORT.to_be_bytes());
    pkt[t + 2..t + 4].copy_from_slice(&conn.peer_port.to_be_bytes());
    pkt[t + 4..t + 8].copy_from_slice(&conn.snd_nxt.to_be_bytes());
    pkt[t + 8..t + 12].copy_from_slice(&conn.rcv_nxt.to_be_bytes());
    pkt[t + 12] = 0x50; // data offset = 5 words (20 bytes), no options
    pkt[t + 13] = flags;
    pkt[t + 14..t + 16].copy_from_slice(&RCV_WINDOW.to_be_bytes());
    pkt[t + 16..t + 18].copy_from_slice(&[0x00, 0x00]); // checksum
    pkt[t + 18..t + 20].copy_from_slice(&[0x00, 0x00]); // urgent ptr
    pkt[t + 20..t + 20 + payload.len()].copy_from_slice(payload);

    // TCP checksum over pseudo-header + TCP segment.
    let csum = tcp_checksum(our_ip, conn.peer_ip, &pkt[t..t + tcp_len]);
    pkt[t + 16..t + 18].copy_from_slice(&csum.to_be_bytes());

    // Advance our send sequence past any data we just shipped.
    conn.snd_nxt = conn.snd_nxt.wrapping_add(payload.len() as u32);

    net::send_packet(&pkt[..total]);
}

/// TCP checksum: internet checksum over the 12-byte pseudo-header
/// (src IP, dst IP, zero, protocol=6, TCP length) followed by the TCP
/// header+payload.
fn tcp_checksum(src_ip: [u8; 4], dst_ip: [u8; 4], tcp: &[u8]) -> u16 {
    // Pseudo-header (12) + tcp, padded to even by internet_checksum.
    let mut buf = [0u8; 12 + 20 + MAX_SEG_PAYLOAD];
    buf[0..4].copy_from_slice(&src_ip);
    buf[4..8].copy_from_slice(&dst_ip);
    buf[8] = 0;
    buf[9] = 6; // protocol
    buf[10..12].copy_from_slice(&(tcp.len() as u16).to_be_bytes());
    buf[12..12 + tcp.len()].copy_from_slice(tcp);
    net_stack::internet_checksum(&buf[..12 + tcp.len()])
}

// ============================================================================
// Remote console — the "application" sitting on top of the socket.
// ============================================================================

mod console {
    use super::{send_segment, Conn, ACK, PSH};
    use super::net_stack;

    /// Greeting sent the moment the connection is established.
    pub fn on_connect(conn: &mut Conn, our_ip: [u8; 4]) {
        let banner = b"BeetOS remote console (M7)\r\nType 'help'.\r\n> ";
        send_segment(conn, our_ip, PSH | ACK, banner);
    }

    /// Interpret one chunk of received bytes as console input. The QEMU
    /// loopback delivers a whole line per segment in practice, so we
    /// treat the payload as a command line (trimming CR/LF) rather than
    /// maintaining a cross-segment line buffer.
    pub fn on_data(conn: &mut Conn, our_ip: [u8; 4], payload: &[u8]) {
        // A scratch reply buffer; commands keep their output well under
        // a single segment.
        let mut out = [0u8; 512];
        let mut n = 0;
        let push = |s: &[u8], out: &mut [u8; 512], n: &mut usize| {
            let take = s.len().min(out.len() - *n);
            out[*n..*n + take].copy_from_slice(&s[..take]);
            *n += take;
        };

        let line = trim(payload);
        match cmd_word(line) {
            b"" => {}
            b"help" => push(
                b"commands: help, ip, ping, uptime, echo <text>, quit\r\n",
                &mut out, &mut n,
            ),
            b"ip" => {
                let ip = net_stack::get_ip();
                let mut tmp = [0u8; 24];
                let len = fmt_ip(ip, &mut tmp);
                push(&tmp[..len], &mut out, &mut n);
                push(b"\r\n", &mut out, &mut n);
            }
            b"ping" => push(b"pong\r\n", &mut out, &mut n),
            b"uptime" => {
                let ticks = super::super::timer::tick_count();
                let mut tmp = [0u8; 32];
                let len = fmt_uptime(ticks, &mut tmp);
                push(&tmp[..len], &mut out, &mut n);
            }
            b"echo" => {
                let rest = after_word(line);
                push(rest, &mut out, &mut n);
                push(b"\r\n", &mut out, &mut n);
            }
            b"quit" => {
                push(b"bye\r\n", &mut out, &mut n);
                push(b"> ", &mut out, &mut n);
                send_segment(conn, our_ip, PSH | ACK, &out[..n]);
                // Initiate close from our side next: easiest is to wait
                // for the client's FIN, but we nudge with our prompt and
                // let the teardown happen on their disconnect.
                return;
            }
            other => {
                push(b"unknown command: ", &mut out, &mut n);
                push(other, &mut out, &mut n);
                push(b"\r\n", &mut out, &mut n);
            }
        }
        push(b"> ", &mut out, &mut n);

        // One segment: ACKs the received data and carries the reply.
        send_segment(conn, our_ip, PSH | ACK, &out[..n]);
    }

    fn trim(b: &[u8]) -> &[u8] {
        let mut end = b.len();
        while end > 0 && (b[end - 1] == b'\r' || b[end - 1] == b'\n' || b[end - 1] == b' ') {
            end -= 1;
        }
        let mut start = 0;
        while start < end && b[start] == b' ' {
            start += 1;
        }
        &b[start..end]
    }

    fn cmd_word(line: &[u8]) -> &[u8] {
        let mut i = 0;
        while i < line.len() && line[i] != b' ' {
            i += 1;
        }
        &line[..i]
    }

    fn after_word(line: &[u8]) -> &[u8] {
        let mut i = 0;
        while i < line.len() && line[i] != b' ' {
            i += 1;
        }
        while i < line.len() && line[i] == b' ' {
            i += 1;
        }
        &line[i..]
    }

    fn fmt_ip(ip: [u8; 4], out: &mut [u8; 24]) -> usize {
        let mut n = 0;
        for (i, octet) in ip.iter().enumerate() {
            if i > 0 {
                out[n] = b'.';
                n += 1;
            }
            n += fmt_u32(*octet as u32, &mut out[n..]);
        }
        n
    }

    fn fmt_uptime(ticks: u64, out: &mut [u8; 32]) -> usize {
        // Timer runs at 100 Hz.
        let secs = ticks / 100;
        let mut n = 0;
        n += fmt_u32((secs % 1_000_000) as u32, &mut out[n..]);
        let tail = b" s up\r\n";
        out[n..n + tail.len()].copy_from_slice(tail);
        n + tail.len()
    }

    /// Minimal decimal formatter for a u32 into `out`; returns bytes written.
    fn fmt_u32(mut v: u32, out: &mut [u8]) -> usize {
        if v == 0 {
            out[0] = b'0';
            return 1;
        }
        let mut digits = [0u8; 10];
        let mut d = 0;
        while v > 0 {
            digits[d] = b'0' + (v % 10) as u8;
            v /= 10;
            d += 1;
        }
        for i in 0..d {
            out[i] = digits[d - 1 - i];
        }
        d
    }
}
