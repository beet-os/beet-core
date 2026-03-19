// SPDX-FileCopyrightText: 2024 BeetOS contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! AArch64 assembly linkage — exception vectors, context save/restore.

// Assembly source (asm.S, start.S) is included via global_asm! in mod.rs.

/// Flush a single TLB entry by virtual address for the current ASID only.
///
/// Uses `vale1is` (VA, Last-level, EL1, Inner Shareable) which invalidates
/// TLB entries matching both the VA and the current ASID. This avoids
/// invalidating TLB entries of other processes that map the same VA range
/// (e.g., user stacks all map to the same virtual address range but with
/// different ASIDs and different physical pages).
///
/// Using the all-ASID variant (`vaale1is`) would invalidate all processes'
/// TLB entries for this VA. On QEMU, this triggers a softmmu coherence
/// issue: writes through L2 block descriptors (identity map) become visible
/// instead of the correct data written through L3 page descriptors.
#[inline]
pub fn flush_tlb_entry(vaddr: usize) {
    // Read the current ASID from TTBR0_EL1 bits [63:48].
    let asid: u64;
    unsafe {
        core::arch::asm!("mrs {}, ttbr0_el1", out(reg) asid, options(nomem, nostack));
    }
    let asid_bits = asid & (0xFFFF_u64 << 48); // Keep only ASID field
    let addr_bits = (vaddr >> 12) as u64;       // VA shifted right by 12
    let tlbi_arg = asid_bits | addr_bits;       // ASID in [63:48], VA in [43:0]
    unsafe {
        core::arch::asm!(
            "dsb ishst",
            "tlbi vae1is, {arg}",
            "dsb ish",
            "isb",
            arg = in(reg) tlbi_arg,
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
