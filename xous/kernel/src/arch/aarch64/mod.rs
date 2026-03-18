// SPDX-FileCopyrightText: 2024 BeetOS contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! AArch64 architecture backend for the Xous kernel.
//!
//! Runs at EL1 on any AArch64 platform. Userspace runs at EL0.
//! Uses 16KB pages with 4-level page tables and TTBR0/TTBR1 split
//! for user/kernel address spaces.
//!
//! This module is PLATFORM-GENERIC — no hardware-specific code here.
//! Platform-specific drivers (GIC, AIC, UART, etc.) live in platform/.

pub mod backtrace;
#[allow(dead_code)]
pub mod elf;
pub mod irq;
pub mod mem;
pub mod panic;
pub mod process;
#[allow(dead_code)]
pub mod rand;
#[allow(dead_code)]
pub mod syscall;

#[allow(dead_code)]
mod asm;

use core::arch::asm;

// Include assembly files via global_asm! (no cc crate needed).
// Combined into a single global_asm! to avoid duplicate symbol issues
// across codegen units.
core::arch::global_asm!(
    include_str!("start.S"),
    include_str!("asm.S"),
);

/// Read the current hardware PID from CONTEXTIDR_EL1.
/// The lower 32 bits hold our process context identifier.
#[allow(dead_code)]
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
    unsafe { asm!("wfi", options(nomem, nostack)) };
    // TODO: Check for pending work, dispatch IRQs, schedule processes
    true
}
