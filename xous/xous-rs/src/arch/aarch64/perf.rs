// SPDX-FileCopyrightText: 2025 BeetOS contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! EL0 access to the ARM Generic Timer's virtual counter.
//!
//! The kernel enables CNTKCTL_EL1.EL0VCTEN at boot (see the platform
//! timer init), so userspace can read CNTVCT_EL0/CNTFRQ_EL0 directly —
//! no syscall, no kernel round-trip in the middle of a measurement.
//!
//! Under QEMU `-icount`, the virtual counter advances with the
//! *instruction count* rather than wall time, which makes benchmark
//! numbers deterministic across runs and host machines.

/// Read the virtual counter. The ISB keeps earlier instructions from
/// drifting past the read (the counter read is not self-serialising).
#[inline]
pub fn counter() -> u64 {
    let cnt: u64;
    unsafe {
        core::arch::asm!(
            "isb",
            "mrs {}, cntvct_el0",
            out(reg) cnt,
            options(nomem, nostack),
        );
    }
    cnt
}

/// Counter frequency in Hz (CNTFRQ_EL0 — 62.5 MHz on QEMU virt,
/// 24 MHz on Apple Silicon).
#[inline]
pub fn frequency() -> u64 {
    let freq: u64;
    unsafe {
        core::arch::asm!("mrs {}, cntfrq_el0", out(reg) freq, options(nomem, nostack));
    }
    freq
}
