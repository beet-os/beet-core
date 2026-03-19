// SPDX-FileCopyrightText: 2024 BeetOS contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Minimal BeetOS user process.
//!
//! Proves the full pipeline: Rust source → ELF → load_elf → MMU-isolated
//! process → syscalls via xous-rs → preemptive scheduling.

#![no_std]
#![no_main]

use core::panic::PanicInfo;

/// Entry point. Called by the kernel via ERET to EL0.
///
/// Creates a server, then yields in a loop forever.
/// The timer will preempt this process even if it stops yielding.
#[no_mangle]
pub extern "C" fn _start() -> ! {
    // GetProcessId — proves xous-rs syscall path works
    let _pid = xous::rsyscall(xous::SysCall::GetProcessId);

    // CreateServerWithAddress — proves we can register services
    let sid = xous::SID::from_array([0xBEE7_0001, 0, 0, 0]);
    let _server = xous::rsyscall(xous::SysCall::CreateServerWithAddress(sid, 0..0));

    // Yield loop — cooperative scheduling (timer preemption also works)
    loop {
        xous::yield_slice();
    }
}

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    loop {
        // WFE to save power while halted
        unsafe { core::arch::asm!("wfe", options(nomem, nostack)) };
    }
}
