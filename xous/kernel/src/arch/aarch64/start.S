// SPDX-FileCopyrightText: 2024 BeetOS contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

// AArch64 kernel entry point — bootstrap page tables and TTBR1 kernel mapping.
//
// This file creates the initial page tables that let the kernel transition
// from running at physical addresses (MMU off) to running at high virtual
// addresses through TTBR1 (MMU on).
//
// ╔═══════════════════════════════════════════════════════════════════════╗
// ║                        BOOT SEQUENCE                                ║
// ╠═══════════════════════════════════════════════════════════════════════╣
// ║  1. _start: entry at PA, MMU OFF                                    ║
// ║     └─ Drop EL2 → EL1 if needed (RPi5, some QEMU configs)          ║
// ║     └─ Enable FP/SIMD (CPACR_EL1.FPEN = 0b11)                      ║
// ║     └─ Save FDT pointer (x0 → x19)                                 ║
// ║                                                                     ║
// ║  2. _create_boot_page_tables:                                       ║
// ║     └─ TTBR0 L1[0] → shared L2 (identity map for boot transition)  ║
// ║     └─ TTBR1 L1[0] → shared L2 (kernel linear map, permanent)      ║
// ║     └─ L2[0..31]  = Device blocks (32 × 32MB = 1GB MMIO)           ║
// ║     └─ L2[32..63] = Normal blocks (32 × 32MB = 1GB RAM)            ║
// ║                                                                     ║
// ║  3. _enable_mmu_boot:                                               ║
// ║     └─ Write MAIR_EL1, TCR_EL1, TTBR0_EL1, TTBR1_EL1              ║
// ║     └─ TLB invalidate all, DSB+ISB                                  ║
// ║     └─ Set SCTLR_EL1.M = 1 (MMU ON)                                ║
// ║                                                                     ║
// ║  4. Branch to _high_va_entry (absolute address in TTBR1 space)      ║
// ║     └─ Now executing at 0xFFFF_8000_... through TTBR1               ║
// ║     └─ Set real kernel stack (SP = _stack_top at high VA)           ║
// ║     └─ Set VBAR_EL1 to exception vectors at high VA                 ║
// ║     └─ Clear .bss, call _start_rust(fdt_ptr)                        ║
// ║                                                                     ║
// ║  After step 4: ALL kernel code runs through TTBR1.                  ║
// ║  TTBR0 is free for user process page tables.                        ║
// ╚═══════════════════════════════════════════════════════════════════════╝
//
// BOOTSTRAP PAGE TABLE LAYOUT (16KB granule, L2 block descriptors):
//
//   TTBR0 L1 (identity map)     TTBR1 L1 (kernel map)
//   ┌─────────────────┐         ┌─────────────────┐
//   │ [0] → L2 (PA)   │         │ [0] → L2 (PA)   │  ← same L2 table!
//   │ [1..2047] = 0   │         │ [1..2047] = 0   │
//   └─────────────────┘         └─────────────────┘
//
//   Shared L2 table (64 entries used):
//   ┌──────────────────────────────────────────────────────────┐
//   │ [0..31]  → Device blocks: PA 0x0000_0000..0x3FFF_FFFF   │  1GB MMIO
//   │ [32..63] → Normal blocks: PA 0x4000_0000..0x7FFF_FFFF   │  1GB RAM
//   │ [64..2047] = 0 (unmapped)                                │
//   └──────────────────────────────────────────────────────────┘
//
//   Identity map (TTBR0): VA 0x4000_0000 → PA 0x4000_0000 (same address)
//   Kernel map  (TTBR1): VA 0xFFFF_8000_4000_0000 → PA 0x4000_0000
//
// The identity map lets the CPU keep fetching instructions after MMU enable
// (PC is still at PA). The kernel map lets us branch to high VA immediately
// after. Once at high VA, the identity map in TTBR0 is no longer needed and
// will be replaced by user process page tables.

// ============================================================================
// Boot section — runs at physical address with MMU off
// ============================================================================
.section .text.boot, "ax"
.globl _start
_start:
    // --- EL2 → EL1 drop (if at EL2, e.g. RPi5 firmware) ---
    // At EL1 (QEMU default with virtualization=off) this is skipped.
    mrs     x1, CurrentEL
    lsr     x1, x1, #2             // CurrentEL[3:2] → x1
    cmp     x1, #2
    b.ne    .Lat_el1

    // Configure EL1 as AArch64 (HCR_EL2.RW = 1)
    mov     x1, #(1 << 31)
    msr     hcr_el2, x1

    // SPSR_EL2: return to EL1h, all exceptions masked (DAIF = 0b1111)
    mov     x1, #0x3c5
    msr     spsr_el2, x1

    // ELR_EL2: return to .Lat_el1
    adr     x1, .Lat_el1
    msr     elr_el2, x1

    eret                            // → EL1, PC = .Lat_el1

.Lat_el1:
    // Mask all exceptions during boot
    msr     daifset, #0xf

    // Enable FP/SIMD at EL1 (CPACR_EL1.FPEN = 0b11)
    // Without this, any NEON instruction (including compiler-generated
    // memcpy/memset) traps as Undefined Instruction.
    mov     x1, #(3 << 20)
    msr     cpacr_el1, x1
    isb

    // Save FDT pointer (x0 from bootloader) in a callee-saved register
    mov     x19, x0

    // Set up a small boot stack for page table creation (at PA).
    // _boot_stack_top is in .boot.bss at physical address.
    adr     x1, _boot_stack_top
    mov     sp, x1

    // === Create bootstrap page tables ===
    bl      _create_boot_page_tables

    // === Enable MMU ===
    bl      _enable_mmu_boot

    // === Jump to high VA ===
    // _high_va_entry is linked at high VA (TTBR1 space).
    // ldr loads the absolute address from the literal pool (in .text.boot).
    // After MMU enable, TTBR1 maps high VA → PA, so the branch succeeds.
    ldr     x8, =_high_va_entry
    br      x8

// We should never reach here
.Lhang_boot:
    wfe
    b       .Lhang_boot

// ============================================================================
// Boot page table setup (runs at PA, called from _start)
// ============================================================================
//
// Creates two L1 tables that share an L2_low table for the kernel image
// and low-PA mapping, plus an optional L2_high table for high-PA MMIO
// (used by BCM2712-style chips where peripherals live above 1 TiB).
//
//   TTBR0 (identity):  VA 0..2 GiB → PA 0..2 GiB
//   TTBR1 (kernel):    VA 0xFFFF_8000_0000_0000 + PA (linear map)
//
// Pre-allocated in .boot.bss (must be 16KB-aligned for hardware):
//   _boot_l1_ttbr0 (16KB): L1 table for TTBR0 (identity map)
//   _boot_l1_ttbr1 (16KB): L1 table for TTBR1 (kernel map)
//   _boot_l2_low   (16KB): L2 table for the low PA range (RAM + maybe MMIO)
//   _boot_l2_high  (16KB): L2 table for the high PA range (MMIO above 1 TiB)
//
// 16KB granule geometry:
//   L1 entry covers 2048 × 32 MiB = 64 GiB (points to L2 table)
//   L2 BLOCK entry covers 32 MiB directly (no L3 needed for boot)
//
// The exact L2 content is platform-specific: QEMU virt splits the low
// range into device (0..1 GiB) + normal (1..2 GiB) and never uses the
// high table; BCM2712 makes the entire low range normal RAM and wires
// L1[16] → L2_high for the peripheral region at PA 0x100_0000_0000+.
// The platform-specific symbol `_fill_boot_tables` implements that
// policy. See `pagetables_qemu_virt.S` / `pagetables_bcm2712.S`.

_create_boot_page_tables:
    stp     x29, x30, [sp, #-16]!

    // Zero all four page tables (4 × 16KB = 64KB = 8192 u64s).
    adr     x0, _boot_l1_ttbr0
    mov     x1, #(4 * 16384 / 8)
.Lzero_tables:
    str     xzr, [x0], #8
    subs    x1, x1, #1
    b.ne    .Lzero_tables

    // Hand off to the platform's _fill_boot_tables. It receives the
    // four page-table PAs and is expected to:
    //   - fill _boot_l2_low (and optionally _boot_l2_high) with block
    //     descriptors appropriate for this platform
    //   - install L1[0] → L2_low in both TTBR0 and TTBR1
    //   - if needed, install L1[N] → L2_high in TTBR1 for the high MMIO
    // ABI:
    //   x0 = _boot_l1_ttbr0
    //   x1 = _boot_l1_ttbr1
    //   x2 = _boot_l2_low
    //   x3 = _boot_l2_high
    adr     x0, _boot_l1_ttbr0
    adr     x1, _boot_l1_ttbr1
    adr     x2, _boot_l2_low
    adr     x3, _boot_l2_high
    bl      _fill_boot_tables

    ldp     x29, x30, [sp], #16
    ret

// ============================================================================
// Enable MMU (runs at PA, called from _start)
// ============================================================================
//
// Configures the four system registers that control address translation,
// then flips the MMU enable bit. After `msr sctlr_el1` + `isb`, the CPU
// fetches the next instruction through the page tables.
//
// Register setup order (must be done with MMU OFF):
//   1. MAIR_EL1  — define memory attribute encodings
//   2. TCR_EL1   — configure page table walk parameters
//   3. TTBR0_EL1 — load identity map L1 base (temporary)
//   4. TTBR1_EL1 — load kernel map L1 base (permanent)
//   5. TLB invalidate — ensure no stale entries
//   6. SCTLR_EL1.M = 1 — MMU ON

_enable_mmu_boot:
    // MAIR_EL1: Memory Attribute Indirection Register
    // Maps AttrIndx values in PTEs to actual memory type encodings:
    //   Attr0 (byte 0) = 0x00: Device-nGnRnE (strongest device ordering)
    //   Attr1 (byte 1) = 0xFF: Normal, Inner+Outer Write-Back, Read+Write Allocate
    //   Attr2 (byte 2) = 0x44: Normal, Inner+Outer Non-Cacheable
    // Must match PTE_ATTR_DEVICE/PTE_ATTR_NORMAL/PTE_ATTR_NORMAL_NC in mem.rs
    movz    x0, #0xFF00
    movk    x0, #0x0044, lsl #16
    msr     mair_el1, x0

    // TCR_EL1: Translation Control Register
    // Controls page table walk for both TTBR0 (user) and TTBR1 (kernel).
    //
    //   T0SZ  = 17  → 47-bit VA for TTBR0 (user):   0x0000_0000..0x7FFF_FFFF_FFFF
    //   T1SZ  = 17  → 47-bit VA for TTBR1 (kernel):  0xFFFF_8000_0000_0000..0xFFFF_FFFF_FFFF_FFFF
    //   TG0   = 10  → 16KB granule for TTBR0
    //   TG1   = 01  → 16KB granule for TTBR1
    //   IPS   = 101 → 48-bit physical address space
    //   SH0/1 = 11  → Inner Shareable (page table walks are cache-coherent)
    //   *RGN* = 01  → Write-Back, Read+Write Allocate (page table walk caching)
    //
    // Pre-computed value: 0x0000_0005_7511_B511
    // Must match TCR_VALUE in mem.rs
    ldr     x0, =0x57511B511
    msr     tcr_el1, x0

    // TTBR0_EL1: Translation Table Base Register 0
    // Points to the boot identity-map L1 table. This is temporary — replaced
    // by user process page tables after the first context switch.
    adr     x0, _boot_l1_ttbr0
    msr     ttbr0_el1, x0

    // TTBR1_EL1: Translation Table Base Register 1
    // Points to the kernel linear-map L1 table. This is PERMANENT — never
    // changed after boot. All kernel code and data is accessed through TTBR1.
    adr     x0, _boot_l1_ttbr1
    msr     ttbr1_el1, x0

    // Barrier: ensure all page table writes are visible to the hardware walker
    // before we enable translation.
    dsb     ish
    isb

    // Invalidate all TLB entries (inner-shareable) to ensure no stale
    // translations from a previous boot or warm reset.
    tlbi    vmalle1is
    dsb     ish
    isb

    // Enable the MMU by setting SCTLR_EL1 bits:
    //   M   (bit 0)  = 1: MMU enable
    //   C   (bit 2)  = 1: Data cache enable
    //   SA  (bit 3)  = 1: Stack alignment check enable
    //   I   (bit 12) = 1: Instruction cache enable
    //   UCI (bit 26) = 1: Allow EL0 to use cache maintenance instructions
    //   WXN (bit 19) = 0: Do NOT enforce W→XN globally (we enforce W^X per-PTE)
    movz    x0, #0x100D             // bits 0,2,3,12
    movk    x0, #0x0400, lsl #16   // bit 26
    msr     sctlr_el1, x0
    isb                             // Synchronize — next fetch uses MMU

    ret

// ============================================================================
// Literal pool for .text.boot
// ============================================================================
.ltorg

// ============================================================================
// Boot BSS: pre-allocated page tables and boot stack
// ============================================================================
.section .boot.bss, "aw", @nobits

// Page tables must be 16KB-aligned (16KB granule requirement)
.balign 16384
.globl _boot_l1_ttbr0
_boot_l1_ttbr0:
    .space 16384

.globl _boot_l1_ttbr1
_boot_l1_ttbr1:
    .space 16384

.globl _boot_l2_low
_boot_l2_low:
    .space 16384

.globl _boot_l2_high
_boot_l2_high:
    .space 16384

// Small boot stack (4KB) for the page table setup code.
// The real kernel stack is in .stack at high VA.
.balign 16
_boot_stack_bottom:
    .space 4096
.globl _boot_stack_top
_boot_stack_top:

// ============================================================================
// High VA entry point — runs after MMU enable, through TTBR1
// ============================================================================
// This is in .text (linked at high VA). The boot trampoline in .text.boot
// jumps here after enabling MMU with both identity map (TTBR0) and
// kernel map (TTBR1).
//
// From this point on, ALL kernel code runs at high VA through TTBR1.
// TTBR0 still has the boot identity map — it will be replaced by user
// process page tables during the first context switch.

.section .text, "ax"
.globl _high_va_entry
_high_va_entry:
    // Set up the real kernel stack at high VA.
    // _stack_top is a linker symbol at high VA.
    adrp    x1, _stack_top
    add     x1, x1, :lo12:_stack_top
    mov     sp, x1

    // Set exception vector table at high VA.
    // _exception_vectors is in .text.vectors at high VA.
    adrp    x1, _exception_vectors
    add     x1, x1, :lo12:_exception_vectors
    msr     vbar_el1, x1
    isb

    // Clear BSS section (at high VA, zeroed through TTBR1).
    adrp    x1, _sbss
    add     x1, x1, :lo12:_sbss
    adrp    x2, _ebss
    add     x2, x2, :lo12:_ebss
.Lclear_bss:
    cmp     x1, x2
    b.ge    .Lbss_done
    str     xzr, [x1], #8
    b       .Lclear_bss
.Lbss_done:

    // Restore FDT pointer (PA) and call Rust entry point.
    // x19 was saved by _start before page table setup.
    mov     x0, x19
    bl      _start_rust

    // Should not return — halt if it does
.Lhang:
    wfe
    b       .Lhang
