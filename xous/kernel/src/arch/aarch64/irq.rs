// SPDX-FileCopyrightText: 2024 BeetOS contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! AArch64 interrupt handling for the Xous kernel.
//!
//! This module handles exception dispatch from the vector table (asm.S).
//! It is PLATFORM-GENERIC — the actual interrupt controller (GIC, AIC, etc.)
//! is accessed through the platform module.

use xous::arch::irq::IrqNumber;

/// Enable a specific interrupt via the platform's interrupt controller.
#[allow(dead_code)]
pub fn enable_irq(_irq_no: IrqNumber) {
    #[cfg(feature = "platform-qemu-virt")]
    crate::platform::qemu_virt::gic::enable_irq(_irq_no as u32);

    #[cfg(feature = "platform-apple-t8103")]
    { /* TODO(M3): AIC enable */ }
}

/// Disable a specific interrupt via the platform's interrupt controller.
#[allow(dead_code)]
pub fn disable_irq(_irq_no: IrqNumber) {
    #[cfg(feature = "platform-qemu-virt")]
    crate::platform::qemu_virt::gic::disable_irq(_irq_no as u32);

    #[cfg(feature = "platform-apple-t8103")]
    { /* TODO(M3): AIC disable */ }
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
    // Report the first IRQ from EL0 as proof that userspace is running
    #[cfg(feature = "platform-qemu-virt")]
    {
        use core::sync::atomic::{AtomicBool, Ordering};
        static FIRST_EL0_IRQ: AtomicBool = AtomicBool::new(true);
        if FIRST_EL0_IRQ.swap(false, Ordering::Relaxed) {
            crate::platform::qemu_virt::uart::puts(
                "\n*** SUCCESS: first IRQ from EL0! User process is running in its own address space. ***\n"
            );
        }
    }
    handle_irq();
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
unsafe extern "C" fn _kernel_irq_handler_rust(context: *mut u8) {
    handle_irq();

    // Clear PSTATE.I in saved SPSR so interrupts stay enabled after eret.
    // Without this, eret restores the SPSR with DAIF.I set (masked on entry).
    let spsr_ptr = context.add(264) as *mut u64;
    let spsr = core::ptr::read_volatile(spsr_ptr);
    core::ptr::write_volatile(spsr_ptr, spsr & !(1 << 7));
}

/// Platform-generic IRQ dispatch.
fn handle_irq() {
    #[cfg(feature = "platform-qemu-virt")]
    {
        use crate::platform::qemu_virt::{gic, timer, uart};

        let irq = gic::ack_irq();
        if irq == gic::IRQ_SPURIOUS {
            return;
        }

        match irq {
            timer::TIMER_IRQ => {
                timer::handle_tick();
            }
            uart::UART_IRQ => {
                // Read all pending characters and feed to shell
                while let Some(c) = uart::try_getc() {
                    crate::shell::process_char(c);
                }
                uart::clear_rx_interrupt();
            }
            irq_id => {
                use core::fmt::Write;
                let _ = write!(uart::UartWriter, "IRQ {}\n", irq_id);
            }
        }

        gic::eoi(irq);
    }

    #[cfg(feature = "platform-apple-t8103")]
    { /* TODO(M3): AIC dispatch */ }
}

/// Handle an SVC (syscall) from userspace.
///
/// The context pointer points to the saved register frame on the kernel stack
/// (matches the Thread register layout from asm.S's save_context).
/// We extract syscall args, dispatch through the Xous kernel, and write
/// the result directly into the saved frame so ERET returns it to userspace.
unsafe fn _handle_svc(context: *mut u8, _iss: u64) {
    use super::process::{Process, Thread};
    use xous::{Result as XousResult, SysCall};

    let frame = &mut *(context as *mut Thread);
    let args = frame.get_args();

    // Parse the raw register values into a typed SysCall enum.
    // args[0] = syscall number, args[1..7] = arguments
    let call = match SysCall::from_args(
        args[0], args[1], args[2], args[3],
        args[4], args[5], args[6], args[7],
    ) {
        Ok(call) => call,
        Err(_e) => {
            let result = XousResult::Error(xous::Error::InvalidSyscall);
            frame.set_args(&result.to_args());
            return;
        }
    };

    let proc = Process::current();
    let tid = proc.current_tid();

    // Log the first N syscalls for debugging, then go quiet.
    #[cfg(feature = "platform-qemu-virt")]
    {
        use core::sync::atomic::{AtomicU32, Ordering};
        static SVC_COUNT: AtomicU32 = AtomicU32::new(0);
        let n = SVC_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
        if n <= 10 {
            use core::fmt::Write;
            let _ = write!(
                crate::platform::qemu_virt::uart::UartWriter,
                "SVC[{}]: PID {} TID {} {:?}\n",
                n, crate::arch::process::current_pid(), tid, call,
            );
        }
    }

    // Dispatch the syscall through the Xous kernel.
    //
    // Syscalls like Yield and SendMessage set the thread result via
    // SystemServices::set_thread_result() (writes to PROCESS_TABLE) and
    // return ResumeProcess. Other syscalls (GetProcessId, etc.) return
    // the result directly.
    let response = crate::syscall::handle(tid, call)
        .unwrap_or_else(XousResult::Error);

    if response == XousResult::ResumeProcess {
        // The kernel wrote the result to PROCESS_TABLE.threads[tid].
        // Copy it to the stack frame so restore_context picks it up.
        let result_args = proc.thread(tid).get_args();
        frame.set_args(&result_args);
    } else {
        // Simple syscall — write the result directly to the stack frame.
        frame.set_args(&response.to_args());
    }

    #[cfg(feature = "platform-qemu-virt")]
    {
        use core::sync::atomic::{AtomicU32, Ordering};
        static SVC_RESULT_COUNT: AtomicU32 = AtomicU32::new(0);
        let n = SVC_RESULT_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
        if n <= 10 {
            use core::fmt::Write;
            let _ = write!(
                crate::platform::qemu_virt::uart::UartWriter,
                "  => {:?}\n", response,
            );
        }
    }
}

/// Handle a data or instruction abort.
unsafe fn _handle_abort(_context: *mut u8, _esr: u64, _is_instruction: bool) {
    let far: u64;
    core::arch::asm!("mrs {}, far_el1", out(reg) far, options(nomem, nostack));

    // TODO: Determine fault type from ISS:
    //   - Translation fault → allocate page (demand paging)
    //   - Permission fault → check if COW, else kill process
    //   - Alignment fault → kill process
    let _ = far;
}

/// Handle an unknown exception type.
unsafe fn _handle_unknown(_context: *mut u8, _esr: u64) {
    // TODO: Log the exception and terminate the process
}
