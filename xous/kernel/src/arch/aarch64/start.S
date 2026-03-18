// SPDX-FileCopyrightText: 2024 BeetOS contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

// AArch64 kernel entry point.
//
// Called by QEMU (-kernel) or bootloader with:
//   x0 = FDT pointer (or KernelArguments pointer)
//   MMU off, caches off, interrupts masked
//   Running at EL1 (QEMU starts at EL1 with -machine virt,virtualization=off)

.section .text.boot, "ax"
.globl _start
_start:
    // Mask all exceptions (DAIF)
    msr     daifset, #0xf

    // Enable FP/SIMD at EL1 (CPACR_EL1.FPEN = 0b11)
    // Without this, any FP/NEON instruction (including compiler-generated
    // memcpy/memset using NEON) will trap with an Undefined Instruction.
    mov     x1, #(3 << 20)
    msr     cpacr_el1, x1
    isb

    // Set up the exception vector table
    adrp    x1, _exception_vectors
    add     x1, x1, :lo12:_exception_vectors
    msr     vbar_el1, x1
    isb

    // Set up kernel stack pointer
    adrp    x1, _stack_top
    add     x1, x1, :lo12:_stack_top
    mov     sp, x1

    // Clear BSS section
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

    // x0 still has FDT/args pointer from bootloader
    // Jump to Rust entry point
    bl      _start_rust

    // Should not return — halt if it does
.Lhang:
    wfe
    b       .Lhang
