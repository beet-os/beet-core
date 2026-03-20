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
    // Read boot parameters from registers BEFORE any function call or syscall.
    // The kernel sets x0=uart_va, x1=argv_ptr, x2=argv_len before ERET.
    // Any syscall (including GetProcessId) will clobber x0-x7.
    let uart_base: usize;
    let argv_ptr: usize;
    let argv_len: usize;
    unsafe {
        core::arch::asm!(
            "mov {0}, x0",
            "mov {1}, x1",
            "mov {2}, x2",
            out(reg) uart_base,
            out(reg) argv_ptr,
            out(reg) argv_len,
            options(nomem, nostack),
        );
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

    // Display argv if present
    if argv_ptr != 0 && argv_len > 0 {
        let argv_data = unsafe { core::slice::from_raw_parts(argv_ptr as *const u8, argv_len) };
        puts("argv: [");
        let mut first = true;
        for arg in argv_data.split(|&b| b == 0) {
            if arg.is_empty() { continue; }
            if !first { puts(", "); }
            first = false;
            puts("\"");
            if let Ok(s) = core::str::from_utf8(arg) {
                puts(s);
            } else {
                puts("<invalid utf8>");
            }
            puts("\"");
        }
        puts("]\n");
    }

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
