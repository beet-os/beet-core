// SPDX-FileCopyrightText: 2024 BeetOS contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! AArch64 interrupt handling for the Xous kernel.
//!
//! On Apple Silicon, interrupts are managed by the Apple Interrupt Controller (AIC).
//! Full AIC driver implementation comes in M2. This module provides the kernel-side
//! interrupt dispatch and the enable/disable API.

use xous::arch::irq::IrqNumber;

/// Enable a specific interrupt in the AIC.
#[allow(dead_code)]
pub fn enable_irq(_irq_no: IrqNumber) {
    // TODO(M2): Write to AIC mask set register
}

/// Disable a specific interrupt in the AIC.
#[allow(dead_code)]
pub fn disable_irq(_irq_no: IrqNumber) {
    // TODO(M2): Write to AIC mask clear register
}

/// Called from asm.S when an SVC exception occurs from EL0.
/// Reads ESR_EL1 to determine the exception class and dispatches accordingly.
#[no_mangle]
unsafe extern "C" fn _user_sync_handler_rust(context: *mut u8) {
    let esr: u64;
    core::arch::asm!("mrs {}, esr_el1", out(reg) esr, options(nomem, nostack));

    let ec = (esr >> 26) & 0x3F; // Exception Class
    let iss = esr & 0x01FF_FFFF; // Instruction Specific Syndrome

    match ec {
        0x15 => {
            // SVC from AArch64 — this is a Xous syscall
            _handle_svc(context, iss);
        }
        0x20 | 0x21 => {
            // Instruction Abort from lower EL (0x20) or current EL (0x21)
            _handle_abort(context, esr, true);
        }
        0x24 | 0x25 => {
            // Data Abort from lower EL (0x24) or current EL (0x25)
            _handle_abort(context, esr, false);
        }
        _ => {
            // Unknown exception — crash the process
            _handle_unknown(context, esr);
        }
    }
}

/// Called from asm.S when an IRQ occurs from EL0.
#[no_mangle]
unsafe extern "C" fn _user_irq_handler_rust(_context: *mut u8) {
    // TODO(M2): Read AIC event register to get IRQ number
    // TODO(M2): Dispatch to registered IRQ handler
    // For now, just return (the eret in asm.S will resume the process)
}

/// Called from asm.S for kernel-mode synchronous exceptions.
#[no_mangle]
unsafe extern "C" fn _kernel_sync_handler_rust(_context: *mut u8) {
    let esr: u64;
    core::arch::asm!("mrs {}, esr_el1", out(reg) esr, options(nomem, nostack));
    // Kernel synchronous exceptions are unexpected — halt
    panic!("Kernel sync exception: ESR_EL1 = {:#018x}", esr);
}

/// Called from asm.S for kernel-mode IRQs.
#[no_mangle]
unsafe extern "C" fn _kernel_irq_handler_rust(_context: *mut u8) {
    // TODO(M2): Handle kernel-mode IRQs (timer tick, etc.)
}

/// Handle an SVC (syscall) from userspace.
unsafe fn _handle_svc(_context: *mut u8, _iss: u64) {
    // The ISS field contains the SVC immediate value (we use SVC #0).
    // Syscall arguments are in X0-X5, X8-X9 (saved in context).
    // TODO(M2): Extract args from context, dispatch via crate::syscall::handle,
    // write result back to context registers.
}

/// Handle a data or instruction abort.
unsafe fn _handle_abort(_context: *mut u8, _esr: u64, _is_instruction: bool) {
    let far: u64;
    core::arch::asm!("mrs {}, far_el1", out(reg) far, options(nomem, nostack));

    // TODO(M2): Determine fault type from ISS:
    //   - Translation fault → allocate page (demand paging)
    //   - Permission fault → check if COW, else kill process
    //   - Alignment fault → kill process
    let _ = far;
}

/// Handle an unknown exception type.
unsafe fn _handle_unknown(_context: *mut u8, _esr: u64) {
    // TODO(M2): Log the exception and terminate the process
}
