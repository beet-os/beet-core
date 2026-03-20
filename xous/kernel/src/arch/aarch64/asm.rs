// SPDX-FileCopyrightText: 2024 BeetOS contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! AArch64 assembly linkage — TLB maintenance, cache operations, barriers,
//! and external symbols from `asm.S` / `start.S`.
//!
//! # TLB Invalidation
//!
//! After modifying any page table entry (PTE), the old translation may still
//! be cached in the TLB. The correct sequence is:
//!
//! 1. Modify the PTE (via `write_volatile`)
//! 2. `DSB ISHST` — ensure the PTE store is visible to the hardware walker
//! 3. `TLBI ...` — invalidate the stale TLB entry
//! 4. `DSB ISH` — wait for invalidation to complete on all cores
//! 5. `ISB` — synchronize the instruction stream (flushes pipeline)
//!
//! All TLBI variants used here are **Inner-Shareable (IS)** — they broadcast
//! to all cores in the inner shareability domain, which is required for SMP.
//!
//! # Cache Operations
//!
//! AArch64 caches are PIPT (Physically Indexed, Physically Tagged) but cache
//! maintenance instructions operate on virtual addresses. The three operations:
//!
//! - **DC CVAC** (Clean): write dirty cache line back to memory
//! - **DC CIVAC** (Clean + Invalidate): write back and discard
//! - **DC IVAC** (Invalidate): discard without write-back (use with care)
//!
//! Apple M1 uses 64-byte cache lines. Cache operations are aligned to cache
//! line boundaries in [`super::mem::MemoryMapping::flush_cache`].

// Assembly source (asm.S, start.S) is included via global_asm! in mod.rs.

/// Invalidate a single TLB entry by virtual address, across **all ASIDs**.
///
/// Uses `TLBI VAALE1IS` (VA, All ASIDs, EL1, Inner Shareable).
/// The virtual address is shifted right by 12 bits as required by the TLBI
/// instruction encoding (not by PAGE_SHIFT — this is a fixed ISA encoding).
///
/// Called after every [`map_page`](super::mem::MemoryMapping::map_page) and
/// [`unmap_page`](super::mem::MemoryMapping::unmap_page).
#[inline]
pub fn flush_tlb_entry(vaddr: usize) {
    unsafe {
        core::arch::asm!(
            "dsb ishst",            // Ensure PTE write is visible
            "tlbi vaale1is, {addr}",// Invalidate VA, all ASIDs, EL1, IS
            "dsb ish",              // Wait for invalidation to complete
            "isb",                  // Synchronize instruction stream
            addr = in(reg) vaddr >> 12, // TLBI encoding: VA[47:12]
            options(nomem, nostack),
        );
    }
}

/// Invalidate all TLB entries for a given ASID (process).
///
/// Uses `TLBI ASIDE1IS` (ASID, EL1, Inner Shareable).
/// The ASID is placed in bits [63:48] of the operand register.
///
/// Called when destroying a process's address space (see
/// [`MemoryMapping::destroy`](super::mem::MemoryMapping::destroy)).
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

/// Invalidate the **entire** TLB (all ASIDs, all VAs).
///
/// Uses `TLBI VMALLE1IS` (VM All, EL1, Inner Shareable).
/// Used during boot and for bulk page table changes. Expensive on SMP —
/// prefer [`flush_tlb_entry`] or [`flush_tlb_asid`] when possible.
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

/// Data Synchronization Barrier (full system).
///
/// Ensures all preceding memory accesses (loads, stores, cache maintenance)
/// have completed before any subsequent instruction executes. Used after
/// cache maintenance sequences.
#[inline]
pub fn dsb() {
    unsafe { core::arch::asm!("dsb sy", options(nomem, nostack)) };
}

/// Instruction Synchronization Barrier.
///
/// Flushes the CPU pipeline and refetches all subsequent instructions.
/// Required after changes to system registers (TTBR, SCTLR, VBAR, etc.)
/// to ensure the new settings take effect.
#[inline]
pub fn isb() {
    unsafe { core::arch::asm!("isb", options(nomem, nostack)) };
}

/// Clean and invalidate data cache by VA to Point of Coherency.
///
/// Writes the dirty cache line back to memory, then marks it invalid.
/// Use when transferring ownership of a buffer (e.g., to a DMA device).
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

/// Clean data cache by VA to Point of Coherency.
///
/// Writes the dirty cache line back to memory but keeps the line valid.
/// Use when the CPU needs to continue accessing the data but an external
/// observer (another core or device) must see the latest value.
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

/// Invalidate data cache by VA to Point of Coherency.
///
/// Discards the cache line **without** writing back dirty data. Only safe
/// when the cache line is known to be clean or when the stale data is
/// intentionally being discarded (e.g., before a DMA read).
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
    /// Resume execution of a saved thread context, defined in `asm.S`.
    ///
    /// Restores all general-purpose registers (X0-X30), SP_EL0, ELR_EL1,
    /// SPSR_EL1, FPCR, FPSR, and NEON registers (V0-V31) from the 816-byte
    /// context frame, then executes `ERET` to return to the thread's PC.
    ///
    /// This function never returns — control transfers to the saved ELR_EL1.
    pub fn _resume_context(context: *const u8) -> !;

    /// Base address of the exception vector table, defined in `asm.S`.
    ///
    /// Written to `VBAR_EL1` during boot. The table contains 16 entries
    /// (4 groups × 4 exception types) aligned to 2KB as required by ARMv8-A.
    pub static _exception_vectors: u8;
}
