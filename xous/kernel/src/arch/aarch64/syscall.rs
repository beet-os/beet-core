// SPDX-FileCopyrightText: 2024 BeetOS contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! AArch64 syscall invocation for the Xous kernel.
//!
//! On hardware, syscalls arrive via SVC exceptions and are dispatched
//! in irq.rs. This module provides a function to invoke a syscall
//! programmatically (used by the kernel itself, e.g., for thread setup).

use super::process::Thread;

/// Invoke a syscall by setting up thread state.
/// Used by the kernel to inject syscalls into a thread's context.
pub fn invoke(
    context: &mut Thread,
    _supervisor: bool,
    pc: usize,
    sp: usize,
    ret_addr: usize,
    args: &[usize],
) {
    context.set_pc(pc);
    context.set_sp(sp);
    context.gpr[30] = ret_addr as u64; // X30 = LR
    context.set_args(args);

    if _supervisor {
        // SPSR: EL1h (kernel mode), interrupts masked
        context.spsr = 0x3C5;
    } else {
        // SPSR: EL0t (user mode), interrupts enabled
        context.spsr = 0;
    }
}
