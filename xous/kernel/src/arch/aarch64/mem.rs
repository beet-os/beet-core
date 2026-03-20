// SPDX-FileCopyrightText: 2024 BeetOS contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! AArch64 MMU and page table management for the Xous kernel.
//!
//! Uses ARMv8.0 translation tables with 16KB granule (Apple Silicon requirement).
//!
//! # Virtual Address Space Layout
//!
//! AArch64 splits the 64-bit virtual address space into two halves using two
//! independent translation table base registers:
//!
//! ```text
//! 0xFFFF_FFFF_FFFF_FFFF ┌─────────────────────────────┐
//!                        │ Exception stack (128KB)      │
//! 0xFFFF_FFFF_FFFF_0000  ├─────────────────────────────┤
//!                        │ IRQ stack (64KB)             │
//! 0xFFFF_FFFF_FFFE_4000  ├─────────────────────────────┤
//!                        │ Kernel stack (256KB)         │
//! 0xFFFF_FFFF_FFF8_0000  ├─────────────────────────────┤
//!                        │ Kernel code (up to 2MB)      │
//! 0xFFFF_FFFF_FFD0_0000  ├─────────────────────────────┤
//!                        │ ...                          │
//!                        │ Allocation tracker           │
//! 0xFFFF_FF80_0000_8000  ├─────────────────────────────┤
//!                        │ ...                          │
//!                        │ Physical RAM linear map      │
//! 0xFFFF_8000_0000_0000  ├═════════════════════════════┤ ← TTBR1 / TTBR0 boundary
//!                        │ Thread context (kernel-only) │
//! 0x0000_7000_0000_0000  ├─────────────────────────────┤
//!                        │ User stack (1MB)             │
//! 0x0000_6FF0_0000_0000  ├─────────────────────────────┤
//!                        │ User IRQ stack               │
//! 0x0000_6FE0_0000_0000  ├─────────────────────────────┤
//!                        │ ...                          │
//!                        │ Kernel arg table (PID1 only) │
//! 0x0000_5800_0000_0000  ├─────────────────────────────┤
//!                        │ Raw ELF temp loading area    │
//! 0x0000_5000_0000_0000  ├─────────────────────────────┤
//!                        │ Memory mirror (CoW/shared)   │
//! 0x0000_4000_0000_0000  ├─────────────────────────────┤
//!                        │ mmap area                    │
//! 0x0000_3000_0000_0000  ├─────────────────────────────┤
//!                        │ ASLR region (code + data)    │
//! 0x0000_0001_0000       ├─────────────────────────────┤
//!                        │ Null guard (64KB)            │
//! 0x0000_0000_0000_0000  └─────────────────────────────┘
//! ```
//!
//! **Upper half (TTBR1_EL1):** Kernel space. Set once at boot, never changed.
//! The kernel accesses all physical memory via a linear map:
//! `kernel_VA = PA + 0xFFFF_8000_0000_0000`. See [`beetos::phys_to_virt`].
//!
//! **Lower half (TTBR0_EL1):** Per-process user space. Switched on every
//! context switch by writing a new L1 table physical address (+ ASID) to
//! TTBR0_EL1. Each process gets its own L1 table; the kernel is NOT mapped
//! in any user page table.
//!
//! # Page Table Structure (16KB granule, 47-bit VA)
//!
//! With `TCR_EL1.TG0 = 10` (16KB) and `T0SZ = T1SZ = 17` (47-bit VA per half),
//! the hardware walks a **3-level** table (L1 → L2 → L3):
//!
//! ```text
//!   63        47 46        36 35        25 24       14 13        0
//!  ┌──────────┬─────────────┬─────────────┬───────────┬──────────┐
//!  │ TTBR sel │  L1 index   │  L2 index   │ L3 index  │  offset  │
//!  │ (0/FFFF) │ (11 bits)   │ (11 bits)   │ (11 bits) │ (14 bits)│
//!  └──────────┴─────────────┴─────────────┴───────────┴──────────┘
//!               2048 entries  2048 entries  2048 entries  16KB page
//!               × 64 GiB ea   × 32 MiB ea  × 16 KiB ea
//! ```
//!
//! Each table level has **2048 entries** (16KB table / 8 bytes per entry).
//! - **L1** entry covers **64 GiB** — points to an L2 table
//! - **L2** entry covers **32 MiB** — points to an L3 table (or a 32MB block)
//! - **L3** entry covers **16 KiB** — points to the final physical page
//!
//! Total addressable per half: 2048 × 64 GiB = 128 TiB (47 bits).
//!
//! # Page Table Entry Format (L3 page descriptor)
//!
//! ```text
//!   63  55 54 53 52         12/14        2 1 0
//!  ┌─────┬──┬──┬──┬─────────────────────┬───┬─┐
//!  │     │UX│PX│  │   Output Address    │ AP│V│
//!  │     │N │N │  │   (PA bits [47:14]) │   │ │
//!  └─────┴──┴──┴──┴─────────────────────┴───┴─┘
//!
//!  Bit 0     : Valid (1 = entry is active)
//!  Bit 1     : Table/Page (1 = L3 page descriptor or L1/L2 table pointer)
//!  Bits 2-4  : AttrIndx (indexes into MAIR_EL1 — selects memory type)
//!  Bits 6-7  : AP (access permissions — EL1-only, EL0+EL1, RO, RW)
//!  Bits 8-9  : SH (shareability — ISH for SMP cache coherency)
//!  Bit 10    : AF (access flag — must be 1 to avoid access-flag faults)
//!  Bit 11    : nG (non-global — 1 for per-ASID user pages)
//!  Bits 14-47: Output address (physical page address, 16KB-aligned)
//!  Bit 53    : PXN (privileged execute-never — blocks EL1 execution)
//!  Bit 54    : UXN (user execute-never — blocks EL0 execution)
//! ```
//!
//! # Memory Attributes (MAIR_EL1)
//!
//! Three memory types are configured via `MAIR_EL1`:
//!
//! | Index | AttrIndx | Type                  | Use                         |
//! |-------|----------|-----------------------|-----------------------------|
//! | 0     | `0x00`   | Device-nGnRnE         | MMIO registers              |
//! | 1     | `0xFF`   | Normal WB, RA, WA     | RAM (code, data, stacks)    |
//! | 2     | `0x44`   | Normal Non-Cacheable   | DMA buffers, shared memory  |
//!
//! # W^X Enforcement
//!
//! No page is ever mapped as both **W**ritable and e**X**ecutable. If both
//! `MemoryFlags::W` and `MemoryFlags::X` are requested, the page is mapped
//! as writable-only (UXN + PXN set). This is enforced in [`flags_to_pte`].
//!
//! # ASID (Address Space ID)
//!
//! Each process is assigned an ASID equal to its PID (8-bit). The ASID is
//! stored in bits [63:48] of TTBR0_EL1. User pages are marked non-global
//! (nG = 1), so the TLB tags them with the ASID. This avoids a full TLB
//! flush on every context switch — only TTBR0 and CONTEXTIDR_EL1 are updated.
//!
//! # Boot Sequence
//!
//! 1. `start.S::_create_boot_page_tables` — creates identity map (TTBR0) and
//!    kernel linear map (TTBR1) using **L2 block descriptors** (32 MiB blocks).
//!    Maps first 2 GiB: 1 GiB MMIO (device) + 1 GiB RAM (normal cacheable).
//! 2. `start.S::_enable_mmu_boot` — writes MAIR, TCR, TTBR0, TTBR1, enables MMU.
//! 3. Boot code jumps to `_high_va_entry` at kernel high VA (through TTBR1).
//! 4. `boot.rs` replaces the boot identity map with proper per-process L3 tables.
//!
//! After boot, TTBR1 keeps the kernel linear map permanently. TTBR0 is
//! overwritten with each process's own L1 table on context switch.
//!
//! # Accessing Page Tables at Runtime
//!
//! Page table pages are physical memory. The kernel accesses them through the
//! TTBR1 linear map: `let l1_va = beetos::phys_to_virt(self.ttbr0)`. This
//! works regardless of the current TTBR0 setting, so the kernel can manipulate
//! any process's page tables without switching address spaces.
//!
//! # TLB Invalidation
//!
//! After modifying a PTE, the corresponding TLB entry must be invalidated.
//! See [`super::asm`] for the three invalidation primitives:
//! - [`flush_tlb_entry`](super::asm::flush_tlb_entry) — single VA (TLBI VAALE1IS)
//! - [`flush_tlb_asid`](super::asm::flush_tlb_asid) — all entries for an ASID (TLBI ASIDE1IS)
//! - [`flush_tlb_all`](super::asm::flush_tlb_all) — entire TLB (TLBI VMALLE1IS)
//!
//! All use Inner-Shareable (IS) variants for SMP correctness.

use core::arch::asm;

use xous::{CacheOperation, Error, MemoryFlags, PID};

use crate::mem::MemoryManager;

pub use beetos::PAGE_SIZE;

/// MMIO addresses shared with all user processes (mapped into every address space).
/// Populated at boot from FDT. Empty until M2 platform init.
pub static SHARED_PERIPHERALS: &[usize] = &[];

// ---------------------------------------------------------------------------
// Page table geometry constants (16KB granule)
// ---------------------------------------------------------------------------
//
// With 16KB pages, each table is one page (16KB) and holds 2048 8-byte entries.
// The virtual address is split into four fields (see module doc above):
//
//   [63:47]  TTBR selector (all-zeros → TTBR0, all-ones → TTBR1)
//   [46:36]  L1 index (11 bits → 2048 entries, each covering 64 GiB)
//   [35:25]  L2 index (11 bits → 2048 entries, each covering 32 MiB)
//   [24:14]  L3 index (11 bits → 2048 entries, each covering 16 KiB)
//   [13:0]   Page offset (14 bits → 16384 bytes)

const PAGE_SHIFT: usize = 14; // log2(16384) — 16KB page size
const TABLE_ENTRIES: usize = 2048; // 16KB table / 8 bytes per entry
const TABLE_INDEX_BITS: usize = 11; // log2(2048) — bits per level

// ---------------------------------------------------------------------------
// Page table entry (PTE) bit definitions
// ---------------------------------------------------------------------------
//
// ARMv8-A D5.3 — VMSAv8-64 translation table format descriptors.
//
// L1/L2 TABLE descriptor:   [47:14] = next-level table PA, bit[1:0] = 0b11
// L3 PAGE descriptor:       [47:14] = output page PA,      bit[1:0] = 0b11
// INVALID descriptor:       bit[0] = 0

/// Bit 0: Valid — entry is active and will be used by the hardware walker.
const PTE_VALID: u64 = 1 << 0;

/// Bit 1: Table (L1/L2) or Page (L3) — distinguishes table pointers from
/// block/page descriptors. For L3, this bit must be 1 for a page descriptor.
const PTE_TABLE: u64 = 1 << 1;

// Lower attributes (bits [11:2])
const PTE_ATTR_IDX_SHIFT: u64 = 2;

/// AttrIndx = 0 → MAIR Attr0 = 0x00 (Device-nGnRnE).
/// Used for MMIO: no gathering, no reordering, no early write acknowledgement.
#[allow(dead_code)]
const PTE_ATTR_DEVICE: u64 = 0 << PTE_ATTR_IDX_SHIFT;

/// AttrIndx = 1 → MAIR Attr1 = 0xFF (Normal, Inner/Outer Write-Back, RA+WA).
/// Used for all RAM (code, data, stacks).
const PTE_ATTR_NORMAL: u64 = 1 << PTE_ATTR_IDX_SHIFT;

/// AttrIndx = 2 → MAIR Attr2 = 0x44 (Normal, Inner/Outer Non-Cacheable).
/// Reserved for DMA buffers or uncached shared memory.
#[allow(dead_code)]
const PTE_ATTR_NORMAL_NC: u64 = 2 << PTE_ATTR_IDX_SHIFT;

// Access Permissions (AP[2:1], bits [7:6]).
// ARMv8-A Table D5-34: data access permissions for stage 1.
//
//   AP  | EL1 (kernel) | EL0 (user)
//  -----+--------------+-----------
//  0b00 | Read/Write   | No access
//  0b01 | Read/Write   | Read/Write
//  0b10 | Read-only    | No access
//  0b11 | Read-only    | Read-only

/// AP = 0b00: Read/write at EL1 only (kernel RW, user no access).
const PTE_AP_RW_EL1: u64 = 0b00 << 6;

/// AP = 0b01: Read/write at EL0 and EL1 (both kernel and user RW).
const PTE_AP_RW_ALL: u64 = 0b01 << 6;

/// AP = 0b10: Read-only at EL1 only (kernel RO, user no access).
const PTE_AP_RO_EL1: u64 = 0b10 << 6;

/// AP = 0b11: Read-only at EL0 and EL1 (both kernel and user RO).
const PTE_AP_RO_ALL: u64 = 0b11 << 6;

/// SH = 0b11 (Inner Shareable). Required for SMP cache coherency — ensures
/// that cache maintenance operations are broadcast to all cores sharing the
/// inner domain.
const PTE_SH_ISH: u64 = 0b11 << 8;

/// Access Flag. Must be set to 1 in all valid entries. If clear, the first
/// access generates a permission fault. We always pre-set it (no lazy AF).
const PTE_AF: u64 = 1 << 10;

/// Non-Global. When set, TLB entries are tagged with the ASID from TTBR0_EL1.
/// Set on all user pages so TLB entries are per-process. Kernel pages (via
/// TTBR1) are global (nG = 0) and shared across all processes.
const PTE_NG: u64 = 1 << 11;

// Upper attributes (bits [63:52])

/// Privileged Execute-Never. Prevents EL1 from executing code on this page.
/// Set on all user-executable pages (user code should not be kernel-executable).
const PTE_PXN: u64 = 1 << 53;

/// User (Unprivileged) Execute-Never. Prevents EL0 from executing code on
/// this page. Set on all writable user pages to enforce W^X.
const PTE_UXN: u64 = 1 << 54;

/// Address mask for the output address field in a PTE (16KB granule).
/// Extracts bits [47:14] — the physical page-frame address, 16KB-aligned.
pub(crate) const PTE_ADDR_MASK: u64 = 0x0000_FFFF_FFFF_C000;

/// MAIR_EL1 — Memory Attribute Indirection Register.
///
/// Defines up to 8 memory attribute encodings. PTEs reference these by index
/// (AttrIndx field, bits [4:2]). Only three slots are used:
///
/// ```text
///   Byte index:  7    6    5    4    3    2    1    0
///   MAIR value: 0x00 0x00 0x00 0x00 0x00 0x44 0xFF 0x00
///                                         │    │    └─ Attr0: Device-nGnRnE (MMIO)
///                                         │    └────── Attr1: Normal WB RA+WA (RAM)
///                                         └─────────── Attr2: Normal NC (DMA)
/// ```
///
/// This value is written to MAIR_EL1 in `start.S::_enable_mmu_boot` and must
/// match the `PTE_ATTR_*` constants above.
#[allow(dead_code)]
pub const MAIR_VALUE: u64 = 0x00_00_00_00_00_44_FF_00;

/// TCR_EL1 — Translation Control Register.
///
/// Controls the page table walk for both TTBR0 (user) and TTBR1 (kernel):
///
/// ```text
///   Field   Bits    Value  Meaning
///   ─────── ─────── ────── ────────────────────────────────────────
///   T0SZ    [5:0]   17     VA size = 64 - 17 = 47 bits (TTBR0)
///   IRGN0   [9:8]   0b01   Inner WB RA+WA for TTBR0 walks
///   ORGN0   [11:10] 0b01   Outer WB RA+WA for TTBR0 walks
///   SH0     [13:12] 0b11   Inner Shareable for TTBR0 walks
///   TG0     [15:14] 0b10   16KB granule for TTBR0
///   T1SZ    [21:16] 17     VA size = 64 - 17 = 47 bits (TTBR1)
///   IRGN1   [25:24] 0b01   Inner WB RA+WA for TTBR1 walks
///   ORGN1   [27:26] 0b01   Outer WB RA+WA for TTBR1 walks
///   SH1     [29:28] 0b11   Inner Shareable for TTBR1 walks
///   TG1     [31:30] 0b01   16KB granule for TTBR1
///   IPS     [34:32] 0b101  48-bit physical address space
/// ```
///
/// Apple M1 supports 42-bit PA, but we configure 48-bit (IPS=101) for
/// portability across platforms. The hardware ignores unused upper PA bits.
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

/// Convert Xous [`MemoryFlags`] to AArch64 page table entry attribute bits.
///
/// Builds the lower + upper attribute fields for an L3 page descriptor:
///
/// | Xous flag | User page (EL0)         | Kernel page (EL1-only)     |
/// |-----------|-------------------------|----------------------------|
/// | `W`       | AP=RW_ALL, UXN+PXN      | AP=RW_EL1                  |
/// | `X`       | AP=RO_ALL, PXN (no UXN) | UXN (no PXN)               |
/// | `W + X`   | → **W only** (W^X)       | — (not used for kernel)   |
/// | neither   | AP=RO_ALL, UXN+PXN      | AP=RO_EL1, UXN+PXN        |
/// | `DEV`     | Device memory attrs      | Device memory attrs        |
///
/// All entries get: `PTE_VALID | PTE_TABLE | PTE_AF | PTE_SH_ISH`.
/// User entries additionally get `PTE_NG` (non-global, ASID-tagged).
fn flags_to_pte(flags: MemoryFlags, user: bool) -> u64 {
    let mut pte: u64 = PTE_VALID | PTE_TABLE | PTE_AF | PTE_SH_ISH;

    // Device memory: uncacheable, strongly-ordered MMIO accesses
    if flags.is_set(MemoryFlags::DEV) {
        pte |= PTE_ATTR_DEVICE;
    } else {
        pte |= PTE_ATTR_NORMAL;
    }

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
///
/// User process mappings go into TTBR0 (low VA addresses).
/// The kernel lives in TTBR1 (high VA) and is NOT part of any process's mapping.
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

    /// Get the physical address of the L1 page table.
    #[allow(dead_code)]
    pub fn ttbr0(&self) -> usize {
        self.ttbr0
    }

    /// Activate this mapping — switch TTBR0_EL1 and CONTEXTIDR_EL1.
    ///
    /// Only TTBR0 changes (user address space). TTBR1 (kernel) never changes.
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

    /// Allocate a new page table hierarchy for a user process.
    ///
    /// Creates a fresh L1 table for TTBR0. The table is initially empty —
    /// user pages are mapped into it as needed. The kernel is NOT mapped
    /// in user TTBR0; it lives in TTBR1 which never changes.
    ///
    /// # Safety
    ///
    /// Must only be called during process creation.
    pub unsafe fn allocate(&mut self, pid: PID) -> Result<(), Error> {
        // Allocate an L1 table (16KB, 2048 entries × 8 bytes)
        let l1_phys = crate::mem::MemoryManager::with_mut(|mm| {
            mm.alloc_range(1, pid).map(|(addr, _zeroed)| addr).map_err(|_| Error::OutOfMemory)
        })?;

        // Zero the L1 table (access via high VA through TTBR1)
        let l1_va = beetos::phys_to_virt(l1_phys);
        core::ptr::write_bytes(l1_va as *mut u8, 0, PAGE_SIZE);

        // User TTBR0 starts empty — no kernel mappings needed.
        // The kernel runs entirely in TTBR1 (high VA).

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
    ///
    /// Page tables are accessed via `phys_to_virt()` (through TTBR1),
    /// so this works regardless of the current TTBR0 setting.
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
        // Access the L1 table at its high VA (through TTBR1)
        let l1_table = beetos::phys_to_virt(self.ttbr0) as *mut u64;
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
    ///
    /// `table` is a high VA pointer (accessed through TTBR1).
    /// Returns a high VA pointer to the next-level table.
    fn ensure_table(
        &self,
        mm: &mut MemoryManager,
        table: *mut u64,
        index: usize,
    ) -> Result<*mut u64, Error> {
        let entry = unsafe { core::ptr::read_volatile(table.add(index)) };
        if entry & PTE_VALID != 0 && entry & PTE_TABLE != 0 {
            // Table already exists — extract its PA and convert to high VA
            let next_pa = (entry & PTE_ADDR_MASK) as usize;
            Ok(beetos::phys_to_virt(next_pa) as *mut u64)
        } else {
            // Allocate a new table page (returns PA)
            let pid = PID::new(self.pid as u8).unwrap_or(unsafe { PID::new_unchecked(1) });
            let new_table_pa = mm.alloc_range(1, pid).map(|(addr, _zeroed)| addr).map_err(|_| Error::OutOfMemory)?;
            // Zero the new table (via high VA)
            let new_table_va = beetos::phys_to_virt(new_table_pa);
            unsafe { core::ptr::write_bytes(new_table_va as *mut u8, 0, PAGE_SIZE) };

            // Write table descriptor (PA in the PTE, not VA)
            let desc = (new_table_pa as u64 & PTE_ADDR_MASK) | PTE_VALID | PTE_TABLE;
            unsafe { core::ptr::write_volatile(table.add(index), desc) };

            Ok(new_table_va as *mut u64)
        }
    }

    /// Unmap a page at the given virtual address.
    pub fn unmap_page(&self, virt: *mut usize) -> Result<(), Error> {
        let va = virt as usize;
        let l1_table = beetos::phys_to_virt(self.ttbr0) as *mut u64;

        // Walk L1 → L2 → L3 (all via high VA)
        let l1_entry = unsafe { core::ptr::read_volatile(l1_table.add(l1_index(va))) };
        if l1_entry & PTE_VALID == 0 {
            return Err(Error::BadAddress);
        }
        let l2_table = beetos::phys_to_virt((l1_entry & PTE_ADDR_MASK) as usize) as *mut u64;
        let l2_entry = unsafe { core::ptr::read_volatile(l2_table.add(l2_index(va))) };
        if l2_entry & PTE_VALID == 0 {
            return Err(Error::BadAddress);
        }
        let l3_table = beetos::phys_to_virt((l2_entry & PTE_ADDR_MASK) as usize) as *mut u64;
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
        let l1_table = beetos::phys_to_virt(self.ttbr0) as *mut u64;

        let l1_entry = unsafe { core::ptr::read_volatile(l1_table.add(l1_index(va))) };
        if l1_entry & PTE_VALID == 0 {
            return Err(Error::BadAddress);
        }
        let l2_table = beetos::phys_to_virt((l1_entry & PTE_ADDR_MASK) as usize) as *mut u64;
        let l2_entry = unsafe { core::ptr::read_volatile(l2_table.add(l2_index(va))) };
        if l2_entry & PTE_VALID == 0 {
            return Err(Error::BadAddress);
        }
        let l3_table = beetos::phys_to_virt((l2_entry & PTE_ADDR_MASK) as usize) as *mut u64;
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
        // Zero the page if it wasn't already zeroed (via high VA)
        if !zeroed {
            unsafe {
                let va = beetos::phys_to_virt(phys);
                core::ptr::write_bytes(va as *mut u8, 0, PAGE_SIZE);
            };
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
