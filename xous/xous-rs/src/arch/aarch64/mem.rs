// SPDX-FileCopyrightText: 2024 BeetOS contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! AArch64 userspace memory allocation helpers.
//!
//! On hardware, memory allocation goes through Xous syscalls (MapMemory).
//! These pre/post functions perform any architecture-specific setup needed
//! before and after the syscall.

use crate::{Error, MemoryAddress, MemoryFlags, MemoryRange};

/// Pre-syscall validation for memory mapping.
pub fn map_memory_pre(
    _phys: &Option<MemoryAddress>,
    _virt: &Option<MemoryAddress>,
    _size: usize,
    _flags: MemoryFlags,
) -> core::result::Result<(), Error> {
    // No pre-processing needed on AArch64 — the kernel handles everything
    Ok(())
}

/// Post-syscall processing for memory mapping.
/// The kernel returns the mapped range; we just pass it through.
pub fn map_memory_post(
    _phys: Option<MemoryAddress>,
    _virt: Option<MemoryAddress>,
    _size: usize,
    _flags: MemoryFlags,
    range: MemoryRange,
) -> core::result::Result<MemoryRange, Error> {
    Ok(range)
}

/// Pre-syscall validation for memory unmapping.
pub fn unmap_memory_pre(_range: &MemoryRange) -> core::result::Result<(), Error> {
    Ok(())
}

/// Post-syscall processing for memory unmapping.
pub fn unmap_memory_post(_range: MemoryRange) -> core::result::Result<(), Error> {
    Ok(())
}
