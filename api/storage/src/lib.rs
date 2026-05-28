// SPDX-FileCopyrightText: 2024 BeetOS contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! BeetOS block storage API.
//!
//! Platform-agnostic traits for block devices. Drivers implement
//! [`BlockDevice`]; the filesystem service depends only on this
//! crate, not on any platform-specific driver.
//!
//! Implementations today live in `xous/kernel/src/block.rs`:
//!
//!   - `SdBlockDevice<'_, M>` — wraps an SDHCI host (RPi5 SD slot)
//!   - `NvmeBlockDevice<T>`   — wraps an NVMe transport (Apple ANS,
//!                              future PCIe NVMe)
//!   - `MemBlockDevice`       — hosted-mode Vec<u8>-backed
//!   - `FileBlockDevice`      — hosted-mode std::fs::File-backed
//!     (the `cargo run` dev loop reads a `target/sd.img` through this)

#![no_std]

/// Legacy hint — every "SDHC-class" card uses this. Modern NVMe
/// drives use 4 KB. Call [`BlockDevice::block_size`] for the
/// real per-device value rather than relying on this constant.
pub const SECTOR_SIZE: usize = 512;

/// Errors returned by block device operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockError {
    /// `lba + n_blocks` would extend past `capacity_blocks()`.
    OutOfRange,
    /// Buffer length is not an exact non-zero multiple of `block_size()`.
    BadBuffer,
    /// Underlying hardware / transport timed out.
    Timeout,
    /// Hardware reported an error status (CRC, data, controller-specific).
    Io,
    /// Device is not initialised yet.
    NotReady,
    /// Anything else we don't yet model.
    Other,
}

/// Platform-agnostic block device interface.
///
/// All methods take `&mut self` because real drivers serialise on
/// per-device state (controller command queues, mailbox channels,
/// MMIO registers). Backends that genuinely want shared access can
/// wrap themselves in `Mutex` / `RefCell` at the call site.
///
/// # Buffer requirements
///
/// Every read / write buffer must be a non-zero multiple of
/// [`BlockDevice::block_size`]. The number of blocks transferred is
/// `buf.len() / block_size()`.
pub trait BlockDevice {
    /// Bytes per logical block. Almost always 512 on SDHC and 4096
    /// on modern NVMe; the FS layer treats this as the device's
    /// native I/O granularity.
    fn block_size(&self) -> u32;

    /// Number of logical blocks the device exposes.
    fn capacity_blocks(&self) -> u64;

    /// Convenience: `block_size() * capacity_blocks()`.
    fn capacity_bytes(&self) -> u64 {
        self.block_size() as u64 * self.capacity_blocks()
    }

    /// Read `buf.len() / block_size()` blocks starting at `lba` into `buf`.
    fn read_blocks(&mut self, lba: u64, buf: &mut [u8]) -> Result<(), BlockError>;

    /// Write `buf.len() / block_size()` blocks starting at `lba` from `buf`.
    fn write_blocks(&mut self, lba: u64, buf: &[u8]) -> Result<(), BlockError>;
}
