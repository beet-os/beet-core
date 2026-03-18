// SPDX-FileCopyrightText: 2024 BeetOS contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! AArch64 panic handler for the Xous kernel.
//!
//! On panic, prints the message to the debug console and halts.

use core::fmt::Write;
use core::panic::PanicInfo;

/// Kernel panic handler. Prints diagnostic info and halts.
#[cfg(not(test))]
#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    // Disable interrupts
    unsafe { core::arch::asm!("msr daifset, #0xf", options(nomem, nostack)) };

    // Write directly to platform UART (println! may not work on bare metal)
    #[cfg(feature = "platform-qemu-virt")]
    {
        let mut w = crate::platform::qemu_virt::uart::UartWriter;
        let _ = write!(w, "\n!!! KERNEL PANIC !!!\n");
        if let Some(location) = info.location() {
            let _ = write!(w, "  at {}:{}:{}\n", location.file(), location.line(), location.column());
        }
        let _ = write!(w, "  {}\n", info.message());
    }

    // Halt: infinite WFE loop
    loop {
        unsafe { core::arch::asm!("wfe", options(nomem, nostack)) };
    }
}
