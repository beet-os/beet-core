// SPDX-FileCopyrightText: 2024 BeetOS contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! AArch64 architecture backend for the Xous kernel.
//!
//! Runs at EL1 on Apple Silicon (T8103). Userspace runs at EL0.
//! Uses 16KB pages (Apple Silicon granule) with 3-level page tables
//! (L1→L2→L3) and TTBR0/TTBR1 split for user/kernel address spaces.

pub mod backtrace;
pub mod elf;
pub mod irq;
pub mod mem;
pub mod panic;
pub mod process;
pub mod rand;
pub mod syscall;

mod asm;

use core::arch::asm;

/// Read the current hardware PID from CONTEXTIDR_EL1.
/// The lower 32 bits hold our process context identifier.
#[inline]
fn current_hw_pid() -> u32 {
    let val: u64;
    unsafe { asm!("mrs {}, contextidr_el1", out(reg) val, options(nomem, nostack)) };
    val as u32
}

/// Architecture-specific initialization.
/// Called once from `init()` after memory and services are set up.
pub fn init() {
    // Set initial CONTEXTIDR_EL1 to PID 1
    unsafe {
        asm!(
            "msr contextidr_el1, {val}",
            "isb",
            val = in(reg) 1u64,
            options(nomem, nostack),
        );
    }
}

/// Main idle loop. Called repeatedly from `kmain()`.
///
/// On hardware, this processes pending interrupts and performs context
/// switches. Returns `true` to continue running, `false` to shut down.
pub fn idle() -> bool {
    // Wait for interrupt (low-power idle)
    unsafe { asm!("wfe", options(nomem, nostack)) };
    // TODO(M2): Check for pending work, dispatch IRQs, schedule processes
    true
}
