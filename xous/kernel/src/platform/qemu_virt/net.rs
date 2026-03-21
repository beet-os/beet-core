// SPDX-FileCopyrightText: 2024 BeetOS contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! virtio-net network device driver for QEMU virt.
//!
//! Implements send/receive of raw Ethernet frames on top of the virtio MMIO
//! transport. The driver scans all 32 MMIO transports for a network device
//! (DeviceID == 1), initializes it, and provides `send_packet`/`poll_recv`.
//!
//! Reference: virtio spec v1.2, section 5.1 (Network Device).

use super::virtio::{self, Virtqueue, VIRTQ_DESC_F_WRITE};
use core::sync::atomic::{fence, Ordering};

/// virtio-net device ID.
const VIRTIO_NET_DEVICE_ID: u32 = 1;

/// Feature bit: device has a MAC address in config space.
const VIRTIO_NET_F_MAC: u32 = 1 << 5;

/// VirtioNetHdr size in bytes (without VIRTIO_NET_F_MRG_RXBUF).
/// Prepended by the device to every received frame, and expected by the device
/// on every transmitted frame.
pub const VNET_HDR_SIZE: usize = 10;

/// Maximum Ethernet frame payload (1500 bytes MTU + 14 byte header).
const MAX_FRAME_SIZE: usize = 1518;

/// RX buffer size: VirtioNetHdr + max Ethernet frame, rounded to 16-byte boundary.
const RX_BUF_SIZE: usize = 1536;

/// Virtqueue depth (number of descriptors). 16 is plenty for polling.
const QUEUE_SIZE: u16 = 16;

// ============================================================================
// Device state
// ============================================================================

struct NetDev {
    base_va: usize,
    mac: [u8; 6],
    rx_queue: Virtqueue,
    tx_queue: Virtqueue,
    irq: u32,
    /// Maps virtqueue descriptor index → RX buffer index.
    rx_desc_to_buf: [usize; QUEUE_SIZE as usize],
}

static mut NET_DEV: Option<NetDev> = None;

// ============================================================================
// Static DMA buffers (all in kernel BSS — physically contiguous)
// ============================================================================

/// Virtqueue buffer type, aligned to page boundary for legacy QueuePFN.
#[repr(C, align(16384))]
struct VqBuf([u8; virtio::Virtqueue::size_bytes(QUEUE_SIZE as usize, beetos::PAGE_SIZE)]);

static mut RX_VQ_BUF: VqBuf =
    VqBuf([0u8; virtio::Virtqueue::size_bytes(QUEUE_SIZE as usize, beetos::PAGE_SIZE)]);

static mut TX_VQ_BUF: VqBuf =
    VqBuf([0u8; virtio::Virtqueue::size_bytes(QUEUE_SIZE as usize, beetos::PAGE_SIZE)]);

/// One RX packet buffer per descriptor slot.
#[repr(C, align(16))]
#[derive(Clone, Copy)]
struct RxBuf([u8; RX_BUF_SIZE]);

static mut RX_BUFS: [RxBuf; QUEUE_SIZE as usize] = [RxBuf([0u8; RX_BUF_SIZE]); QUEUE_SIZE as usize];

/// Single TX packet buffer: zeroed VirtioNetHdr + Ethernet frame.
static mut TX_BUF: [u8; RX_BUF_SIZE] = [0u8; RX_BUF_SIZE];

// ============================================================================
// Public API
// ============================================================================

/// Probe all virtio MMIO transports and initialize the first network device found.
/// Called during platform init (after MMU is up).
pub fn probe_and_init(virtio_base_va: usize) {
    for i in 0..virtio::NUM_TRANSPORTS {
        let base = virtio_base_va + i * virtio::TRANSPORT_SIZE;
        if let Some(device_id) = virtio::probe_transport(base) {
            if device_id == VIRTIO_NET_DEVICE_ID {
                if init_net_device(base, i) {
                    return;
                }
            }
        }
    }
    // No network device found — QEMU was not launched with -device virtio-net-device.
}

/// Returns true if a network device was found and initialized.
pub fn is_available() -> bool {
    unsafe { (*(&raw const NET_DEV)).is_some() }
}

/// Returns the device MAC address, or None if not initialized.
pub fn get_mac() -> Option<[u8; 6]> {
    unsafe { (*(&raw const NET_DEV)).as_ref().map(|d| d.mac) }
}

/// Returns the GIC IRQ number for the network device, or None if not initialized.
pub fn irq_number() -> Option<u32> {
    unsafe { (*(&raw const NET_DEV)).as_ref().map(|d| d.irq) }
}

/// Acknowledge and clear a virtio-net interrupt. Called from the IRQ handler.
pub fn handle_irq() {
    unsafe {
        if let Some(dev) = (*(&raw const NET_DEV)).as_ref() {
            virtio::ack_interrupt(dev.base_va);
        }
    }
}

/// Poll the RX used ring for a received packet.
///
/// Returns `(desc_head, frame_len)` if a packet is available, where `frame_len`
/// is the Ethernet frame length (excluding the VirtioNetHdr prepended by the device).
///
/// The caller must call `return_rx_buffer(desc_head)` after processing to
/// make the buffer available to the device again.
pub fn poll_recv() -> Option<(u16, usize)> {
    unsafe {
        let dev = (*(&raw mut NET_DEV)).as_mut()?;
        loop {
            let (desc_head, len) = dev.rx_queue.pop_used()?;
            let frame_len = (len as usize).saturating_sub(VNET_HDR_SIZE);
            if frame_len < 14 {
                // Runt or header-only — re-submit and skip.
                resubmit_rx_desc(dev, desc_head);
                continue;
            }
            return Some((desc_head, frame_len));
        }
    }
}

/// Return a slice of the received Ethernet frame for the given descriptor.
///
/// The slice is valid until `return_rx_buffer(desc_head)` is called.
///
/// # Safety
/// Only one RX descriptor should be "live" at a time (single-threaded kernel).
pub fn get_rx_frame(desc_head: u16, frame_len: usize) -> &'static [u8] {
    unsafe {
        let dev = (*(&raw const NET_DEV)).as_ref().expect("net not initialized");
        let buf_idx = dev.rx_desc_to_buf[desc_head as usize];
        &RX_BUFS[buf_idx].0[VNET_HDR_SIZE..VNET_HDR_SIZE + frame_len]
    }
}

/// Return an RX buffer to the RX queue after the caller has finished processing.
pub fn return_rx_buffer(desc_head: u16) {
    unsafe {
        if let Some(dev) = (*(&raw mut NET_DEV)).as_mut() {
            resubmit_rx_desc(dev, desc_head);
        }
    }
}

/// Send a raw Ethernet frame.
///
/// `frame` is the Ethernet frame bytes (14-byte header + payload).
/// The VirtioNetHdr is prepended automatically.
pub fn send_packet(frame: &[u8]) {
    if frame.len() > MAX_FRAME_SIZE {
        return;
    }
    unsafe {
        let dev = match (*(&raw mut NET_DEV)).as_mut() {
            Some(d) => d,
            None => return,
        };

        // Reclaim any already-completed TX descriptors.
        while let Some((head, _)) = dev.tx_queue.pop_used() {
            dev.tx_queue.free_chain(head);
        }

        let desc = match dev.tx_queue.alloc_desc() {
            Some(d) => d,
            None => return, // TX queue full — drop frame
        };

        // Build packet: zeroed VirtioNetHdr + Ethernet frame.
        let total_len = VNET_HDR_SIZE + frame.len();
        for b in &mut TX_BUF[..VNET_HDR_SIZE] {
            *b = 0;
        }
        TX_BUF[VNET_HDR_SIZE..VNET_HDR_SIZE + frame.len()].copy_from_slice(frame);

        let tx_pa = beetos::virt_to_phys((&raw const TX_BUF) as usize);
        {
            let d = &mut *dev.tx_queue.desc.add(desc as usize);
            d.addr = tx_pa as u64;
            d.len = total_len as u32;
            d.flags = 0; // device-readable (no WRITE flag)
            d.next = 0;
        }

        fence(Ordering::Release);
        dev.tx_queue.push_avail(desc);
        virtio::notify(dev.base_va, 1); // queue 1 = TX

        // Synchronous: poll until the device marks the descriptor as used.
        for _ in 0..1_000_000 {
            if let Some((head, _)) = dev.tx_queue.pop_used() {
                dev.tx_queue.free_chain(head);
                break;
            }
            core::hint::spin_loop();
        }
    }
}

// ============================================================================
// Internal helpers
// ============================================================================

fn init_net_device(base_va: usize, transport_idx: usize) -> bool {
    // Negotiate VIRTIO_NET_F_MAC only (no GSO, no MRG_RXBUF).
    let features = match virtio::init_device(base_va, VIRTIO_NET_F_MAC) {
        Some(f) => f,
        None => return false,
    };

    // Read MAC address from device config space (bytes 0–5).
    let mac = if features & VIRTIO_NET_F_MAC != 0 {
        let w0 = virtio::read_config_u32(base_va, 0); // bytes 0–3
        let w1 = virtio::read_config_u32(base_va, 4); // bytes 4–7 (we use 4–5)
        [
            (w0 & 0xFF) as u8,
            ((w0 >> 8) & 0xFF) as u8,
            ((w0 >> 16) & 0xFF) as u8,
            ((w0 >> 24) & 0xFF) as u8,
            (w1 & 0xFF) as u8,
            ((w1 >> 8) & 0xFF) as u8,
        ]
    } else {
        [0x52, 0x54, 0x00, 0x12, 0x34, 0x56] // QEMU default fallback
    };

    // Initialize RX virtqueue.
    let rx_va = unsafe { (*(&raw mut RX_VQ_BUF)).0.as_mut_ptr() as usize };
    let rx_pa = beetos::virt_to_phys(rx_va);
    unsafe {
        core::ptr::write_bytes(
            rx_va as *mut u8,
            0,
            virtio::Virtqueue::size_bytes(QUEUE_SIZE as usize, beetos::PAGE_SIZE),
        )
    };
    let mut rx_queue = unsafe { Virtqueue::init(rx_va, rx_pa, QUEUE_SIZE, beetos::PAGE_SIZE) };

    // Initialize TX virtqueue.
    let tx_va = unsafe { (*(&raw mut TX_VQ_BUF)).0.as_mut_ptr() as usize };
    let tx_pa = beetos::virt_to_phys(tx_va);
    unsafe {
        core::ptr::write_bytes(
            tx_va as *mut u8,
            0,
            virtio::Virtqueue::size_bytes(QUEUE_SIZE as usize, beetos::PAGE_SIZE),
        )
    };
    let tx_queue = unsafe { Virtqueue::init(tx_va, tx_pa, QUEUE_SIZE, beetos::PAGE_SIZE) };

    // Register both queues with the device.
    virtio::setup_queue(base_va, 0, &rx_queue, beetos::PAGE_SIZE);
    virtio::setup_queue(base_va, 1, &tx_queue, beetos::PAGE_SIZE);

    // Mark device ready.
    virtio::driver_ok(base_va);

    let irq = virtio::VIRTIO_IRQ_BASE + transport_idx as u32;
    super::gic::enable_irq(irq);

    // Pre-populate the RX queue with all available buffers.
    let mut rx_desc_to_buf = [0usize; QUEUE_SIZE as usize];

    for i in 0..QUEUE_SIZE as usize {
        if let Some(desc) = rx_queue.alloc_desc() {
            let buf_va = unsafe { RX_BUFS[i].0.as_mut_ptr() as usize };
            let buf_pa = beetos::virt_to_phys(buf_va);
            unsafe {
                let d = &mut *rx_queue.desc.add(desc as usize);
                d.addr = buf_pa as u64;
                d.len = RX_BUF_SIZE as u32;
                d.flags = VIRTQ_DESC_F_WRITE; // device-writable
                d.next = 0;
            }
            rx_desc_to_buf[desc as usize] = i;
            rx_queue.push_avail(desc);
        }
    }

    // Notify device of the new RX buffers.
    virtio::notify(base_va, 0);

    use core::fmt::Write;
    let _ = write!(
        super::uart::UartWriter,
        "virtio-net: MAC={:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}, IRQ {}\n",
        mac[0], mac[1], mac[2], mac[3], mac[4], mac[5], irq,
    );

    unsafe {
        NET_DEV = Some(NetDev {
            base_va,
            mac,
            rx_queue,
            tx_queue,
            irq,
            rx_desc_to_buf,
        });
    }

    true
}

/// Re-add an RX descriptor to the available ring after the caller processed its packet.
fn resubmit_rx_desc(dev: &mut NetDev, desc_head: u16) {
    let buf_idx = dev.rx_desc_to_buf[desc_head as usize];
    let buf_va = unsafe { RX_BUFS[buf_idx].0.as_mut_ptr() as usize };
    let buf_pa = beetos::virt_to_phys(buf_va);

    unsafe {
        let d = &mut *dev.rx_queue.desc.add(desc_head as usize);
        d.addr = buf_pa as u64;
        d.len = RX_BUF_SIZE as u32;
        d.flags = VIRTQ_DESC_F_WRITE;
        d.next = 0;
    }

    dev.rx_queue.push_avail(desc_head);
    virtio::notify(dev.base_va, 0);
}
