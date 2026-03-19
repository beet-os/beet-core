// SPDX-FileCopyrightText: 2024 BeetOS contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! BeetOS "hello" process.
//!
//! When used as "idle" (PID 2): yields in a loop forever, absorbing CPU
//! when no other process is ready.
//!
//! When spawned by name as "hello": prints a greeting with its PID,
//! then exits cleanly via TerminateProcess(0).
//!
//! The kernel determines the behavior by checking the process name.
//! Both cases use the same binary — the name is set during create_process.

#![no_std]
#![no_main]

use core::panic::PanicInfo;

// ============================================================================
// UART output
// ============================================================================

const UART_DR: usize = 0x00;
const UART_FR: usize = 0x18;
const UART_FR_TXFF: u32 = 1 << 5;

static mut UART_BASE: usize = 0;

fn putc(c: u8) {
    unsafe {
        if UART_BASE == 0 {
            return;
        }
        let base = UART_BASE;
        while (core::ptr::read_volatile((base + UART_FR) as *const u32) & UART_FR_TXFF) != 0 {}
        if c == b'\n' {
            core::ptr::write_volatile((base + UART_DR) as *mut u32, b'\r' as u32);
            while (core::ptr::read_volatile((base + UART_FR) as *const u32) & UART_FR_TXFF) != 0 {}
        }
        core::ptr::write_volatile((base + UART_DR) as *mut u32, c as u32);
    }
}

fn puts(s: &str) {
    for b in s.bytes() {
        putc(b);
    }
}

fn put_usize(mut n: usize) {
    if n == 0 {
        putc(b'0');
        return;
    }
    let mut buf = [0u8; 20];
    let mut i = 0;
    while n > 0 {
        buf[i] = b'0' + (n % 10) as u8;
        n /= 10;
        i += 1;
    }
    while i > 0 {
        i -= 1;
        putc(buf[i]);
    }
}

// ============================================================================
// Entry point
// ============================================================================

#[no_mangle]
pub extern "C" fn _start() -> ! {
    // x0 = UART MMIO base VA (set by kernel before ERET)
    let uart_base: usize;
    unsafe {
        core::arch::asm!("mov {}, x0", out(reg) uart_base, options(nomem, nostack));
        UART_BASE = uart_base;
    }

    // Get our PID
    let pid = match xous::rsyscall(xous::SysCall::GetProcessId) {
        Ok(xous::Result::ProcessID(pid)) => pid.get() as usize,
        _ => 0,
    };

    // If we're PID 2 (idle), just yield forever
    if pid == 2 {
        loop {
            xous::yield_slice();
        }
    }

    // Otherwise, we were spawned as "hello" — print greeting and exit
    puts("Hello, BeetOS!\n");
    puts("I am PID ");
    put_usize(pid);
    puts(", running at EL0.\n");

    // Clean exit
    xous::terminate_process(0);
}

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    puts("PANIC in hello!\n");
    loop {
        unsafe { core::arch::asm!("wfe", options(nomem, nostack)) };
    }
}
