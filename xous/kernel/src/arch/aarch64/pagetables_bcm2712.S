// SPDX-FileCopyrightText: 2025 BeetOS contributors
// SPDX-License-Identifier: MIT OR Apache-2.0
//
// Platform-specific boot pagetable fill for the Raspberry Pi 5
// (BCM2712).
//
// BCM2712 memory map (ARM physical addresses):
//   PA 0x00_0000_0000..0x02_0000_0000   — RAM (up to 8 GiB on 8 GiB SKU)
//   PA 0x10_7C00_0000..0x10_8000_0000   — peripherals (UART0, GIC,
//                                          mailbox, RP1, SDHCI, …)
//
// Examples:
//   UART0 PL011                0x10_7D00_1000
//   GIC distributor (GICD)     0x10_7FFF_9000
//   GIC redistributor (GICR)   0x10_7FFD_0000
//   Mailbox property iface     0x10_7C01_3880
//
// The kernel image loads at PA 0x00_0008_0000 (RPi firmware drops
// kernel8.img at +512 KiB), well inside the low RAM range. The
// peripheral region lives at ~66 GiB — same L1 entry (L1[1]) but on
// the *other* side of the 64 GiB stride from RAM — so we need *two*
// L2 tables in the bootstrap:
//
//   L2_low  → 4 GiB of low RAM mapped as Normal (covers the kernel
//             image and plenty of room for early allocations).
//   L2_high → 4 GiB of device memory at PA 0x10_0000_0000+, covering
//             every BCM2712 peripheral the kernel touches in early
//             boot.
//
// Wiring:
//   TTBR0 L1[0] → L2_low         (identity map, used for the MMU-on
//                                  transition then dropped)
//   TTBR1 L1[0] → L2_low         (kernel low half, permanent)
//   TTBR1 L1[1] → L2_high        (kernel MMIO half, permanent)
//
// L1 index for the high range:
//   0x10_7D00_1000 / 64 GiB = 1.something  →  L1[1] covers
//   PA 0x10_0000_0000 .. 0x20_0000_0000.

.section .text.boot, "ax"
.globl _fill_boot_tables
// x0 = _boot_l1_ttbr0, x1 = _boot_l1_ttbr1,
// x2 = _boot_l2_low,   x3 = _boot_l2_high
_fill_boot_tables:
    stp     x29, x30, [sp, #-16]!

    // --- Fill L2_low: 128 normal blocks → 128 × 32 MiB = 4 GiB RAM ---
    // Normal attrs (executable kernel, cacheable, inner-shareable).
    //   VALID | AttrIdx=1 | SH=ISH | AF | UXN  = 0x0040_0000_0000_0705
    movz    x4, #0x0705
    movk    x4, #0x0040, lsl #48

    mov     x10, x2                // x10 = L2_low base
    mov     x5, #0                 // block index
.Lbcm_low:
    cmp     x5, #128
    b.ge    .Lbcm_low_done

    lsl     x6, x5, #25            // PA = index × 32 MiB
    orr     x6, x6, x4
    str     x6, [x10, x5, lsl #3]

    add     x5, x5, #1
    b       .Lbcm_low

.Lbcm_low_done:
    // --- Fill L2_high: 128 device blocks → 128 × 32 MiB = 4 GiB MMIO ---
    // Device attrs (uncacheable, strongly ordered, exec-never).
    //   VALID | AttrIdx=0 | SH=ISH | AF | UXN | PXN = 0x0060_0000_0000_0701
    movz    x4, #0x0701
    movk    x4, #0x0060, lsl #48

    // Base PA covered by L2_high under L1[1]:
    //   L1[1] = 1 × 64 GiB = 0x10_0000_0000.
    // Each L2 block is 32 MiB, so block N maps base + N × 32 MiB.
    // Build 0x10_0000_0000 = 1 << 36 in x11.
    mov     x11, #1
    lsl     x11, x11, #36

    mov     x10, x3                // x10 = L2_high base
    mov     x5, #0
.Lbcm_high:
    cmp     x5, #128
    b.ge    .Lbcm_high_done

    lsl     x6, x5, #25            // offset within L1[1] = index × 32 MiB
    add     x6, x6, x11            // PA = 0x10_0000_0000 + offset
    orr     x6, x6, x4             // PA | device attrs
    str     x6, [x10, x5, lsl #3]

    add     x5, x5, #1
    b       .Lbcm_high

.Lbcm_high_done:
    // --- Wire the L1 entries ---
    // L2_low → L1[0] in both TTBR0 and TTBR1.
    orr     x7, x2, #0x3           // L2_low | VALID | TABLE
    str     x7, [x0]               // TTBR0 L1[0] (identity)
    str     x7, [x1]               // TTBR1 L1[0] (kernel low)

    // L2_high → L1[1] in TTBR1 only.
    // Offset = 1 × 8 bytes = 8 from the base of the L1 table.
    orr     x7, x3, #0x3           // L2_high | VALID | TABLE
    str     x7, [x1, #8]           // TTBR1 L1[1]

    ldp     x29, x30, [sp], #16
    ret
