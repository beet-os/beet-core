// SPDX-FileCopyrightText: 2024 BeetOS contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! AArch64 panic handler for the Xous kernel.
//!
//! On panic, prints the message to the debug console, captures a backtrace,
//! and halts the system.

use core::panic::PanicInfo;

/// Kernel panic handler. Prints diagnostic info and halts.
#[cfg(not(test))]
#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    // Disable interrupts
    unsafe { core::arch::asm!("msr daifset, #0xf", options(nomem, nostack)) };

    println!("!!! KERNEL PANIC !!!");
    if let Some(location) = info.location() {
        println!("  at {}:{}:{}", location.file(), location.line(), location.column());
    }
    if let Some(message) = info.message().as_str() {
        println!("  {}", message);
    }

    // Print backtrace
    super::backtrace::print_current_process_backtrace();

    // Halt: infinite WFE loop
    loop {
        unsafe { core::arch::asm!("wfe", options(nomem, nostack)) };
    }
}
