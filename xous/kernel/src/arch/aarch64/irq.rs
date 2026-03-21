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
///
/// This is the preemption entry point: if the timer fires while a user process
/// is running, we save its context, handle the IRQ, then let the scheduler
/// pick the next process to run (which may be a different one).
#[no_mangle]
unsafe extern "C" fn _user_irq_handler_rust(context: *mut u8) {
    use super::process::{Process, Thread};

    let frame = context as *mut Thread;

    // Save the interrupted process's register state into PROCESS_TABLE.
    let interrupted_pid = crate::arch::process::current_pid();
    let proc = Process::current();
    let tid = proc.current_tid();
    proc.save_context_to_table(tid, frame);

    // Handle the hardware interrupt (timer, UART, etc.)
    let was_timer = handle_irq();

    // If the timer fired, rotate the scheduler and possibly switch processes.
    if was_timer {
        crate::services::SystemServices::with_mut(|ss| {
            crate::scheduler::Scheduler::with_mut(|s| {
                let prio = ss.current_process().thread_priority(tid);
                s.yield_thread(interrupted_pid, tid, prio);
                let _ = s.activate_current(ss);
            });
        });
    }

    // Load the (potentially new) process's context into the stack frame.
    let resume_proc = Process::current();
    let resume_tid = resume_proc.current_tid();
    resume_proc.load_context_from_table(resume_tid, frame);

    // Log the first preemptive switch
    #[cfg(feature = "platform-qemu-virt")]
    if was_timer {
        let resume_pid = crate::arch::process::current_pid();
        if resume_pid != interrupted_pid {
            use core::sync::atomic::{AtomicBool, Ordering};
            static FIRST_PREEMPT: AtomicBool = AtomicBool::new(true);
            if FIRST_PREEMPT.swap(false, Ordering::Relaxed) {
                use core::fmt::Write;
                let _ = write!(
                    crate::platform::qemu_virt::uart::UartWriter,
                    "PREEMPT: timer switched PID {} -> {}\n",
                    interrupted_pid, resume_pid,
                );
            }
        }
    }
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
/// No preemption — kernel mode always returns to the same context.
#[no_mangle]
unsafe extern "C" fn _kernel_irq_handler_rust(context: *mut u8) {
    let _ = handle_irq();

    // Clear PSTATE.I in saved SPSR so interrupts stay enabled after eret.
    // Without this, eret restores the SPSR with DAIF.I set (masked on entry).
    let spsr_ptr = context.add(264) as *mut u64;
    let spsr = core::ptr::read_volatile(spsr_ptr);
    core::ptr::write_volatile(spsr_ptr, spsr & !(1 << 7));
}

/// Platform-generic IRQ dispatch. Returns true if the IRQ was a timer tick
/// (used by the caller to decide whether to invoke the scheduler).
fn handle_irq() -> bool {
    #[cfg(feature = "platform-qemu-virt")]
    {
        use crate::platform::qemu_virt::{blk, gic, timer, uart};

        let irq = gic::ack_irq();
        if irq == gic::IRQ_SPURIOUS {
            return false;
        }

        let is_timer = irq == timer::TIMER_IRQ;

        match irq {
            timer::TIMER_IRQ => {
                timer::handle_tick();
            }
            uart::UART_IRQ => {
                // Read all pending characters and send to the shell process via IPC
                while let Some(c) = uart::try_getc() {
                    send_char_to_console(c);
                }
                uart::clear_rx_interrupt();
            }
            irq_id if blk::irq_number() == Some(irq_id) => {
                blk::handle_irq();
            }
            irq_id => {
                use core::fmt::Write;
                let _ = write!(uart::UartWriter, "IRQ {}\n", irq_id);
            }
        }

        gic::eoi(irq);
        return is_timer;
    }

    #[cfg(feature = "platform-bcm2712")]
    {
        use crate::platform::bcm2712::{gic, timer};

        let irq = gic::ack_irq();
        if irq == gic::IRQ_SPURIOUS {
            return false;
        }

        let is_timer = irq == timer::TIMER_IRQ;

        match irq {
            timer::TIMER_IRQ => { timer::handle_tick(); }
            irq_id => {
                use core::fmt::Write;
                let _ = write!(crate::platform::bcm2712::uart::UartWriter, "IRQ {}\n", irq_id);
            }
        }

        gic::eoi(irq);
        return is_timer;
    }

    #[cfg(feature = "platform-apple-t8103")]
    { /* TODO(M3b): AIC dispatch */ false }
}

/// Send a received UART character to the console/shell server via IPC.
///
/// Called from IRQ context. Finds the console server by its well-known SID,
/// and delivers a Scalar message containing the character. If the server
/// thread is blocked in ReceiveMessage, it is woken up.
#[cfg(feature = "platform-qemu-virt")]
fn send_char_to_console(c: u8) {
    use crate::services::SystemServices;
    use xous::{Message, ScalarMessage, SID};

    let console_sid = SID::from_array(beetos_api_console::CONSOLE_SID);

    SystemServices::with_mut(|ss| {
        // Find the console server. It may not exist yet during early boot.
        let sidx = match ss.servers.iter().position(|s| {
            s.as_ref().is_some_and(|s| s.sid == console_sid)
        }) {
            Some(idx) => idx,
            None => return, // Console server not registered yet
        };

        let server = match ss.server_from_sidx_mut(sidx) {
            Some(s) => s,
            None => return,
        };

        let server_pid = server.pid;

        // Create a Scalar message with the char
        let msg = Message::Scalar(ScalarMessage {
            id: beetos_api_console::ConsoleOp::Char as usize,
            arg1: c as usize,
            arg2: 0,
            arg3: 0,
            arg4: 0,
        });

        // If the server has a thread parked in ReceiveMessage, deliver directly
        if let Some(server_tid) = server.take_available_thread() {
            let sender = xous::MessageSender::from_usize(0); // kernel sender
            let envelope = xous::MessageEnvelope { sender, body: msg };

            ss.process_mut(server_pid)
                .map(|p| p.set_thread_state(server_tid, crate::process::ThreadState::Ready))
                .ok();
            ss.set_thread_result(server_pid, server_tid, xous::Result::MessageEnvelope(envelope))
                .ok();
        } else {
            // Queue the message for later pickup
            let kernel_pid = xous::PID::new(1).unwrap();
            ss.queue_server_message(sidx, kernel_pid, 1, msg, None).ok();
        }
    });
}

/// Handle an SVC (syscall) from userspace.
///
/// The context pointer points to the saved register frame on the kernel stack
/// (matches the Thread register layout from asm.S's save_context).
///
/// Flow for context switches:
///   1. Save stack frame → PROCESS_TABLE[old_pid][old_tid]
///   2. Dispatch syscall (may call activate_current → switch CURRENT_PID + TTBR0)
///   3. Load PROCESS_TABLE[new_pid][new_tid] → stack frame
///   4. restore_context → ERET to the correct process
unsafe fn _handle_svc(context: *mut u8, _iss: u64) {
    use super::process::{Process, Thread};
    use xous::{Result as XousResult, SysCall};

    let frame = context as *mut Thread;

    // Capture the calling process before any context switch.
    let caller_pid = crate::arch::process::current_pid();
    let mut caller_proc = Process::current();
    let caller_tid = caller_proc.current_tid();

    // Step 1: Save the stack frame into PROCESS_TABLE so the kernel has
    // the caller's full register state if a context switch happens.
    caller_proc.save_context_to_table(caller_tid, frame);

    let args = (*frame).get_args();

    // Debug: log all syscalls from PID 6 (hello-std) to diagnose hang
    #[cfg(feature = "platform-qemu-virt")]
    if caller_pid.get() == 6 {
        use core::fmt::Write;
        let _ = write!(
            crate::platform::qemu_virt::uart::UartWriter,
            "[SVC] pid={} x0={} x1={:#x}\n",
            caller_pid.get(), args[0], args[1],
        );
    }

    // Parse the raw register values into a typed SysCall enum.
    let call = match SysCall::from_args(
        args[0], args[1], args[2], args[3],
        args[4], args[5], args[6], args[7],
    ) {
        Ok(call) => call,
        Err(_e) => {
            #[cfg(feature = "platform-qemu-virt")]
            {
                use core::fmt::Write;
                let _ = write!(
                    crate::platform::qemu_virt::uart::UartWriter,
                    "[SVC] InvalidSyscall a0={} pid={}\n",
                    args[0], caller_pid.get(),
                );
            }
            let result = XousResult::Error(xous::Error::InvalidSyscall);
            (*frame).set_args(&result.to_args());
            return;
        }
    };

    // Step 2: Dispatch the syscall through the Xous kernel.
    // This may change CURRENT_PID and TTBR0 via activate_current().
    let response = crate::syscall::handle(caller_tid, call)
        .unwrap_or_else(XousResult::Error);

    // For simple syscalls that return a value directly (not ResumeProcess),
    // write the result into the caller's PROCESS_TABLE entry.
    let is_resume = response == XousResult::ResumeProcess;
    if !is_resume {
        caller_proc.set_thread_result(caller_tid, response);
    }

    // Step 3: Load the *current* process's context into the stack frame.
    // After a context switch, CURRENT_PID may differ from caller_pid.
    // This loads the correct process's registers for ERET.
    let resume_proc = Process::current();
    let resume_tid = resume_proc.current_tid();
    resume_proc.load_context_from_table(resume_tid, frame);

}

/// Handle a data or instruction abort.
///
/// Instead of freezing the entire system (which happens if we loop with wfe
/// while IRQs are masked), we terminate the faulting process and let the
/// scheduler pick the next runnable process.
unsafe fn _handle_abort(context: *mut u8, esr: u64, is_instruction: bool) {
    use super::process::{Process, Thread};

    let far: u64;
    core::arch::asm!("mrs {}, far_el1", out(reg) far, options(nomem, nostack));

    let elr: u64;
    core::arch::asm!("mrs {}, elr_el1", out(reg) elr, options(nomem, nostack));

    let pid = crate::arch::process::current_pid();
    let abort_type = if is_instruction { "IABT" } else { "DABT" };
    let iss = esr & 0x01FF_FFFF;
    let dfsc = iss & 0x3F; // Data/Instruction Fault Status Code

    #[cfg(feature = "platform-qemu-virt")]
    {
        use core::fmt::Write;
        let _ = write!(
            crate::platform::qemu_virt::uart::UartWriter,
            "ABORT: PID {} {} at PC={:#x} FAR={:#x} ESR={:#x} DFSC={:#x}\n",
            pid, abort_type, elr, far, esr, dfsc,
        );
    }

    // Terminate the faulting process and switch to the next runnable one.
    // This prevents a single user fault from freezing the entire system.
    let frame = context as *mut Thread;
    let caller_proc = Process::current();
    let caller_tid = caller_proc.current_tid();
    caller_proc.save_context_to_table(caller_tid, frame);

    let _ = crate::services::SystemServices::with_mut(|ss| {
        ss.terminate_current_process(1)
    });

    // Load the next process's context so restore_context + ERET runs it.
    let resume_proc = Process::current();
    let resume_tid = resume_proc.current_tid();
    resume_proc.load_context_from_table(resume_tid, frame);
}

/// Handle an unknown exception type.
unsafe fn _handle_unknown(_context: *mut u8, esr: u64) {
    let pid = crate::arch::process::current_pid();

    #[cfg(feature = "platform-qemu-virt")]
    {
        use core::fmt::Write;
        let _ = write!(
            crate::platform::qemu_virt::uart::UartWriter,
            "UNKNOWN EXCEPTION: PID {} ESR={:#x}\n",
            pid, esr,
        );
    }

    loop {
        core::arch::asm!("wfe", options(nomem, nostack));
    }
}
