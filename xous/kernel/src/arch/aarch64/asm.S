// SPDX-FileCopyrightText: 2024 BeetOS contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

// AArch64 exception vectors, context save/restore for the Xous kernel.
// Kernel runs at EL1, userspace at EL0.

.section .text.vectors, "ax"
.balign 0x800

// ============================================================================
// Exception Vector Table (VBAR_EL1)
// ============================================================================
// Each entry is 128 bytes (32 instructions). 16 entries total.
// Layout:
//   0x000: Current EL with SP0 — Synchronous
//   0x080: Current EL with SP0 — IRQ
//   0x100: Current EL with SP0 — FIQ
//   0x180: Current EL with SP0 — SError
//   0x200: Current EL with SPx — Synchronous
//   0x280: Current EL with SPx — IRQ
//   0x300: Current EL with SPx — FIQ
//   0x380: Current EL with SPx — SError
//   0x400: Lower EL using AArch64 — Synchronous (SVC from EL0)
//   0x480: Lower EL using AArch64 — IRQ
//   0x500: Lower EL using AArch64 — FIQ
//   0x580: Lower EL using AArch64 — SError
//   0x600: Lower EL using AArch32 — Synchronous
//   0x680: Lower EL using AArch32 — IRQ
//   0x700: Lower EL using AArch32 — FIQ
//   0x780: Lower EL using AArch32 — SError

.globl _exception_vectors
_exception_vectors:

// --- Current EL with SP0 (should not happen — we use SPx) ---
.balign 0x80
_el1_sp0_sync:
    b       _unhandled_exception
.balign 0x80
_el1_sp0_irq:
    b       _unhandled_exception
.balign 0x80
_el1_sp0_fiq:
    b       _unhandled_exception
.balign 0x80
_el1_sp0_serror:
    b       _unhandled_exception

// --- Current EL with SPx (kernel exceptions) ---
.balign 0x80
_el1_spx_sync:
    b       _kernel_sync_handler
.balign 0x80
_el1_spx_irq:
    b       _kernel_irq_handler
.balign 0x80
_el1_spx_fiq:
    b       _unhandled_exception
.balign 0x80
_el1_spx_serror:
    b       _unhandled_exception

// --- Lower EL using AArch64 (user exceptions — the main path) ---
.balign 0x80
_el0_aarch64_sync:
    b       _user_sync_entry
.balign 0x80
_el0_aarch64_irq:
    b       _user_irq_entry
.balign 0x80
_el0_aarch64_fiq:
    b       _unhandled_exception
.balign 0x80
_el0_aarch64_serror:
    b       _unhandled_exception

// --- Lower EL using AArch32 (not supported) ---
.balign 0x80
_el0_aarch32_sync:
    b       _unhandled_exception
.balign 0x80
_el0_aarch32_irq:
    b       _unhandled_exception
.balign 0x80
_el0_aarch32_fiq:
    b       _unhandled_exception
.balign 0x80
_el0_aarch32_serror:
    b       _unhandled_exception


// ============================================================================
// Context save macro
// ============================================================================
// Saves all general-purpose registers, SP_EL0, ELR_EL1, SPSR_EL1, TPIDR_EL0
// to the stack. Uses 34 * 8 = 272 bytes of stack space for GPRs + specials,
// plus 32 * 16 = 512 bytes for NEON/FP registers = 784 bytes total.
// We round up to 800 for alignment.
//
// Stack layout after save (from SP):
//   [0..248]    X0-X30 (31 regs × 8 bytes)
//   [248]       SP_EL0
//   [256]       ELR_EL1 (return PC)
//   [264]       SPSR_EL1
//   [272]       TPIDR_EL0 (thread pointer)
//   [280]       FPCR
//   [288]       FPSR
//   [296]       padding
//   [304..816]  V0-V31 (32 regs × 16 bytes)

.macro save_context
    // Allocate stack frame
    sub     sp, sp, #816

    // Save X0-X29
    stp     x0,  x1,  [sp, #0]
    stp     x2,  x3,  [sp, #16]
    stp     x4,  x5,  [sp, #32]
    stp     x6,  x7,  [sp, #48]
    stp     x8,  x9,  [sp, #64]
    stp     x10, x11, [sp, #80]
    stp     x12, x13, [sp, #96]
    stp     x14, x15, [sp, #112]
    stp     x16, x17, [sp, #128]
    stp     x18, x19, [sp, #144]
    stp     x20, x21, [sp, #160]
    stp     x22, x23, [sp, #176]
    stp     x24, x25, [sp, #192]
    stp     x26, x27, [sp, #208]
    stp     x28, x29, [sp, #224]

    // Save X30 (LR)
    str     x30, [sp, #240]

    // Save SP_EL0, ELR_EL1, SPSR_EL1, TPIDR_EL0
    mrs     x0, sp_el0
    mrs     x1, elr_el1
    mrs     x2, spsr_el1
    mrs     x3, tpidr_el0
    stp     x0, x1, [sp, #248]
    stp     x2, x3, [sp, #264]

    // Save FPCR, FPSR
    mrs     x0, fpcr
    mrs     x1, fpsr
    stp     x0, x1, [sp, #280]

    // Save V0-V31
    stp     q0,  q1,  [sp, #304]
    stp     q2,  q3,  [sp, #336]
    stp     q4,  q5,  [sp, #368]
    stp     q6,  q7,  [sp, #400]
    stp     q8,  q9,  [sp, #432]
    stp     q10, q11, [sp, #464]
    stp     q12, q13, [sp, #496]
    stp     q14, q15, [sp, #528]
    stp     q16, q17, [sp, #560]
    stp     q18, q19, [sp, #592]
    stp     q20, q21, [sp, #624]
    stp     q22, q23, [sp, #656]
    stp     q24, q25, [sp, #688]
    stp     q26, q27, [sp, #720]
    stp     q28, q29, [sp, #752]
    stp     q30, q31, [sp, #784]
.endm

// ============================================================================
// Context restore macro
// ============================================================================
.macro restore_context
    // Restore V0-V31
    ldp     q0,  q1,  [sp, #304]
    ldp     q2,  q3,  [sp, #336]
    ldp     q4,  q5,  [sp, #368]
    ldp     q6,  q7,  [sp, #400]
    ldp     q8,  q9,  [sp, #432]
    ldp     q10, q11, [sp, #464]
    ldp     q12, q13, [sp, #496]
    ldp     q14, q15, [sp, #528]
    ldp     q16, q17, [sp, #560]
    ldp     q18, q19, [sp, #592]
    ldp     q20, q21, [sp, #624]
    ldp     q22, q23, [sp, #656]
    ldp     q24, q25, [sp, #688]
    ldp     q26, q27, [sp, #720]
    ldp     q28, q29, [sp, #752]
    ldp     q30, q31, [sp, #784]

    // Restore FPCR, FPSR
    ldp     x0, x1, [sp, #280]
    msr     fpcr, x0
    msr     fpsr, x1

    // Restore SP_EL0, ELR_EL1, SPSR_EL1, TPIDR_EL0
    ldp     x0, x1, [sp, #248]
    ldp     x2, x3, [sp, #264]
    msr     sp_el0, x0
    msr     elr_el1, x1
    msr     spsr_el1, x2
    msr     tpidr_el0, x3

    // Restore X30
    ldr     x30, [sp, #240]

    // Restore X0-X29
    ldp     x0,  x1,  [sp, #0]
    ldp     x2,  x3,  [sp, #16]
    ldp     x4,  x5,  [sp, #32]
    ldp     x6,  x7,  [sp, #48]
    ldp     x8,  x9,  [sp, #64]
    ldp     x10, x11, [sp, #80]
    ldp     x12, x13, [sp, #96]
    ldp     x14, x15, [sp, #112]
    ldp     x16, x17, [sp, #128]
    ldp     x18, x19, [sp, #144]
    ldp     x20, x21, [sp, #160]
    ldp     x22, x23, [sp, #176]
    ldp     x24, x25, [sp, #192]
    ldp     x26, x27, [sp, #208]
    ldp     x28, x29, [sp, #224]

    // Deallocate stack frame
    add     sp, sp, #816

    // Return to EL0 (or EL1 if that's where we came from)
    eret
.endm


// ============================================================================
// User synchronous exception handler (SVC from EL0, data/instruction aborts)
// ============================================================================
.globl _user_sync_entry
_user_sync_entry:
    save_context

    // Pass saved context pointer as first argument
    mov     x0, sp
    bl      _user_sync_handler_rust

    restore_context


// ============================================================================
// User IRQ handler
// ============================================================================
.globl _user_irq_entry
_user_irq_entry:
    save_context

    mov     x0, sp
    bl      _user_irq_handler_rust

    restore_context


// ============================================================================
// Kernel synchronous exception handler
// ============================================================================
.globl _kernel_sync_handler
_kernel_sync_handler:
    save_context

    mov     x0, sp
    bl      _kernel_sync_handler_rust

    restore_context


// ============================================================================
// Kernel IRQ handler
// ============================================================================
.globl _kernel_irq_handler
_kernel_irq_handler:
    save_context

    mov     x0, sp
    bl      _kernel_irq_handler_rust

    restore_context


// ============================================================================
// Resume a saved context (called from Rust)
// ============================================================================
// extern "C" fn _resume_context(context: *const u8) -> !
// The context pointer points to the same layout as saved by save_context.
.globl _resume_context
_resume_context:
    mov     sp, x0
    restore_context


// ============================================================================
// Unhandled exception — spin forever
// ============================================================================
_unhandled_exception:
    wfe
    b       _unhandled_exception
