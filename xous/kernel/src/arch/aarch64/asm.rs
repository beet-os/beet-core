// SPDX-FileCopyrightText: 2024 BeetOS contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! AArch64 assembly linkage — exception vectors, context save/restore.

use core::arch::global_asm;

// Include the assembly source for exception vectors and context switching.
global_asm!(include_str!("asm.S"));

/// Flush a single TLB entry by virtual address (all ASIDs).
#[inline]
pub fn flush_tlb_entry(vaddr: usize) {
    unsafe {
        core::arch::asm!(
            "dsb ishst",
            "tlbi vaale1is, {addr}",
            "dsb ish",
            "isb",
            addr = in(reg) vaddr >> 12, // TLBI takes the address shifted right by 12
            options(nomem, nostack),
        );
    }
}

/// Flush all TLB entries for a given ASID.
#[inline]
pub fn flush_tlb_asid(asid: u16) {
    unsafe {
        core::arch::asm!(
            "dsb ishst",
            "tlbi aside1is, {asid}",
            "dsb ish",
            "isb",
            asid = in(reg) (asid as u64) << 48,
            options(nomem, nostack),
        );
    }
}

/// Flush the entire TLB.
#[inline]
pub fn flush_tlb_all() {
    unsafe {
        core::arch::asm!(
            "dsb ishst",
            "tlbi vmalle1is",
            "dsb ish",
            "isb",
            options(nomem, nostack),
        );
    }
}

/// Data Synchronization Barrier.
#[inline]
pub fn dsb() {
    unsafe { core::arch::asm!("dsb sy", options(nomem, nostack)) };
}

/// Instruction Synchronization Barrier.
#[inline]
pub fn isb() {
    unsafe { core::arch::asm!("isb", options(nomem, nostack)) };
}

/// Clean and invalidate data cache by virtual address to Point of Coherency.
#[inline]
pub fn dc_civac(addr: usize) {
    unsafe {
        core::arch::asm!(
            "dc civac, {addr}",
            addr = in(reg) addr,
            options(nomem, nostack),
        );
    }
}

/// Clean data cache by virtual address to Point of Coherency.
#[inline]
pub fn dc_cvac(addr: usize) {
    unsafe {
        core::arch::asm!(
            "dc cvac, {addr}",
            addr = in(reg) addr,
            options(nomem, nostack),
        );
    }
}

/// Invalidate data cache by virtual address to Point of Coherency.
#[inline]
pub fn dc_ivac(addr: usize) {
    unsafe {
        core::arch::asm!(
            "dc ivac, {addr}",
            addr = in(reg) addr,
            options(nomem, nostack),
        );
    }
}

extern "C" {
    /// Resume execution of a thread. Defined in asm.S.
    /// Takes a pointer to the saved thread context.
    pub fn _resume_context(context: *const u8) -> !;

    /// The exception vector table base address. Defined in asm.S.
    pub static _exception_vectors: u8;
}
