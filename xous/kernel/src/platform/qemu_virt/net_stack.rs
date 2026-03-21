// SPDX-FileCopyrightText: 2024 BeetOS contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Minimal ARP / DHCP / ICMP network stack for BeetOS.
//!
//! Implements just enough networking to:
//!   - Get an IP via DHCP (QEMU user-mode networking → 10.0.2.15)
//!   - Respond to ARP requests for our IP
//!   - Respond to ICMP echo (ping) requests
//!
//! No heap allocator required — all state lives in static variables.
//! `tick()` is called from the timer IRQ at 100 Hz.

use super::net;

// ============================================================================
// State
// ============================================================================

#[derive(Clone, Copy, PartialEq)]
enum DhcpState {
    Init,
    Discovering,
    Requesting,
    Bound,
}

struct NetState {
    ip: [u8; 4],
    gateway: [u8; 4],
    server_ip: [u8; 4],
    dhcp_state: DhcpState,
    dhcp_xid: u32,
    dhcp_retry: u64,  // tick count at which to retry
    req_retries: u8,
}

static mut STATE: NetState = NetState {
    ip: [0u8; 4],
    gateway: [0u8; 4],
    server_ip: [0u8; 4],
    dhcp_state: DhcpState::Init,
    dhcp_xid: 0,
    dhcp_retry: 0,
    req_retries: 0,
};

/// Retry DHCP every 2 seconds (200 ticks at 100 Hz).
const DHCP_RETRY_TICKS: u64 = 200;

// ============================================================================
// Public API
// ============================================================================

/// Initialize the network stack. Called after the net device is probed.
pub fn init() {
    unsafe {
        if let Some(mac) = net::get_mac() {
            // Derive a pseudo-random XID from the last 4 MAC bytes.
            STATE.dhcp_xid = u32::from_be_bytes([mac[2], mac[3], mac[4], mac[5]]);
            STATE.dhcp_state = DhcpState::Discovering;
            STATE.dhcp_retry = 0; // send DISCOVER on the very first tick
        }
    }
}

/// Called every timer tick. Drains received packets and manages the DHCP state machine.
pub fn tick(tick_count: u64) {
    if !net::is_available() {
        return;
    }

    // Process all packets that arrived since the last tick.
    while let Some((desc_head, frame_len)) = net::poll_recv() {
        // Copy frame to a local buffer before returning the descriptor,
        // so the device can reuse the RX buffer immediately.
        let mut buf = [0u8; 1518];
        let len = frame_len.min(1518);
        buf[..len].copy_from_slice(net::get_rx_frame(desc_head, frame_len));
        net::return_rx_buffer(desc_head);
        process_frame(&buf[..len]);
    }

    // DHCP state machine retransmission.
    unsafe {
        let s = &mut STATE;
        match s.dhcp_state {
            DhcpState::Init => {}
            DhcpState::Discovering => {
                if tick_count >= s.dhcp_retry {
                    send_dhcp_discover();
                    s.dhcp_retry = tick_count + DHCP_RETRY_TICKS;
                }
            }
            DhcpState::Requesting => {
                if tick_count >= s.dhcp_retry {
                    s.req_retries += 1;
                    if s.req_retries > 3 {
                        // Fall back to discovery.
                        s.dhcp_state = DhcpState::Discovering;
                        s.req_retries = 0;
                        s.dhcp_retry = tick_count + DHCP_RETRY_TICKS;
                    } else {
                        send_dhcp_request(s.ip, s.server_ip);
                        s.dhcp_retry = tick_count + DHCP_RETRY_TICKS;
                    }
                }
            }
            DhcpState::Bound => {}
        }
    }
}

/// Returns the current IPv4 address (all zeros if DHCP has not completed).
pub fn get_ip() -> [u8; 4] {
    unsafe { STATE.ip }
}

// ============================================================================
// Packet processing
// ============================================================================

fn process_frame(frame: &[u8]) {
    if frame.len() < 14 {
        return;
    }
    let ethertype = u16::from_be_bytes([frame[12], frame[13]]);

    match ethertype {
        0x0806 => handle_arp(&frame[14..]),
        0x0800 => handle_ipv4(frame, &frame[14..]),
        _ => {}
    }
}

fn handle_arp(arp: &[u8]) {
    if arp.len() < 28 {
        return;
    }
    let op = u16::from_be_bytes([arp[6], arp[7]]);
    if op != 1 {
        return; // only handle ARP requests
    }

    let target_ip: [u8; 4] = match arp[24..28].try_into() {
        Ok(v) => v,
        Err(_) => return,
    };
    let our_ip = unsafe { STATE.ip };
    if our_ip == [0, 0, 0, 0] || target_ip != our_ip {
        return; // not for us or no IP yet
    }

    let sender_mac: [u8; 6] = match arp[8..14].try_into() {
        Ok(v) => v,
        Err(_) => return,
    };
    let sender_ip: [u8; 4] = match arp[14..18].try_into() {
        Ok(v) => v,
        Err(_) => return,
    };
    let our_mac = match net::get_mac() {
        Some(m) => m,
        None => return,
    };

    let mut pkt = [0u8; 42]; // ETH(14) + ARP(28)
    // Ethernet header
    pkt[0..6].copy_from_slice(&sender_mac);
    pkt[6..12].copy_from_slice(&our_mac);
    pkt[12..14].copy_from_slice(&[0x08, 0x06]);
    // ARP reply
    let a = &mut pkt[14..];
    a[0..2].copy_from_slice(&[0x00, 0x01]); // hw type: Ethernet
    a[2..4].copy_from_slice(&[0x08, 0x00]); // proto: IPv4
    a[4] = 6;
    a[5] = 4;
    a[6..8].copy_from_slice(&[0x00, 0x02]); // op: reply
    a[8..14].copy_from_slice(&our_mac);
    a[14..18].copy_from_slice(&our_ip);
    a[18..24].copy_from_slice(&sender_mac);
    a[24..28].copy_from_slice(&sender_ip);
    net::send_packet(&pkt);
}

fn handle_ipv4(frame: &[u8], ip: &[u8]) {
    if ip.len() < 20 {
        return;
    }
    let ihl = ((ip[0] & 0x0F) as usize) * 4;
    if ihl < 20 || ip.len() < ihl {
        return;
    }
    let proto = ip[9];
    let src_ip: [u8; 4] = match ip[12..16].try_into() {
        Ok(v) => v,
        Err(_) => return,
    };
    let dst_ip: [u8; 4] = match ip[16..20].try_into() {
        Ok(v) => v,
        Err(_) => return,
    };
    let payload = &ip[ihl..];

    match proto {
        1 => handle_icmp(frame, src_ip, dst_ip, payload),
        17 => handle_udp(src_ip, dst_ip, payload),
        _ => {}
    }
}

fn handle_icmp(frame: &[u8], src_ip: [u8; 4], dst_ip: [u8; 4], icmp: &[u8]) {
    let our_ip = unsafe { STATE.ip };
    if our_ip == [0, 0, 0, 0] || dst_ip != our_ip {
        return;
    }
    if icmp.len() < 8 || icmp[0] != 8 {
        return; // not ICMP echo request
    }

    let src_mac: [u8; 6] = match frame[6..12].try_into() {
        Ok(v) => v,
        Err(_) => return,
    };
    let our_mac = match net::get_mac() {
        Some(m) => m,
        None => return,
    };

    let payload_len = icmp.len();
    let copy_len = payload_len.saturating_sub(4);
    let total = 14 + 20 + payload_len;
    if total > 1518 {
        return; // oversized
    }

    let mut reply = [0u8; 1518];
    // Ethernet
    reply[0..6].copy_from_slice(&src_mac);
    reply[6..12].copy_from_slice(&our_mac);
    reply[12..14].copy_from_slice(&[0x08, 0x00]);

    // IPv4
    let ip_total = (20 + payload_len) as u16;
    reply[14] = 0x45;
    reply[15] = 0;
    reply[16..18].copy_from_slice(&ip_total.to_be_bytes());
    reply[18..20].copy_from_slice(&[0x00, 0x00]); // ID
    reply[20..22].copy_from_slice(&[0x00, 0x00]); // flags + fragment offset
    reply[22] = 64;                                // TTL
    reply[23] = 1;                                 // protocol: ICMP
    reply[24..26].copy_from_slice(&[0x00, 0x00]); // checksum placeholder
    reply[26..30].copy_from_slice(&our_ip);
    reply[30..34].copy_from_slice(&src_ip);
    let ip_csum = internet_checksum(&reply[14..34]);
    reply[24..26].copy_from_slice(&ip_csum.to_be_bytes());

    // ICMP echo reply: type=0, same code/id/seq/data
    reply[34] = 0; // type: echo reply
    reply[35] = 0; // code
    reply[36..38].copy_from_slice(&[0x00, 0x00]); // checksum placeholder
    if copy_len > 0 {
        reply[38..38 + copy_len].copy_from_slice(&icmp[4..4 + copy_len]);
    }
    let icmp_csum = internet_checksum(&reply[34..34 + payload_len]);
    reply[36..38].copy_from_slice(&icmp_csum.to_be_bytes());

    net::send_packet(&reply[..total]);
}

fn handle_udp(src_ip: [u8; 4], _dst_ip: [u8; 4], udp: &[u8]) {
    if udp.len() < 8 {
        return;
    }
    let src_port = u16::from_be_bytes([udp[0], udp[1]]);
    let dst_port = u16::from_be_bytes([udp[2], udp[3]]);
    // DHCP response: server port 67 → client port 68.
    if src_port == 67 && dst_port == 68 && udp.len() > 8 {
        handle_dhcp(src_ip, &udp[8..]);
    }
}

// ============================================================================
// DHCP client
// ============================================================================

// DHCP message offsets (relative to start of DHCP payload).
const DHCP_MAGIC: [u8; 4] = [99, 130, 83, 99];
const DHCP_OPT_MSG_TYPE: u8 = 53;
const DHCP_OPT_SERVER_ID: u8 = 54;
const DHCP_OPT_REQUESTED_IP: u8 = 50;
const DHCP_OPT_SUBNET_MASK: u8 = 1;
const DHCP_OPT_ROUTER: u8 = 3;
const DHCP_MSG_OFFER: u8 = 2;
const DHCP_MSG_ACK: u8 = 5;

fn handle_dhcp(server_ip: [u8; 4], dhcp: &[u8]) {
    if dhcp.len() < 240 {
        return;
    }
    // Must be a BOOTREPLY with the correct magic cookie.
    if dhcp[0] != 2 || dhcp[236..240] != DHCP_MAGIC {
        return;
    }
    let xid = u32::from_be_bytes([dhcp[4], dhcp[5], dhcp[6], dhcp[7]]);
    if xid != unsafe { STATE.dhcp_xid } {
        return;
    }

    let yiaddr: [u8; 4] = match dhcp[16..20].try_into() {
        Ok(v) => v,
        Err(_) => return,
    };

    // Parse options.
    let mut msg_type: u8 = 0;
    let mut offered_server_ip = server_ip;
    let mut router = [0u8; 4];

    let mut i = 240usize;
    while i < dhcp.len() {
        let opt = dhcp[i];
        i += 1;
        match opt {
            0 => {}    // pad byte
            255 => break, // end
            _ => {
                if i >= dhcp.len() {
                    break;
                }
                let len = dhcp[i] as usize;
                i += 1;
                if i + len > dhcp.len() {
                    break;
                }
                match opt {
                    DHCP_OPT_MSG_TYPE if len >= 1 => msg_type = dhcp[i],
                    DHCP_OPT_SERVER_ID if len >= 4 => {
                        offered_server_ip.copy_from_slice(&dhcp[i..i + 4]);
                    }
                    DHCP_OPT_ROUTER if len >= 4 => {
                        router.copy_from_slice(&dhcp[i..i + 4]);
                    }
                    _ => {}
                }
                i += len;
            }
        }
    }

    unsafe {
        let s = &mut STATE;
        match (s.dhcp_state, msg_type) {
            (DhcpState::Discovering, DHCP_MSG_OFFER) => {
                // Got OFFER — record offered config and send REQUEST.
                s.ip = yiaddr;
                s.server_ip = offered_server_ip;
                s.gateway = router;
                s.dhcp_state = DhcpState::Requesting;
                s.req_retries = 0;
                s.dhcp_retry = 0; // trigger immediate REQUEST on next tick
            }
            (DhcpState::Requesting, DHCP_MSG_ACK) => {
                // Got ACK — we're bound!
                s.ip = yiaddr;
                s.server_ip = offered_server_ip;
                s.gateway = router;
                s.dhcp_state = DhcpState::Bound;

                use core::fmt::Write;
                let _ = write!(
                    crate::platform::qemu_virt::uart::UartWriter,
                    "virtio-net: IP={}.{}.{}.{} GW={}.{}.{}.{}\n",
                    yiaddr[0], yiaddr[1], yiaddr[2], yiaddr[3],
                    router[0], router[1], router[2], router[3],
                );
            }
            _ => {}
        }
    }
}

/// Frame layout (all offsets from start of `pkt`):
///   [0..14]   Ethernet header
///   [14..34]  IPv4 header
///   [34..42]  UDP header
///   [42..278] DHCP fixed fields (236 bytes)
///   [278..282] DHCP magic cookie
///   [282..338] DHCP options (56 bytes max)
const PKT_SIZE: usize = 338;
const DHCP_OPTS_OFF: usize = 14 + 20 + 8 + 236 + 4; // = 282

fn send_dhcp_discover() {
    let mac = match net::get_mac() {
        Some(m) => m,
        None => return,
    };
    let xid = unsafe { STATE.dhcp_xid };
    let mut pkt = [0u8; PKT_SIZE];
    build_dhcp_eth_ip_udp(&mut pkt, mac, xid);

    let o = &mut pkt[DHCP_OPTS_OFF..];
    o[0] = DHCP_OPT_MSG_TYPE; o[1] = 1; o[2] = 1; // DISCOVER
    o[3] = DHCP_OPT_SUBNET_MASK;                    // param request: subnet mask
    // Actually encode as option 55 (Parameter Request List)
    o[3] = 55; o[4] = 2; o[5] = DHCP_OPT_SUBNET_MASK; o[6] = DHCP_OPT_ROUTER;
    o[7] = 255; // end

    finalize_ipv4_checksum(&mut pkt);
    net::send_packet(&pkt);
}

fn send_dhcp_request(requested_ip: [u8; 4], server_ip: [u8; 4]) {
    let mac = match net::get_mac() {
        Some(m) => m,
        None => return,
    };
    let xid = unsafe { STATE.dhcp_xid };
    let mut pkt = [0u8; PKT_SIZE];
    build_dhcp_eth_ip_udp(&mut pkt, mac, xid);

    let o = &mut pkt[DHCP_OPTS_OFF..];
    o[0] = DHCP_OPT_MSG_TYPE; o[1] = 1; o[2] = 3;             // REQUEST
    o[3] = DHCP_OPT_REQUESTED_IP; o[4] = 4;
    o[5..9].copy_from_slice(&requested_ip);
    o[9] = DHCP_OPT_SERVER_ID; o[10] = 4;
    o[11..15].copy_from_slice(&server_ip);
    o[15] = 255; // end

    finalize_ipv4_checksum(&mut pkt);
    net::send_packet(&pkt);
}

/// Fill in Ethernet, IPv4, UDP, and DHCP fixed fields. Checksum is written later.
fn build_dhcp_eth_ip_udp(buf: &mut [u8; PKT_SIZE], mac: [u8; 6], xid: u32) {
    // Ethernet header
    buf[0..6].copy_from_slice(&[0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF]); // broadcast
    buf[6..12].copy_from_slice(&mac);
    buf[12..14].copy_from_slice(&[0x08, 0x00]); // IPv4

    // IPv4 header (offset 14)
    let udp_payload_len: u16 = 8 + 236 + 4 + 56; // UDP hdr + DHCP fixed + magic + options
    let ip_total: u16 = 20 + udp_payload_len;
    buf[14] = 0x45; // version=4, IHL=5
    buf[15] = 0x10; // DSCP: minimize delay (DHCP convention)
    buf[16..18].copy_from_slice(&ip_total.to_be_bytes());
    buf[18..20].copy_from_slice(&[0x00, 0xAB]); // ID (arbitrary)
    buf[20..22].copy_from_slice(&[0x00, 0x00]); // flags + fragment offset
    buf[22] = 64;                                // TTL
    buf[23] = 17;                                // protocol: UDP
    buf[24..26].copy_from_slice(&[0x00, 0x00]); // checksum placeholder
    buf[26..30].copy_from_slice(&[0, 0, 0, 0]); // src: 0.0.0.0
    buf[30..34].copy_from_slice(&[255, 255, 255, 255]); // dst: broadcast

    // UDP header (offset 34)
    buf[34..36].copy_from_slice(&68u16.to_be_bytes()); // src port: 68 (DHCP client)
    buf[36..38].copy_from_slice(&67u16.to_be_bytes()); // dst port: 67 (DHCP server)
    buf[38..40].copy_from_slice(&udp_payload_len.to_be_bytes());
    buf[40..42].copy_from_slice(&[0x00, 0x00]); // UDP checksum (optional for IPv4)

    // DHCP fixed fields (offset 42)
    let d = &mut buf[42..];
    d[0] = 1;  // op: BOOTREQUEST
    d[1] = 1;  // htype: Ethernet
    d[2] = 6;  // hlen
    d[3] = 0;  // hops
    d[4..8].copy_from_slice(&xid.to_be_bytes());
    d[8..10].copy_from_slice(&[0x00, 0x00]);  // secs
    d[10..12].copy_from_slice(&[0x80, 0x00]); // flags: broadcast bit set
    // ciaddr, yiaddr, siaddr, giaddr: all zeros (already)
    // chaddr (offset 28 in DHCP = offset 28 in d):
    d[28..34].copy_from_slice(&mac);
    // sname (64 bytes), file (128 bytes): all zeros
    // magic cookie (offset 236 in d):
    d[236..240].copy_from_slice(&DHCP_MAGIC);
}

/// Compute and fill in the IPv4 header checksum (bytes 24–25 in `pkt`).
fn finalize_ipv4_checksum(pkt: &mut [u8]) {
    let csum = internet_checksum(&pkt[14..34]);
    pkt[24..26].copy_from_slice(&csum.to_be_bytes());
}

// ============================================================================
// Internet checksum (RFC 1071)
// ============================================================================

fn internet_checksum(data: &[u8]) -> u16 {
    let mut sum: u32 = 0;
    let mut i = 0;

    while i + 1 < data.len() {
        sum += u16::from_be_bytes([data[i], data[i + 1]]) as u32;
        i += 2;
    }
    if i < data.len() {
        sum += (data[i] as u32) << 8;
    }
    while sum >> 16 != 0 {
        sum = (sum & 0xFFFF) + (sum >> 16);
    }
    !(sum as u16)
}
