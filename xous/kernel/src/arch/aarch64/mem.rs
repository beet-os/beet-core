// SPDX-FileCopyrightText: 2024 BeetOS contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! AArch64 MMU and page table management for the Xous kernel.
//!
//! Uses ARMv8.0 translation tables with 16KB granule (Apple Silicon).
//!
//! # Page Table Structure (16KB granule, 47-bit VA)
//!
//! With TCR_EL1.TG0 = 16KB and T0SZ/T1SZ = 17 (47-bit VA per half):
//!   - L1: 2048 entries, each covering 64 GiB (bits [46:36])
//!   - L2: 2048 entries, each covering 32 MiB (bits [35:25])
//!   - L3: 2048 entries, each covering 16 KiB (bits [24:14])
//!
//! TTBR0_EL1 holds the user page table root (L1).
//! TTBR1_EL1 holds the kernel page table root (L1).

use core::arch::asm;

use xous::{CacheOperation, Error, MemoryFlags, PID};

use crate::mem::MemoryManager;

pub use beetos::PAGE_SIZE;

/// MMIO addresses shared with all user processes (mapped into every address space).
/// Populated at boot from FDT. Empty until M2 platform init.
pub static SHARED_PERIPHERALS: &[usize] = &[];

// Page table constants for 16KB granule
const PAGE_SHIFT: usize = 14; // log2(16384)
const TABLE_ENTRIES: usize = 2048; // 16KB / 8 bytes per entry
const TABLE_INDEX_BITS: usize = 11; // log2(2048)

// Entry type bits [1:0]
const PTE_VALID: u64 = 1 << 0;
const PTE_TABLE: u64 = 1 << 1; // For L1/L2: next-level table. For L3: page descriptor.

// Lower attributes
const PTE_ATTR_IDX_SHIFT: u64 = 2;
#[allow(dead_code)]
const PTE_ATTR_DEVICE: u64 = 0 << PTE_ATTR_IDX_SHIFT; // MAIR index 0: Device-nGnRnE
const PTE_ATTR_NORMAL: u64 = 1 << PTE_ATTR_IDX_SHIFT; // MAIR index 1: Normal WB cacheable
#[allow(dead_code)]
const PTE_ATTR_NORMAL_NC: u64 = 2 << PTE_ATTR_IDX_SHIFT; // MAIR index 2: Normal non-cacheable

const PTE_AP_RW_EL1: u64 = 0b00 << 6; // Read/write at EL1 only
const PTE_AP_RW_ALL: u64 = 0b01 << 6; // Read/write at EL0 and EL1
const PTE_AP_RO_EL1: u64 = 0b10 << 6; // Read-only at EL1 only
const PTE_AP_RO_ALL: u64 = 0b11 << 6; // Read-only at EL0 and EL1

const PTE_SH_ISH: u64 = 0b11 << 8; // Inner-shareable (for SMP)
const PTE_AF: u64 = 1 << 10; // Access flag (must be set to avoid access flag faults)
const PTE_NG: u64 = 1 << 11; // Non-global (per-ASID)

// Upper attributes
const PTE_PXN: u64 = 1 << 53; // Privileged execute-never
const PTE_UXN: u64 = 1 << 54; // User execute-never (EL0)

/// Address mask for output address in page table entries (16KB granule).
/// Bits [47:14] hold the physical address.
pub(crate) const PTE_ADDR_MASK: u64 = 0x0000_FFFF_FFFF_C000;

/// MAIR_EL1 value:
///   Attr0 = 0x00 (Device-nGnRnE)
///   Attr1 = 0xFF (Normal, Inner/Outer Write-Back, Read-Allocate, Write-Allocate)
///   Attr2 = 0x44 (Normal, Inner/Outer Non-Cacheable)
#[allow(dead_code)]
pub const MAIR_VALUE: u64 = 0x00_00_00_00_00_44_FF_00;

/// TCR_EL1 value for 16KB granule, 47-bit VA, both halves.
///   T0SZ = 17 (64 - 47)
///   T1SZ = 17
///   TG0 = 0b10 (16KB)
///   TG1 = 0b01 (16KB)
///   IPS = 0b101 (48-bit PA) — Apple M1 supports 42-bit PA but we use 48 to be safe
///   SH0/SH1 = 0b11 (Inner Shareable)
///   ORGN0/IRGN0/ORGN1/IRGN1 = 0b01 (Write-Back, Read-Allocate, Write-Allocate)
#[allow(dead_code)]
pub const TCR_VALUE: u64 = {
    let t0sz: u64 = 17;
    let t1sz: u64 = 17 << 16;
    let tg0: u64 = 0b10 << 14; // 16KB granule for TTBR0
    let tg1: u64 = 0b01 << 30; // 16KB granule for TTBR1
    let ips: u64 = 0b101 << 32; // 48-bit PA
    let sh0: u64 = 0b11 << 12; // Inner Shareable
    let sh1: u64 = 0b11 << 28; // Inner Shareable
    let orgn0: u64 = 0b01 << 10; // WB RA WA
    let irgn0: u64 = 0b01 << 8; // WB RA WA
    let orgn1: u64 = 0b01 << 26;
    let irgn1: u64 = 0b01 << 24;
    t0sz | t1sz | tg0 | tg1 | ips | sh0 | sh1 | orgn0 | irgn0 | orgn1 | irgn1
};

/// Extract the L1 index from a virtual address (bits [46:36]).
#[inline]
const fn l1_index(va: usize) -> usize {
    (va >> (PAGE_SHIFT + 2 * TABLE_INDEX_BITS)) & (TABLE_ENTRIES - 1)
}

/// Extract the L2 index from a virtual address (bits [35:25]).
#[inline]
const fn l2_index(va: usize) -> usize {
    (va >> (PAGE_SHIFT + TABLE_INDEX_BITS)) & (TABLE_ENTRIES - 1)
}

/// Extract the L3 index from a virtual address (bits [24:14]).
#[inline]
const fn l3_index(va: usize) -> usize {
    (va >> PAGE_SHIFT) & (TABLE_ENTRIES - 1)
}

/// Convert Xous `MemoryFlags` to AArch64 page table entry attributes.
fn flags_to_pte(flags: MemoryFlags, user: bool) -> u64 {
    let mut pte: u64 = PTE_VALID | PTE_TABLE | PTE_AF | PTE_SH_ISH;

    // Normal memory by default (not device)
    pte |= PTE_ATTR_NORMAL;

    if user {
        pte |= PTE_NG; // Per-ASID for user pages

        // W^X enforcement: a page cannot be both writable and executable
        let writable = flags.is_set(MemoryFlags::W);
        let executable = flags.is_set(MemoryFlags::X);

        if writable && executable {
            // Enforce W^X: default to writable, not executable
            pte |= PTE_AP_RW_ALL;
            pte |= PTE_UXN | PTE_PXN;
        } else if writable {
            pte |= PTE_AP_RW_ALL;
            pte |= PTE_UXN | PTE_PXN;
        } else if executable {
            pte |= PTE_AP_RO_ALL;
            pte |= PTE_PXN; // User can execute, kernel cannot
        } else {
            // Read-only: on ARM, all mapped pages are readable.
            // No W or X flag means read-only user access.
            pte |= PTE_AP_RO_ALL;
            pte |= PTE_UXN | PTE_PXN;
        }
    } else {
        // Kernel mapping: EL1 only
        if flags.is_set(MemoryFlags::W) {
            pte |= PTE_AP_RW_EL1;
        } else {
            pte |= PTE_AP_RO_EL1;
        }
        if flags.is_set(MemoryFlags::X) {
            // Kernel executable
            pte |= PTE_UXN; // Never user-executable
        } else {
            pte |= PTE_UXN | PTE_PXN;
        }
    }

    pte
}

/// The memory mapping for a single process.
/// On AArch64, this is essentially the TTBR0_EL1 value (user page table root)
/// plus the ASID.
#[derive(Copy, Clone, Default, Debug, PartialEq)]
pub struct MemoryMapping {
    /// Physical address of the L1 page table for this process (stored in TTBR0_EL1).
    ttbr0: usize,
    /// The PID / ASID for this mapping.
    pid: usize,
    /// ASLR slide applied to this process.
    aslr_slide: usize,
}

impl MemoryMapping {
    /// Get the currently active memory mapping by reading TTBR0_EL1 and CONTEXTIDR_EL1.
    pub fn current() -> MemoryMapping {
        let ttbr0: u64;
        let ctx: u64;
        unsafe {
            asm!(
                "mrs {ttbr}, ttbr0_el1",
                "mrs {ctx}, contextidr_el1",
                ttbr = out(reg) ttbr0,
                ctx = out(reg) ctx,
                options(nomem, nostack),
            );
        }
        MemoryMapping {
            ttbr0: (ttbr0 & PTE_ADDR_MASK) as usize,
            pid: ctx as usize,
            aslr_slide: 0,
        }
    }

    /// Get the PID from this mapping.
    pub fn get_pid(self) -> PID {
        PID::new(self.pid as u8).unwrap_or(unsafe { PID::new_unchecked(1) })
    }

    /// Activate this mapping — switch TTBR0_EL1 and CONTEXTIDR_EL1.
    pub fn activate(self) {
        let ttbr0_val = self.ttbr0 as u64 | ((self.pid as u64) << 48); // ASID in upper bits
        unsafe {
            asm!(
                "msr ttbr0_el1, {ttbr}",
                "msr contextidr_el1, {ctx}",
                "isb",
                ttbr = in(reg) ttbr0_val,
                ctx = in(reg) self.pid as u64,
                options(nomem, nostack),
            );
        }
    }

    /// Allocate a new page table hierarchy for a process.
    ///
    /// # Safety
    ///
    /// Must only be called during process creation.
    pub unsafe fn allocate(&mut self, pid: PID) -> Result<(), Error> {
        // Allocate an L1 table (16KB, 2048 entries × 8 bytes)
        let l1_phys = crate::mem::MemoryManager::with_mut(|mm| {
            mm.alloc_range(1, pid).map(|(addr, _zeroed)| addr).map_err(|_| Error::OutOfMemory)
        })?;

        // Zero the L1 table
        let l1_ptr = l1_phys as *mut u8;
        core::ptr::write_bytes(l1_ptr, 0, PAGE_SIZE);

        self.ttbr0 = l1_phys;
        self.pid = pid.get() as usize;
        Ok(())
    }

    /// Destroy this mapping — free page tables and flush TLB.
    #[allow(dead_code)]
    pub fn destroy(&self) {
        // Flush TLB entries for this ASID
        super::asm::flush_tlb_asid(self.pid as u16);
        // TODO(M2): Walk and free all page table pages
    }

    /// Map a physical page into this address space.
    pub fn map_page(
        &mut self,
        mm: &mut MemoryManager,
        phys: usize,
        virt: *mut usize,
        flags: MemoryFlags,
        map_user: bool,
    ) -> Result<(), Error> {
        let va = virt as usize;
        let pte_flags = flags_to_pte(flags, map_user);

        // Walk or allocate L1 → L2 → L3
        let l1_table = self.ttbr0 as *mut u64;
        let l1_idx = l1_index(va);
        let l2_table = self.ensure_table(mm, l1_table, l1_idx)?;
        let l2_idx = l2_index(va);
        let l3_table = self.ensure_table(mm, l2_table, l2_idx)?;
        let l3_idx = l3_index(va);

        // Write the L3 page entry
        let entry = (phys as u64 & PTE_ADDR_MASK) | pte_flags;
        unsafe {
            let l3_entry = l3_table.add(l3_idx);
            core::ptr::write_volatile(l3_entry, entry);
        }

        // Invalidate TLB for this address
        super::asm::flush_tlb_entry(va);
        Ok(())
    }

    /// Ensure a next-level table exists at `table[index]`. If not, allocate one.
    fn ensure_table(
        &self,
        mm: &mut MemoryManager,
        table: *mut u64,
        index: usize,
    ) -> Result<*mut u64, Error> {
        let entry = unsafe { core::ptr::read_volatile(table.add(index)) };
        if entry & PTE_VALID != 0 && entry & PTE_TABLE != 0 {
            // Table already exists
            Ok((entry & PTE_ADDR_MASK) as *mut u64)
        } else {
            // Allocate a new table page
            let pid = PID::new(self.pid as u8).unwrap_or(unsafe { PID::new_unchecked(1) });
            let new_table = mm.alloc_range(1, pid).map(|(addr, _zeroed)| addr).map_err(|_| Error::OutOfMemory)?;
            unsafe { core::ptr::write_bytes(new_table as *mut u8, 0, PAGE_SIZE) };

            // Write table descriptor
            let desc = (new_table as u64 & PTE_ADDR_MASK) | PTE_VALID | PTE_TABLE;
            unsafe { core::ptr::write_volatile(table.add(index), desc) };

            Ok(new_table as *mut u64)
        }
    }

    /// Unmap a page at the given virtual address.
    pub fn unmap_page(&self, virt: *mut usize) -> Result<(), Error> {
        let va = virt as usize;
        let l1_table = self.ttbr0 as *mut u64;

        // Walk L1 → L2 → L3
        let l1_entry = unsafe { core::ptr::read_volatile(l1_table.add(l1_index(va))) };
        if l1_entry & PTE_VALID == 0 {
            return Err(Error::BadAddress);
        }
        let l2_table = (l1_entry & PTE_ADDR_MASK) as *mut u64;
        let l2_entry = unsafe { core::ptr::read_volatile(l2_table.add(l2_index(va))) };
        if l2_entry & PTE_VALID == 0 {
            return Err(Error::BadAddress);
        }
        let l3_table = (l2_entry & PTE_ADDR_MASK) as *mut u64;
        let l3_idx = l3_index(va);

        // Clear the entry
        unsafe { core::ptr::write_volatile(l3_table.add(l3_idx), 0) };
        super::asm::flush_tlb_entry(va);
        Ok(())
    }

    /// Move a page from one address space to another.
    pub fn move_page(
        &mut self,
        mm: &mut MemoryManager,
        src_addr: *mut usize,
        dest_space: &mut MemoryMapping,
        dest_addr: *mut usize,
    ) -> Result<(), Error> {
        let phys = self.virt_to_phys(src_addr as *const usize)?;
        self.unmap_page(src_addr)?;
        // Map into destination with default RW flags
        dest_space.map_page(
            mm,
            phys,
            dest_addr,
            MemoryFlags::W,
            true,
        )
    }

    /// Lend a page to another address space (shared memory).
    pub fn lend_page(
        &mut self,
        mm: &mut MemoryManager,
        src_addr: *mut usize,
        dest_space: &mut MemoryMapping,
        dest_addr: *mut usize,
        mutable: bool,
    ) -> Result<(), Error> {
        let phys = self.virt_to_phys(src_addr as *const usize)?;
        let flags = if mutable {
            MemoryFlags::W
        } else {
            MemoryFlags::empty()
        };
        dest_space.map_page(mm, phys, dest_addr, flags, true)
    }

    /// Return a lent page.
    pub fn return_page(
        &mut self,
        src_addr: *mut usize,
        dest_space: &mut MemoryMapping,
        dest_addr: *mut usize,
    ) -> Result<(), Error> {
        // Unmap from the borrower
        self.unmap_page(src_addr)?;
        // The original page is still mapped in dest_space, nothing else needed.
        let _ = dest_addr;
        let _ = dest_space;
        Ok(())
    }

    /// Translate a virtual address to its physical address.
    pub fn virt_to_phys(&self, virt: *const usize) -> Result<usize, Error> {
        let va = virt as usize;
        let l1_table = self.ttbr0 as *mut u64;

        let l1_entry = unsafe { core::ptr::read_volatile(l1_table.add(l1_index(va))) };
        if l1_entry & PTE_VALID == 0 {
            return Err(Error::BadAddress);
        }
        let l2_table = (l1_entry & PTE_ADDR_MASK) as *mut u64;
        let l2_entry = unsafe { core::ptr::read_volatile(l2_table.add(l2_index(va))) };
        if l2_entry & PTE_VALID == 0 {
            return Err(Error::BadAddress);
        }
        let l3_table = (l2_entry & PTE_ADDR_MASK) as *mut u64;
        let l3_entry = unsafe { core::ptr::read_volatile(l3_table.add(l3_index(va))) };
        if l3_entry & PTE_VALID == 0 {
            return Err(Error::BadAddress);
        }

        let page_offset = va & (PAGE_SIZE - 1);
        Ok((l3_entry & PTE_ADDR_MASK) as usize | page_offset)
    }

    /// Invalidate a page mapping (used after returning memory).
    pub fn invalidate_page(&self, virt: *mut usize, _phys: usize) {
        super::asm::flush_tlb_entry(virt as usize);
    }

    /// Check if a virtual address is available (not mapped).
    pub fn address_available(&self, virt: *const usize) -> bool {
        self.virt_to_phys(virt).is_err()
    }

    /// Check if a virtual address is accessible from user mode (EL0).
    pub fn address_user_accessible(&self, virt: *const usize) -> bool {
        let va = virt as usize;
        // User addresses are in the lower half (TTBR0 range)
        if va >= beetos::USER_AREA_END {
            return false;
        }
        // Check if mapped
        self.virt_to_phys(virt).is_ok()
    }

    /// Flush cache for a memory range.
    pub fn flush_cache(&self, mem: xous::MemoryRange, op: CacheOperation) -> Result<(), Error> {
        let start = mem.as_ptr() as usize;
        let end = start + mem.len();
        let mut addr = start & !(64 - 1); // Cache line aligned (64 bytes on Apple M1)

        match op {
            CacheOperation::Clean => {
                while addr < end {
                    super::asm::dc_cvac(addr);
                    addr += 64;
                }
            }
            CacheOperation::CleanAndInvalidate => {
                while addr < end {
                    super::asm::dc_civac(addr);
                    addr += 64;
                }
            }
            CacheOperation::Invalidate => {
                while addr < end {
                    super::asm::dc_ivac(addr);
                    addr += 64;
                }
            }
        }
        super::asm::dsb();
        Ok(())
    }

    /// Print the page table map for debugging.
    #[allow(dead_code)]
    pub fn print_map(&self, output: &mut impl core::fmt::Write) {
        #[allow(unused_imports)]
        use core::fmt::Write;
        let _ = writeln!(output, "  TTBR0: {:#018x} ASID: {}", self.ttbr0, self.pid);
        // TODO(M2): Walk and print page table entries
    }

    /// Ensure a page exists at the given virtual address by allocating a
    /// backing physical page if needed (on-demand page allocation).
    pub fn ensure_page_exists(mm: &mut MemoryManager, address: *mut usize) -> Result<(), Error> {
        let mapping = MemoryMapping::current();
        // If the page is already mapped, nothing to do.
        if mapping.virt_to_phys(address as *const usize).is_ok() {
            return Ok(());
        }
        // Allocate a new physical page and map it as user read-write.
        let pid = mapping.get_pid();
        let (phys, zeroed) = mm.alloc_range(1, pid)?;
        // Zero the page if it wasn't already zeroed
        if !zeroed {
            unsafe { core::ptr::write_bytes(phys as *mut u8, 0, PAGE_SIZE) };
        }
        let mut mapping = mapping;
        mapping.map_page(mm, phys, address, MemoryFlags::W, true)
    }

    /// Check page table consistency against the memory manager.
    #[allow(dead_code)]
    pub fn check_consistency(&self, _mm: &MemoryManager, output: &mut impl core::fmt::Write) {
        #[allow(unused_imports)]
        use core::fmt::Write;
        let _ = writeln!(output, "  Consistency check: OK (stub)");
        // TODO(M2): Verify all mapped pages are tracked by MemoryManager
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_index_extraction() {
        // Address 0x0000_0001_0000 (64KB) should be in L1[0], L2[0], L3[4]
        let va = 0x0000_0001_0000;
        assert_eq!(l1_index(va), 0);
        assert_eq!(l2_index(va), 0);
        assert_eq!(l3_index(va), 4); // 0x10000 / 0x4000 = 4

        // Address near end of user space
        let va = 0x0000_6FFF_FFFF_C000;
        assert_eq!(l3_index(va), 2047);
    }

    #[test]
    fn test_flags_to_pte_wx_enforcement() {
        // W+X should result in writable but NOT executable (W^X)
        let pte = flags_to_pte(MemoryFlags::W | MemoryFlags::X, true);
        assert!(pte & PTE_UXN != 0, "W+X page should have UXN set");
    }

    #[test]
    fn test_page_table_constants() {
        assert_eq!(PAGE_SIZE, 16384);
        assert_eq!(TABLE_ENTRIES, 2048);
        assert_eq!(1 << PAGE_SHIFT, PAGE_SIZE);
    }
}
