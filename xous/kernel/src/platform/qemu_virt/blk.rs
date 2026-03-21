// SPDX-FileCopyrightText: 2024 BeetOS contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! virtio-blk block device driver for QEMU virt.
//!
//! Implements synchronous (polling) block I/O on top of the virtio MMIO
//! transport. The driver scans all 32 MMIO transports for a block device
//! (DeviceID == 2), initializes it, and provides `read_sectors()`.
//!
//! Reference: virtio spec v1.2, section 5.2 (Block Device).

use super::virtio::{self, Virtqueue, VIRTQ_DESC_F_NEXT, VIRTQ_DESC_F_WRITE};
use core::sync::atomic::{fence, Ordering};

/// Block sector size (always 512 bytes for virtio-blk).
pub const SECTOR_SIZE: usize = 512;

/// virtio-blk request types.
const VIRTIO_BLK_T_IN: u32 = 0;   // read
#[allow(dead_code)]
const VIRTIO_BLK_T_OUT: u32 = 1;  // write

/// virtio-blk status codes.
const VIRTIO_BLK_S_OK: u8 = 0;
const VIRTIO_BLK_S_IOERR: u8 = 1;

/// virtio-blk request header (spec §5.2.6).
#[repr(C)]
struct VirtioBlkReqHeader {
    type_: u32,
    reserved: u32,
    sector: u64,
}

/// Block device state.
struct BlkDevice {
    /// MMIO transport kernel VA.
    base_va: usize,
    /// Device capacity in 512-byte sectors.
    capacity: u64,
    /// The requestq (queue 0).
    queue: Virtqueue,
    /// GIC IRQ number for this transport.
    irq: u32,
}

/// Global block device state. None if no block device found.
static mut BLK_DEV: Option<BlkDevice> = None;

// Static buffers for the virtqueue and request data.
// These live in kernel BSS, so their PA = VA - KERNEL_VA_OFFSET.
// We use a small queue (16 entries) — one page is more than enough.

/// Queue size: 16 entries is plenty for synchronous I/O.
const QUEUE_SIZE: u16 = 16;

/// Virtqueue buffer: descriptors + available ring + padding + used ring.
/// Aligned to page boundary for legacy QueuePFN.
#[repr(C, align(16384))]
struct VirtqueueBuffer {
    data: [u8; virtio::Virtqueue::size_bytes(QUEUE_SIZE as usize, beetos::PAGE_SIZE)],
}

static mut VQUEUE_BUF: VirtqueueBuffer = VirtqueueBuffer {
    data: [0u8; virtio::Virtqueue::size_bytes(QUEUE_SIZE as usize, beetos::PAGE_SIZE)],
};

/// Request header buffer (used for the current in-flight request).
static mut REQ_HEADER: VirtioBlkReqHeader = VirtioBlkReqHeader {
    type_: 0,
    reserved: 0,
    sector: 0,
};

/// Status byte buffer (written by device after request completes).
static mut REQ_STATUS: u8 = 0xFF;

// ============================================================================
// Public API
// ============================================================================

/// Probe all virtio MMIO transports and initialize the first block device found.
/// Called during platform init.
pub fn probe_and_init(virtio_base_va: usize) {
    for i in 0..virtio::NUM_TRANSPORTS {
        let base = virtio_base_va + i * virtio::TRANSPORT_SIZE;
        if let Some(device_id) = virtio::probe_transport(base) {
            if device_id == virtio::DEVICE_ID_BLOCK {
                if init_block_device(base, i) {
                    return;
                }
            }
        }
    }
    // No block device found — that's OK (no -drive flag passed to QEMU)
}

/// Returns the block device capacity in 512-byte sectors, or 0 if no device.
pub fn capacity() -> u64 {
    unsafe {
        match (*(&raw const BLK_DEV)).as_ref() {
            Some(dev) => dev.capacity,
            None => 0,
        }
    }
}

/// Returns true if a block device is available.
pub fn is_available() -> bool {
    unsafe { (*(&raw const BLK_DEV)).is_some() }
}

/// Read sectors from the block device.
///
/// `lba`: starting sector number (512-byte sectors).
/// `buf`: destination buffer, must be `count * 512` bytes.
///
/// Returns Ok(()) on success, Err(BlkError) on failure.
pub fn read_sectors(lba: u64, buf: &mut [u8]) -> Result<(), BlkError> {
    let count = buf.len() / SECTOR_SIZE;
    if buf.len() % SECTOR_SIZE != 0 || count == 0 {
        return Err(BlkError::InvalidSize);
    }

    let dev = unsafe {
        match (*(&raw mut BLK_DEV)).as_mut() {
            Some(d) => d,
            None => return Err(BlkError::NoDevice),
        }
    };

    if lba + count as u64 > dev.capacity {
        return Err(BlkError::OutOfRange);
    }

    // Submit one request per call (synchronous, polling).
    // For simplicity, we read all sectors in a single virtio request.
    // The virtio spec allows data buffers up to any size.
    unsafe {
        do_block_request(dev, VIRTIO_BLK_T_IN, lba, buf.as_mut_ptr(), buf.len())
    }
}

/// Write sectors to the block device.
#[allow(dead_code)]
pub fn write_sectors(lba: u64, buf: &[u8]) -> Result<(), BlkError> {
    let count = buf.len() / SECTOR_SIZE;
    if buf.len() % SECTOR_SIZE != 0 || count == 0 {
        return Err(BlkError::InvalidSize);
    }

    let dev = unsafe {
        match (*(&raw mut BLK_DEV)).as_mut() {
            Some(d) => d,
            None => return Err(BlkError::NoDevice),
        }
    };

    if lba + count as u64 > dev.capacity {
        return Err(BlkError::OutOfRange);
    }

    unsafe {
        do_block_request(dev, VIRTIO_BLK_T_OUT, lba, buf.as_ptr() as *mut u8, buf.len())
    }
}

/// Block I/O error types.
#[derive(Debug)]
pub enum BlkError {
    NoDevice,
    InvalidSize,
    OutOfRange,
    IoError,
    DeviceFailed,
}

/// Zero-sized handle to the QEMU virtio-blk device.
///
/// Implements `beetos_api_storage::BlockDevice` so the filesystem service
/// and any other consumer can depend on the platform-agnostic trait rather
/// than calling `blk::read_sectors()` / `blk::write_sectors()` directly.
#[allow(dead_code)]
pub struct VirtioBlk;

impl beetos_api_storage::BlockDevice for VirtioBlk {
    fn read_sectors(&self, lba: u64, buf: &mut [u8]) -> Result<(), beetos_api_storage::BlockError> {
        read_sectors(lba, buf).map_err(|e| match e {
            BlkError::OutOfRange => beetos_api_storage::BlockError::OutOfRange,
            BlkError::NoDevice   => beetos_api_storage::BlockError::NotReady,
            _                    => beetos_api_storage::BlockError::IoError,
        })
    }

    fn write_sectors(&self, lba: u64, buf: &[u8]) -> Result<(), beetos_api_storage::BlockError> {
        write_sectors(lba, buf).map_err(|e| match e {
            BlkError::OutOfRange => beetos_api_storage::BlockError::OutOfRange,
            BlkError::NoDevice   => beetos_api_storage::BlockError::NotReady,
            _                    => beetos_api_storage::BlockError::IoError,
        })
    }

    fn capacity_sectors(&self) -> u64 {
        capacity()
    }
}

/// Handle a virtio-blk interrupt. Called from the IRQ handler.
pub fn handle_irq() {
    unsafe {
        if let Some(dev) = (*(&raw const BLK_DEV)).as_ref() {
            virtio::ack_interrupt(dev.base_va);
        }
    }
}

/// Return the IRQ number for the block device, if present.
pub fn irq_number() -> Option<u32> {
    unsafe { (*(&raw const BLK_DEV)).as_ref().map(|d| d.irq) }
}

// ============================================================================
// Internal implementation
// ============================================================================

fn init_block_device(base_va: usize, transport_idx: usize) -> bool {
    // No features required for basic read/write.
    let features = match virtio::init_device(base_va, 0) {
        Some(f) => f,
        None => return false,
    };
    let _ = features;

    // Read capacity from config space (offset 0x00, 8 bytes).
    let capacity = virtio::read_config_u64(base_va, 0);
    if capacity == 0 {
        return false;
    }

    // Initialize the virtqueue buffer.
    let buf_va = unsafe { (*(&raw mut VQUEUE_BUF)).data.as_mut_ptr() as usize };
    let buf_pa = beetos::virt_to_phys(buf_va);

    // Zero the buffer.
    unsafe {
        core::ptr::write_bytes(
            buf_va as *mut u8,
            0,
            virtio::Virtqueue::size_bytes(QUEUE_SIZE as usize, beetos::PAGE_SIZE),
        );
    }

    let queue = unsafe {
        Virtqueue::init(buf_va, buf_pa, QUEUE_SIZE, beetos::PAGE_SIZE)
    };

    // Configure queue 0 (requestq) on the device.
    virtio::setup_queue(base_va, 0, &queue, beetos::PAGE_SIZE);

    // Mark device ready.
    virtio::driver_ok(base_va);

    let irq = virtio::VIRTIO_IRQ_BASE + transport_idx as u32;

    // Enable the IRQ in the GIC.
    super::gic::enable_irq(irq);

    use core::fmt::Write;
    let _ = write!(
        super::uart::UartWriter,
        "virtio-blk: {} sectors ({} MiB), IRQ {}\n",
        capacity,
        capacity * SECTOR_SIZE as u64 / (1024 * 1024),
        irq,
    );

    unsafe {
        BLK_DEV = Some(BlkDevice {
            base_va,
            capacity,
            queue,
            irq,
        });
    }

    true
}

/// Submit a block request and poll for completion.
///
/// # Safety
/// `data_ptr` must point to `data_len` bytes of valid memory.
/// For reads, the memory must be writable. For writes, it must be readable.
unsafe fn do_block_request(
    dev: &mut BlkDevice,
    req_type: u32,
    sector: u64,
    data_ptr: *mut u8,
    data_len: usize,
) -> Result<(), BlkError> {
    let q = &mut dev.queue;

    // Allocate 3 descriptors: header, data, status.
    let d0 = q.alloc_desc().ok_or(BlkError::DeviceFailed)?;
    let d1 = match q.alloc_desc() {
        Some(d) => d,
        None => { q.free_desc(d0); return Err(BlkError::DeviceFailed); }
    };
    let d2 = match q.alloc_desc() {
        Some(d) => d,
        None => { q.free_desc(d1); q.free_desc(d0); return Err(BlkError::DeviceFailed); }
    };

    // Set up request header.
    REQ_HEADER.type_ = req_type;
    REQ_HEADER.reserved = 0;
    REQ_HEADER.sector = sector;
    REQ_STATUS = 0xFF;

    let header_pa = beetos::virt_to_phys(&raw const REQ_HEADER as usize);
    let data_pa = beetos::virt_to_phys(data_ptr as usize);
    let status_pa = beetos::virt_to_phys(&raw const REQ_STATUS as usize);

    // Descriptor 0: request header (device-readable).
    let desc0 = &mut *q.desc.add(d0 as usize);
    desc0.addr = header_pa as u64;
    desc0.len = core::mem::size_of::<VirtioBlkReqHeader>() as u32;
    desc0.flags = VIRTQ_DESC_F_NEXT;
    desc0.next = d1;

    // Descriptor 1: data buffer.
    let desc1 = &mut *q.desc.add(d1 as usize);
    desc1.addr = data_pa as u64;
    desc1.len = data_len as u32;
    desc1.flags = VIRTQ_DESC_F_NEXT;
    if req_type == VIRTIO_BLK_T_IN {
        desc1.flags |= VIRTQ_DESC_F_WRITE; // device writes to this buffer
    }
    desc1.next = d2;

    // Descriptor 2: status byte (device-writable).
    let desc2 = &mut *q.desc.add(d2 as usize);
    desc2.addr = status_pa as u64;
    desc2.len = 1;
    desc2.flags = VIRTQ_DESC_F_WRITE;
    desc2.next = 0;

    // Push to available ring and notify.
    fence(Ordering::Release);
    q.push_avail(d0);
    virtio::notify(dev.base_va, 0);

    // Poll for completion.
    let mut spins: u32 = 0;
    loop {
        if let Some((head, _len)) = q.pop_used() {
            // Free the descriptor chain.
            q.free_chain(head);
            break;
        }
        spins += 1;
        if spins > 10_000_000 {
            return Err(BlkError::DeviceFailed);
        }
        core::hint::spin_loop();
    }

    // Check status.
    match REQ_STATUS {
        VIRTIO_BLK_S_OK => Ok(()),
        VIRTIO_BLK_S_IOERR => Err(BlkError::IoError),
        _ => Err(BlkError::DeviceFailed),
    }
}
