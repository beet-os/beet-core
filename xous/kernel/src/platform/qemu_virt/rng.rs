// SPDX-FileCopyrightText: 2026 BeetOS contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! virtio-rng (entropy device) driver for QEMU virt.
//!
//! Feeds real host entropy to the kernel RNG. QEMU's default
//! `neoverse-n1` CPU predates FEAT_RNG, so without this device
//! `GetRandom` degrades to a counter-mixed xorshift PRNG — acceptable
//! as a last resort, but the AES-GCM nonces in `api/cryptblock` deserve
//! better on the primary dev/CI target.
//!
//! Protocol (virtio spec v1.2, §5.4 Entropy Device): device ID 4, one
//! requestq. The driver queues a device-writable buffer; the device
//! fills it with entropy and reports the byte count in the used ring.
//! No request header, no status byte — the simplest virtio device
//! there is.
//!
//! Usage model: synchronous polling, no IRQ. Entropy is consumed from
//! a small pool refilled on demand. Callers are syscall-context only
//! (`GetRandom`, server-ID generation); no IRQ path touches this
//! module, so the `static mut` state is single-threaded by
//! construction, same as `blk.rs`.

use super::virtio::{self, Virtqueue, VIRTQ_DESC_F_WRITE};
use core::sync::atomic::{fence, Ordering};

/// Queue size: entropy requests are one-at-a-time; 8 is generous.
const QUEUE_SIZE: u16 = 8;

/// Bytes fetched from the device per refill. One GetRandom syscall
/// consumes 16 bytes, so a 64-byte pool amortizes the virtqueue round
/// trip over four syscalls.
const POOL_SIZE: usize = 64;

/// Bounded poll before declaring the device dead (same order of
/// magnitude as blk.rs; QEMU answers in far fewer spins).
const POLL_SPINS_MAX: u32 = 10_000_000;

struct RngDevice {
    /// MMIO transport kernel VA.
    base_va: usize,
    /// The requestq (queue 0).
    queue: Virtqueue,
}

/// Global device state. None if no entropy device was found.
static mut RNG_DEV: Option<RngDevice> = None;

/// Virtqueue memory: descriptors + avail ring + padding + used ring,
/// page-aligned for the legacy QueuePFN register.
#[repr(C, align(16384))]
struct VirtqueueBuffer {
    data: [u8; virtio::Virtqueue::size_bytes(QUEUE_SIZE as usize, beetos::PAGE_SIZE)],
}

static mut VQUEUE_BUF: VirtqueueBuffer = VirtqueueBuffer {
    data: [0u8; virtio::Virtqueue::size_bytes(QUEUE_SIZE as usize, beetos::PAGE_SIZE)],
};

/// DMA target the device writes entropy into (kernel BSS: PA = VA -
/// KERNEL_VA_OFFSET, same trick as blk.rs's request buffers).
static mut ENTROPY_BUF: [u8; POOL_SIZE] = [0; POOL_SIZE];

/// Consumption pool: `POOL[..POOL_LEN]` holds not-yet-consumed bytes.
static mut POOL: [u8; POOL_SIZE] = [0; POOL_SIZE];
static mut POOL_LEN: usize = 0;

/// Probe all virtio MMIO transports and initialize the first entropy
/// device found. Called during platform init.
pub fn probe_and_init(virtio_base_va: usize) {
    for i in 0..virtio::NUM_TRANSPORTS {
        let base = virtio_base_va + i * virtio::TRANSPORT_SIZE;
        if let Some(device_id) = virtio::probe_transport(base) {
            if device_id == virtio::DEVICE_ID_ENTROPY {
                if init_rng_device(base) {
                    return;
                }
            }
        }
    }
    // No entropy device — fine, GetRandom falls back to the arch PRNG.
}

/// Four bytes of hardware entropy, or None when no device is present
/// (or it stopped answering — callers fall back to the arch PRNG).
pub fn get_u32() -> Option<u32> {
    unsafe {
        let pool_len = *(&raw const POOL_LEN);
        if pool_len < 4 && !refill() {
            return None;
        }
        let pool_len = *(&raw const POOL_LEN);
        if pool_len < 4 {
            return None; // device answered with < 4 bytes — treat as dry
        }
        let pool = &mut *(&raw mut POOL);
        let start = pool_len - 4;
        let val = u32::from_le_bytes([
            pool[start], pool[start + 1], pool[start + 2], pool[start + 3],
        ]);
        // Hygiene: never leave consumed entropy lying around in BSS.
        pool[start..pool_len].fill(0);
        *(&raw mut POOL_LEN) = start;
        Some(val)
    }
}

fn init_rng_device(base_va: usize) -> bool {
    // No feature bits defined for the entropy device.
    if virtio::init_device(base_va, 0).is_none() {
        return false;
    }

    let buf_va = unsafe { (*(&raw mut VQUEUE_BUF)).data.as_mut_ptr() as usize };
    let buf_pa = beetos::virt_to_phys(buf_va);
    unsafe {
        core::ptr::write_bytes(
            buf_va as *mut u8,
            0,
            virtio::Virtqueue::size_bytes(QUEUE_SIZE as usize, beetos::PAGE_SIZE),
        );
    }
    let queue = unsafe { Virtqueue::init(buf_va, buf_pa, QUEUE_SIZE, beetos::PAGE_SIZE) };

    virtio::setup_queue(base_va, 0, &queue, beetos::PAGE_SIZE);
    virtio::driver_ok(base_va);
    // Deliberately no GIC enable: we poll synchronously and ack the
    // (never-delivered) interrupt status after each completion.

    unsafe {
        RNG_DEV = Some(RngDevice { base_va, queue });
    }

    use core::fmt::Write;
    let _ = write!(super::uart::UartWriter, "virtio-rng: entropy source ready\n");
    true
}

/// Ask the device for a pool's worth of fresh entropy. Returns false
/// on timeout / missing device; true when at least one byte landed.
fn refill() -> bool {
    unsafe {
        let dev = match (*(&raw mut RNG_DEV)).as_mut() {
            Some(d) => d,
            None => return false,
        };
        let q = &mut dev.queue;

        let d0 = match q.alloc_desc() {
            Some(d) => d,
            None => return false,
        };
        let buf_pa = beetos::virt_to_phys((&raw const ENTROPY_BUF) as usize);
        let desc = &mut *q.desc.add(d0 as usize);
        desc.addr = buf_pa as u64;
        desc.len = POOL_SIZE as u32;
        desc.flags = VIRTQ_DESC_F_WRITE; // device fills the buffer
        desc.next = 0;

        fence(Ordering::Release);
        q.push_avail(d0);
        virtio::notify(dev.base_va, 0);

        let mut got: u32 = 0;
        let mut spins: u32 = 0;
        let mut wedged = false;
        loop {
            if let Some((head, len)) = q.pop_used() {
                q.free_chain(head);
                got = len.min(POOL_SIZE as u32);
                break;
            }
            spins += 1;
            if spins > POLL_SPINS_MAX {
                wedged = true;
                break;
            }
            core::hint::spin_loop();
        }
        // Copy what we need out of `dev` BEFORE any write to RNG_DEV —
        // `dev` borrows through the same static, so it must be dead by
        // the time we overwrite the Option.
        let base_va = dev.base_va;
        if wedged {
            // Device wedged: drop it so subsequent calls fall back to
            // the arch PRNG instead of re-spinning every time.
            *(&raw mut RNG_DEV) = None;
            return false;
        }
        virtio::ack_interrupt(base_va);

        fence(Ordering::Acquire);
        let n = got as usize;
        let src = &*(&raw const ENTROPY_BUF);
        let pool = &mut *(&raw mut POOL);
        pool[..n].copy_from_slice(&src[..n]);
        *(&raw mut POOL_LEN) = n;
        n > 0
    }
}
