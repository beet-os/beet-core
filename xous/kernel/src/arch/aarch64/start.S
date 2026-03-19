// SPDX-FileCopyrightText: 2024 BeetOS contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

// AArch64 kernel entry point — TTBR1 kernel mapping.
//
// Boot sequence:
//   1. _start runs at PA with MMU off (loaded by QEMU -kernel)
//   2. Drop from EL2 to EL1 if needed, enable FP/SIMD
//   3. Create bootstrap page tables:
//      - TTBR0: identity map (PA → PA) for boot transition
//      - TTBR1: linear map (PA + 0xFFFF_8000_0000_0000 → PA) for kernel
//   4. Enable MMU
//   5. Jump to _high_va_entry at high VA (through TTBR1)
//   6. Set SP, VBAR, clear BSS, call _start_rust — all at high VA
//
// After step 5, ALL kernel code runs through TTBR1. TTBR0 is free
// for user process page tables and changes on every context switch.

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
// Creates:
//   TTBR0 identity map: PA 0..2GB → VA 0..2GB (for boot transition)
//   TTBR1 kernel map:   PA 0..2GB → VA 0xFFFF_8000_0000_0000..+2GB
//
// Uses pre-allocated tables in .boot.bss:
//   _boot_l1_ttbr0 (16KB): L1 table for TTBR0
//   _boot_l1_ttbr1 (16KB): L1 table for TTBR1
//   _boot_l2       (16KB): L2 table shared by both (same PA→PA mapping)
//
// For 16KB granule:
//   L1 entry covers 64 GiB (2048 entries × 32 MiB per L2 block)
//   L2 block covers 32 MiB
//   We map 64 L2 blocks = 2 GiB (MMIO 0..1GB + RAM 1..2GB)

_create_boot_page_tables:
    stp     x29, x30, [sp, #-16]!

    // Zero all three page tables (3 × 16KB = 48KB = 6144 u64s)
    adr     x0, _boot_l1_ttbr0
    mov     x1, #(3 * 16384 / 8)
.Lzero_tables:
    str     xzr, [x0], #8
    subs    x1, x1, #1
    b.ne    .Lzero_tables

    // --- Fill L2 with block descriptors ---
    adr     x10, _boot_l2

    // Device memory block attributes (MMIO: PA 0..0x4000_0000)
    // bits: VALID(0) | AttrIdx=0(device) | SH=ISH(9:8) | AF(10) | UXN(54) | PXN(53)
    // = 0x0060_0000_0000_0701
    movz    x2, #0x0701
    movk    x2, #0x0060, lsl #48

    // Map MMIO blocks: L2[0..31] = PA 0..0x3E00_0000 (32 × 32MB = 1GB)
    mov     x3, #0                  // block index
.Lmap_mmio:
    cmp     x3, #32
    b.ge    .Lmap_ram

    lsl     x5, x3, #25            // PA = index × 32MB
    orr     x5, x5, x2             // PA | device attrs
    str     x5, [x10, x3, lsl #3]  // L2[index] = descriptor

    add     x3, x3, #1
    b       .Lmap_mmio

.Lmap_ram:
    // Normal memory block attributes (RAM: PA 0x4000_0000..0x8000_0000)
    // bits: VALID(0) | AttrIdx=1(normal,2) | SH=ISH(9:8) | AF(10) | UXN(54)
    // No PXN — kernel can execute from RAM.
    // = 0x0040_0000_0000_0705
    movz    x2, #0x0705
    movk    x2, #0x0040, lsl #48

    // Map RAM blocks: L2[32..63] = PA 0x4000_0000..0x7E00_0000 (32 × 32MB = 1GB)
    mov     x3, #32
.Lmap_ram_loop:
    cmp     x3, #64
    b.ge    .Lmap_done

    lsl     x5, x3, #25            // PA = index × 32MB
    orr     x5, x5, x2             // PA | normal attrs
    str     x5, [x10, x3, lsl #3]  // L2[index] = descriptor

    add     x3, x3, #1
    b       .Lmap_ram_loop

.Lmap_done:
    // --- Wire L1[0] → L2 in both TTBR0 and TTBR1 ---
    // L1 table descriptor: L2_PA | VALID(1) | TABLE(2) = L2_PA | 0x3
    adr     x0, _boot_l2
    orr     x0, x0, #0x3           // table descriptor

    adr     x5, _boot_l1_ttbr0
    str     x0, [x5]               // TTBR0 L1[0] → L2

    adr     x5, _boot_l1_ttbr1
    str     x0, [x5]               // TTBR1 L1[0] → L2

    ldp     x29, x30, [sp], #16
    ret

// ============================================================================
// Enable MMU (runs at PA, called from _start)
// ============================================================================

_enable_mmu_boot:
    // MAIR_EL1: memory attribute indirection register
    //   Attr0 = 0x00 (Device-nGnRnE)
    //   Attr1 = 0xFF (Normal WB, RA, WA, inner+outer)
    //   Attr2 = 0x44 (Normal non-cacheable)
    movz    x0, #0xFF00
    movk    x0, #0x0044, lsl #16
    msr     mair_el1, x0

    // TCR_EL1: translation control register
    //   T0SZ=17, T1SZ=17 (47-bit VA per half)
    //   TG0=0b10 (16KB granule), TG1=0b01 (16KB granule)
    //   IPS=0b101 (48-bit PA)
    //   SH0=SH1=0b11 (inner shareable)
    //   ORGN0=IRGN0=ORGN1=IRGN1=0b01 (WB RA WA)
    // Pre-computed: 0x0000_0005_7511_B511
    ldr     x0, =0x57511B511
    msr     tcr_el1, x0

    // TTBR0_EL1: identity map (boot only, replaced by user page tables later)
    adr     x0, _boot_l1_ttbr0
    msr     ttbr0_el1, x0

    // TTBR1_EL1: kernel linear map (permanent, never changed)
    adr     x0, _boot_l1_ttbr1
    msr     ttbr1_el1, x0

    // Ensure page table writes are visible
    dsb     ish
    isb

    // Invalidate all TLB entries
    tlbi    vmalle1is
    dsb     ish
    isb

    // Enable MMU
    // SCTLR_EL1: M(0) | C(2) | SA(3) | I(12) | UCI(26)
    // WXN(19) = 0: don't enforce W→XN for EL1
    movz    x0, #0x100D             // bits 0,2,3,12
    movk    x0, #0x0400, lsl #16   // bit 26
    msr     sctlr_el1, x0
    isb

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

.globl _boot_l2
_boot_l2:
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
