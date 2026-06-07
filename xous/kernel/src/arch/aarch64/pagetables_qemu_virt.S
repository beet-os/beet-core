// SPDX-FileCopyrightText: 2025 BeetOS contributors
// SPDX-License-Identifier: MIT OR Apache-2.0
//
// Platform-specific boot pagetable fill for the QEMU virt machine.
//
// QEMU virt memory map (relevant subset):
//   PA 0x0000_0000..0x4000_0000  — MMIO (UART, GIC, virtio, …)
//   PA 0x4000_0000..0x8000_0000  — RAM (kernel + initial heap)
//
// We map the low 2 GiB through one L2 table (no L2_high needed) and
// preserve the historic split: device blocks for the MMIO half,
// normal blocks for the RAM half.
//
// L2 block descriptor bits (32 MiB blocks):
//   Device: VALID | AttrIdx=0 | SH=ISH | AF | UXN | PXN = 0x0060_0000_0000_0701
//   Normal: VALID | AttrIdx=1 | SH=ISH | AF | UXN       = 0x0040_0000_0000_0705
//           (No PXN — kernel executes from RAM after MMU enable)

.section .text.boot, "ax"
.globl _fill_boot_tables
// x0 = _boot_l1_ttbr0, x1 = _boot_l1_ttbr1,
// x2 = _boot_l2_low,   x3 = _boot_l2_high (unused here)
_fill_boot_tables:
    stp     x29, x30, [sp, #-16]!

    // --- Fill L2_low ---
    // x2 already points at _boot_l2_low; preserve a stable alias.
    mov     x10, x2

    // Device memory attrs (PA 0..0x4000_0000).
    movz    x4, #0x0701
    movk    x4, #0x0060, lsl #48

    // L2[0..31] = PA 0..0x3E00_0000 as device.
    mov     x5, #0
.Lqemu_mmio:
    cmp     x5, #32
    b.ge    .Lqemu_ram_setup

    lsl     x6, x5, #25            // PA = index × 32 MiB
    orr     x6, x6, x4             // PA | device attrs
    str     x6, [x10, x5, lsl #3]

    add     x5, x5, #1
    b       .Lqemu_mmio

.Lqemu_ram_setup:
    // Normal memory attrs (PA 0x4000_0000..0x8000_0000), executable.
    movz    x4, #0x0705
    movk    x4, #0x0040, lsl #48

    // L2[32..63] = PA 0x4000_0000..0x7E00_0000 as normal.
    mov     x5, #32
.Lqemu_ram:
    cmp     x5, #64
    b.ge    .Lqemu_done

    lsl     x6, x5, #25
    orr     x6, x6, x4
    str     x6, [x10, x5, lsl #3]

    add     x5, x5, #1
    b       .Lqemu_ram

.Lqemu_done:
    // --- Wire L1[0] → L2_low in both TTBR0 and TTBR1 ---
    // Table descriptor: L2_PA | VALID | TABLE (= L2_PA | 0x3).
    orr     x7, x2, #0x3
    str     x7, [x0]                // TTBR0 L1[0] → L2_low
    str     x7, [x1]                // TTBR1 L1[0] → L2_low

    // L2_high is unused on QEMU virt — left zero. (`_boot_l2_high` is
    // still allocated by start.S; that's just 16 KiB of harmless .bss.)

    ldp     x29, x30, [sp], #16
    ret
