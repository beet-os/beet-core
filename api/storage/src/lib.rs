// SPDX-FileCopyrightText: 2024 BeetOS contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! BeetOS block storage API.
//!
//! Platform-agnostic traits for block devices (virtio-blk on QEMU,
//! SD card on RPi5, NVMe on Apple M1). Drivers implement `BlockDevice`;
//! the filesystem service depends only on this crate, not on any
//! platform-specific driver.

#![no_std]

/// Size of a single block in bytes. All I/O is aligned to this boundary.
pub const BLOCK_SIZE: usize = 512;

/// Errors returned by block device operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockError {
    /// Hardware or transport error during the operation.
    IoError,
    /// Requested LBA is beyond the end of the device.
    OutOfRange,
    /// Device is not yet initialized or not present.
    NotReady,
}

/// Platform-agnostic block device interface.
///
/// Implementors: `VirtioBlk` (QEMU), SD card driver (RPi5), NVMe (Apple M1).
///
/// All operations are synchronous (polling). IRQ-driven I/O is a future
/// optimization and will not change this interface.
///
/// # Buffer requirements
///
/// `buf` must be a multiple of `BLOCK_SIZE` bytes. The number of sectors
/// transferred equals `buf.len() / BLOCK_SIZE`.
pub trait BlockDevice {
    /// Read sectors starting at `lba` into `buf`.
    ///
    /// `buf.len()` must be a non-zero multiple of `BLOCK_SIZE`.
    fn read_sectors(&self, lba: u64, buf: &mut [u8]) -> Result<(), BlockError>;

    /// Write sectors from `buf` starting at `lba`.
    ///
    /// `buf.len()` must be a non-zero multiple of `BLOCK_SIZE`.
    fn write_sectors(&self, lba: u64, buf: &[u8]) -> Result<(), BlockError>;

    /// Total device capacity in 512-byte sectors.
    fn capacity_sectors(&self) -> u64;

    /// Total device capacity in bytes.
    fn capacity_bytes(&self) -> u64 {
        self.capacity_sectors() * BLOCK_SIZE as u64
    }
}
