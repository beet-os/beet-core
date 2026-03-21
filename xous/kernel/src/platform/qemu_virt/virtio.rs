// SPDX-FileCopyrightText: 2024 BeetOS contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Generic virtio MMIO transport (legacy, version 1).
//!
//! Implements device discovery and virtqueue management for the QEMU virt
//! machine's MMIO-based virtio transports. Each transport occupies 0x200
//! bytes starting at 0x0A00_0000, with IRQs starting at SPI 16 (INTID 48).
//!
//! Reference: virtio spec v1.2, section 4.2 (Virtio Over MMIO).

use core::sync::atomic::{fence, Ordering};

// ============================================================================
// MMIO register offsets (legacy / version 1)
// ============================================================================

#[allow(dead_code)]
pub mod regs {
    pub const MAGIC_VALUE: usize = 0x000;
    pub const VERSION: usize = 0x004;
    pub const DEVICE_ID: usize = 0x008;
    pub const VENDOR_ID: usize = 0x00C;
    pub const HOST_FEATURES: usize = 0x010;
    pub const HOST_FEATURES_SEL: usize = 0x014;
    pub const GUEST_FEATURES: usize = 0x020;
    pub const GUEST_FEATURES_SEL: usize = 0x024;
    pub const GUEST_PAGE_SIZE: usize = 0x028;
    pub const QUEUE_SEL: usize = 0x030;
    pub const QUEUE_NUM_MAX: usize = 0x034;
    pub const QUEUE_NUM: usize = 0x038;
    pub const QUEUE_ALIGN: usize = 0x03C;
    pub const QUEUE_PFN: usize = 0x040;
    pub const QUEUE_NOTIFY: usize = 0x050;
    pub const INTERRUPT_STATUS: usize = 0x060;
    pub const INTERRUPT_ACK: usize = 0x064;
    pub const STATUS: usize = 0x070;
    pub const CONFIG: usize = 0x100;
}

/// Expected magic value: "virt" in little-endian.
pub const VIRTIO_MAGIC: u32 = 0x74726976;

/// Device status bits (spec §2.1).
pub const STATUS_ACKNOWLEDGE: u32 = 1;
pub const STATUS_DRIVER: u32 = 2;
pub const STATUS_DRIVER_OK: u32 = 4;
pub const STATUS_FEATURES_OK: u32 = 8;
pub const STATUS_FAILED: u32 = 128;

/// Device IDs.
pub const DEVICE_ID_BLOCK: u32 = 2;

/// QEMU virt: 32 virtio MMIO transports.
pub const NUM_TRANSPORTS: usize = 32;
/// Each transport is 0x200 bytes.
pub const TRANSPORT_SIZE: usize = 0x200;
/// First virtio transport physical address.
pub const VIRTIO_BASE_PHYS: usize = 0x0A00_0000;
/// IRQ base: SPI 16 = INTID 48.
pub const VIRTIO_IRQ_BASE: u32 = 48;

// ============================================================================
// MMIO register access
// ============================================================================

/// Read a 32-bit MMIO register.
#[inline(always)]
unsafe fn read_reg(base: usize, offset: usize) -> u32 {
    core::ptr::read_volatile((base + offset) as *const u32)
}

/// Write a 32-bit MMIO register.
#[inline(always)]
unsafe fn write_reg(base: usize, offset: usize, val: u32) {
    core::ptr::write_volatile((base + offset) as *mut u32, val);
}

// ============================================================================
// Virtqueue descriptor and ring structures (spec §2.7)
// ============================================================================

/// Virtqueue descriptor (16 bytes).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct VirtqDesc {
    /// Physical address of the buffer.
    pub addr: u64,
    /// Length of the buffer in bytes.
    pub len: u32,
    /// Descriptor flags (NEXT, WRITE, INDIRECT).
    pub flags: u16,
    /// Index of the next descriptor if NEXT flag is set.
    pub next: u16,
}

/// Descriptor flag: buffer continues via `next` field.
pub const VIRTQ_DESC_F_NEXT: u16 = 1;
/// Descriptor flag: buffer is device-writable (vs device-readable).
pub const VIRTQ_DESC_F_WRITE: u16 = 2;

/// Available ring header.
#[repr(C)]
pub struct VirtqAvail {
    pub flags: u16,
    pub idx: u16,
    // Followed by ring[queue_size] entries (u16 each).
}

/// Used ring header.
#[repr(C)]
pub struct VirtqUsed {
    pub flags: u16,
    pub idx: u16,
    // Followed by ring[queue_size] × VirtqUsedElem entries.
}

/// Used ring element (8 bytes).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct VirtqUsedElem {
    /// Index of the head descriptor of the used chain.
    pub id: u32,
    /// Total bytes written by the device.
    pub len: u32,
}

// ============================================================================
// Virtqueue management
// ============================================================================

/// Maximum queue size we support. Keeps memory usage bounded.
#[allow(dead_code)]
pub const MAX_QUEUE_SIZE: usize = 128;

/// A virtqueue with its descriptor table, available ring, and used ring
/// all laid out in a single contiguous buffer.
///
/// Memory layout (legacy, with guest page size alignment):
///   [descriptors: 16 × num] [avail ring: 6 + 2×num] [padding] [used ring: 6 + 8×num]
pub struct Virtqueue {
    /// Kernel virtual address of the descriptor table.
    pub desc: *mut VirtqDesc,
    /// Kernel virtual address of the available ring.
    pub avail: *mut VirtqAvail,
    /// Kernel virtual address of the used ring.
    pub used: *mut VirtqUsed,
    /// Queue size (number of descriptors).
    pub num: u16,
    /// Head of the free descriptor chain.
    pub free_head: u16,
    /// Number of free descriptors.
    pub num_free: u16,
    /// Last seen used ring index.
    pub last_used_idx: u16,
    /// Physical address of the buffer (for QueuePFN).
    pub phys_base: usize,
}

impl Virtqueue {
    /// Calculate the total byte size needed for a virtqueue with `num` entries.
    /// Legacy layout: descriptors + avail ring are contiguous, then page-aligned
    /// gap, then used ring.
    pub const fn size_bytes(num: usize, page_size: usize) -> usize {
        let desc_size = 16 * num;
        let avail_size = 6 + 2 * num;
        let used_size = 6 + 8 * num;
        // Legacy: used ring starts at page-aligned offset after desc+avail
        let first_part = desc_size + avail_size;
        let aligned = (first_part + page_size - 1) & !(page_size - 1);
        aligned + used_size
    }

    /// Initialize a virtqueue from a zeroed buffer at the given kernel VA and PA.
    ///
    /// # Safety
    /// `buf_va` must point to at least `size_bytes(num)` bytes of zeroed memory.
    /// `buf_pa` must be the corresponding physical address.
    pub unsafe fn init(buf_va: usize, buf_pa: usize, num: u16, guest_page_size: usize) -> Self {
        let n = num as usize;
        let desc = buf_va as *mut VirtqDesc;
        let avail = (buf_va + 16 * n) as *mut VirtqAvail;

        // Used ring starts at the next page-aligned offset (legacy layout)
        let first_part = 16 * n + 6 + 2 * n;
        let used_offset = (first_part + guest_page_size - 1) & !(guest_page_size - 1);
        let used = (buf_va + used_offset) as *mut VirtqUsed;

        // Build free descriptor chain: each points to the next.
        for i in 0..n {
            let d = desc.add(i);
            (*d).addr = 0;
            (*d).len = 0;
            (*d).flags = 0;
            (*d).next = if i + 1 < n { (i + 1) as u16 } else { 0 };
        }

        // Available ring starts empty (idx = 0).
        (*avail).flags = 0;
        (*avail).idx = 0;

        // Used ring starts empty.
        (*used).flags = 0;
        (*used).idx = 0;

        Virtqueue {
            desc,
            avail,
            used,
            num,
            free_head: 0,
            num_free: num,
            last_used_idx: 0,
            phys_base: buf_pa,
        }
    }

    /// Allocate a descriptor from the free list. Returns the index.
    pub fn alloc_desc(&mut self) -> Option<u16> {
        if self.num_free == 0 {
            return None;
        }
        let idx = self.free_head;
        unsafe {
            self.free_head = (*self.desc.add(idx as usize)).next;
        }
        self.num_free -= 1;
        Some(idx)
    }

    /// Free a descriptor back to the free list.
    pub fn free_desc(&mut self, idx: u16) {
        unsafe {
            let d = &mut *self.desc.add(idx as usize);
            d.addr = 0;
            d.len = 0;
            d.flags = 0;
            d.next = self.free_head;
        }
        self.free_head = idx;
        self.num_free += 1;
    }

    /// Free a chain of descriptors starting at `head`.
    pub fn free_chain(&mut self, head: u16) {
        let mut idx = head;
        loop {
            let d = unsafe { &*self.desc.add(idx as usize) };
            let has_next = d.flags & VIRTQ_DESC_F_NEXT != 0;
            let next = d.next;
            self.free_desc(idx);
            if has_next {
                idx = next;
            } else {
                break;
            }
        }
    }

    /// Push a descriptor chain head into the available ring.
    pub fn push_avail(&mut self, desc_idx: u16) {
        unsafe {
            let avail = &mut *self.avail;
            let ring_base = (self.avail as *mut u16).add(2); // skip flags + idx
            let ring_idx = avail.idx as usize % self.num as usize;
            core::ptr::write_volatile(ring_base.add(ring_idx), desc_idx);
            fence(Ordering::Release);
            core::ptr::write_volatile(&mut avail.idx, avail.idx.wrapping_add(1));
        }
    }

    /// Check if there are new entries in the used ring.
    /// Returns Some((desc_head_idx, bytes_written)) if available.
    pub fn pop_used(&mut self) -> Option<(u16, u32)> {
        fence(Ordering::Acquire);
        let used_idx = unsafe { core::ptr::read_volatile(&(*self.used).idx) };
        if self.last_used_idx == used_idx {
            return None;
        }
        let ring_base = unsafe { (self.used as *mut u8).add(4) as *mut VirtqUsedElem };
        let slot = self.last_used_idx as usize % self.num as usize;
        let elem = unsafe { core::ptr::read_volatile(ring_base.add(slot)) };
        self.last_used_idx = self.last_used_idx.wrapping_add(1);
        Some((elem.id as u16, elem.len))
    }
}

// ============================================================================
// Device discovery and initialization
// ============================================================================

/// Probe a single MMIO transport. Returns the device ID if a valid virtio device is present.
pub fn probe_transport(base_va: usize) -> Option<u32> {
    unsafe {
        let magic = read_reg(base_va, regs::MAGIC_VALUE);
        if magic != VIRTIO_MAGIC {
            return None;
        }
        let device_id = read_reg(base_va, regs::DEVICE_ID);
        if device_id == 0 {
            return None; // no device at this transport
        }
        Some(device_id)
    }
}

/// Reset a virtio device.
#[allow(dead_code)]
pub fn reset_device(base_va: usize) {
    unsafe {
        write_reg(base_va, regs::STATUS, 0);
    }
}

/// Perform the standard device initialization sequence (spec §3.1).
/// Returns the negotiated features, or None on failure.
///
/// Steps:
/// 1. Reset
/// 2. Set ACKNOWLEDGE
/// 3. Set DRIVER
/// 4. Read and negotiate features
/// 5. Set guest page size (legacy)
/// 6. Set FEATURES_OK
/// 7. Verify FEATURES_OK stuck
pub fn init_device(base_va: usize, desired_features: u32) -> Option<u32> {
    unsafe {
        // 1. Reset
        write_reg(base_va, regs::STATUS, 0);

        // 2. Acknowledge
        let mut status = STATUS_ACKNOWLEDGE;
        write_reg(base_va, regs::STATUS, status);

        // 3. Driver
        status |= STATUS_DRIVER;
        write_reg(base_va, regs::STATUS, status);

        // 4. Feature negotiation (word 0 only — legacy)
        write_reg(base_va, regs::HOST_FEATURES_SEL, 0);
        let host_features = read_reg(base_va, regs::HOST_FEATURES);
        let negotiated = host_features & desired_features;
        write_reg(base_va, regs::GUEST_FEATURES_SEL, 0);
        write_reg(base_va, regs::GUEST_FEATURES, negotiated);

        // 5. Set guest page size (legacy only)
        write_reg(base_va, regs::GUEST_PAGE_SIZE, beetos::PAGE_SIZE as u32);

        // 6. Features OK
        status |= STATUS_FEATURES_OK;
        write_reg(base_va, regs::STATUS, status);

        // 7. Verify
        let readback = read_reg(base_va, regs::STATUS);
        if readback & STATUS_FEATURES_OK == 0 {
            write_reg(base_va, regs::STATUS, STATUS_FAILED);
            return None;
        }

        Some(negotiated)
    }
}

/// Mark the device as ready (set DRIVER_OK).
pub fn driver_ok(base_va: usize) {
    unsafe {
        let status = read_reg(base_va, regs::STATUS);
        write_reg(base_va, regs::STATUS, status | STATUS_DRIVER_OK);
    }
}

/// Configure a virtqueue on the device.
///
/// `queue_idx`: which queue to configure (0 for block device requestq).
/// `queue`: the initialized Virtqueue (provides PFN and size).
/// `guest_page_size`: the page size set via GUEST_PAGE_SIZE.
pub fn setup_queue(base_va: usize, queue_idx: u32, queue: &Virtqueue, guest_page_size: usize) {
    unsafe {
        write_reg(base_va, regs::QUEUE_SEL, queue_idx);
        let max = read_reg(base_va, regs::QUEUE_NUM_MAX);
        if max == 0 {
            return; // queue not available
        }
        let num = core::cmp::min(queue.num as u32, max);
        write_reg(base_va, regs::QUEUE_NUM, num);
        write_reg(base_va, regs::QUEUE_ALIGN, guest_page_size as u32);
        // Legacy: QueuePFN = physical address / guest page size
        let pfn = (queue.phys_base / guest_page_size) as u32;
        write_reg(base_va, regs::QUEUE_PFN, pfn);
    }
}

/// Notify the device that new buffers are available in a queue.
#[inline(always)]
pub fn notify(base_va: usize, queue_idx: u32) {
    fence(Ordering::Release);
    unsafe {
        write_reg(base_va, regs::QUEUE_NOTIFY, queue_idx);
    }
}

/// Read and acknowledge the interrupt status register.
/// Returns the status bits (bit 0 = used buffer notification, bit 1 = config change).
pub fn ack_interrupt(base_va: usize) -> u32 {
    unsafe {
        let status = read_reg(base_va, regs::INTERRUPT_STATUS);
        if status != 0 {
            write_reg(base_va, regs::INTERRUPT_ACK, status);
        }
        status
    }
}

/// Read a device-specific config register (32-bit) at `offset` from config base.
pub fn read_config_u32(base_va: usize, offset: usize) -> u32 {
    unsafe { read_reg(base_va, regs::CONFIG + offset) }
}

/// Read a device-specific config register (64-bit) at `offset` from config base.
pub fn read_config_u64(base_va: usize, offset: usize) -> u64 {
    unsafe {
        let lo = read_reg(base_va, regs::CONFIG + offset) as u64;
        let hi = read_reg(base_va, regs::CONFIG + offset + 4) as u64;
        lo | (hi << 32)
    }
}
