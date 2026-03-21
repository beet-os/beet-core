// SPDX-FileCopyrightText: 2024 BeetOS contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! BeetOS log server.
//!
//! Receives stdout/stderr/panic messages via Xous IPC and forwards them
//! to the UART. Implements the `xous-log-server` SID that the Rust std
//! PAL expects (see library/std/src/os/beetos/services/log.rs).
//!
//! Protocol (opcodes match xous-api-log):
//!   Borrow(1)      StandardOutput    — write buf[..valid] to UART
//!   Borrow(2)      StandardError     — write buf[..valid] to UART
//!   Scalar(1000)   BeginPanic        — print "PANIC: "
//!   Scalar(1100+N) AppendPanicMessage — print N bytes packed in 4 args
//!   Scalar(1200)   PanicFinished     — print "\n"

#![no_std]
#![no_main]

use core::panic::PanicInfo;

// ============================================================================
// UART output (mapped by kernel at SHELL_UART_VA before ERET)
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

fn write_bytes(s: &[u8]) {
    for &b in s {
        putc(b);
    }
}

fn puts(s: &str) {
    write_bytes(s.as_bytes());
}

// ============================================================================
// Log server SID — must match b"xous-log-server " (16 bytes, trailing space)
// Same encoding as SID::from_bytes: u32::from_le_bytes per 4-byte chunk.
// ============================================================================

const LOG_SID: [u32; 4] = [
    u32::from_le_bytes(*b"xous"),
    u32::from_le_bytes(*b"-log"),
    u32::from_le_bytes(*b"-ser"),
    u32::from_le_bytes(*b"ver "),
];

const OPCODE_STDOUT: usize = 1;
const OPCODE_STDERR: usize = 2;
const OPCODE_BEGIN_PANIC: usize = 1000;
const OPCODE_PANIC_MSG_BASE: usize = 1100;
const OPCODE_PANIC_FINISHED: usize = 1200;

// ============================================================================
// Entry point
// ============================================================================

#[no_mangle]
pub extern "C" fn _start() -> ! {
    // x0 = UART MMIO base VA, set by kernel in launch_first_process before ERET.
    let uart_base: usize;

    unsafe {
        core::arch::asm!("mov {}, x0", out(reg) uart_base, options(nomem, nostack));
        UART_BASE = uart_base;
    }

    puts("[log] starting\n");

    let sid = xous::SID::from_array(LOG_SID);
    let _server = xous::rsyscall(xous::SysCall::CreateServerWithAddress(sid, 0..0));

    puts("[log] registered xous-log-server\n");

    loop {
        let msg = xous::rsyscall(xous::SysCall::ReceiveMessage(sid));

        match msg {
            Ok(xous::Result::MessageEnvelope(env)) => {
                match &env.body {
                    xous::Message::Borrow(mem) => {
                        let opcode = mem.id;
                        let valid_len =
                            mem.valid.map(|v| v.get()).unwrap_or(0).min(mem.buf.len());

                        if opcode == OPCODE_STDOUT || opcode == OPCODE_STDERR {
                            let buf = mem.buf.as_slice::<u8>();
                            write_bytes(&buf[..valid_len]);
                        }

                        // env drops here — Drop impl calls return_memory_offset_valid,
                        // which unblocks the sender's lend() call.
                    }

                    xous::Message::Scalar(scalar) | xous::Message::BlockingScalar(scalar) => {
                        let id = scalar.id;

                        if id == OPCODE_BEGIN_PANIC {
                            puts("\nPANIC: ");
                        } else if id > OPCODE_PANIC_MSG_BASE && id <= OPCODE_PANIC_FINISHED {
                            // AppendPanicMessage: id - base = number of bytes in this chunk.
                            // Bytes are packed little-endian into 4 usize args.
                            let n = id - OPCODE_PANIC_MSG_BASE;
                            let args =
                                [scalar.arg1, scalar.arg2, scalar.arg3, scalar.arg4];
                            let mut written = 0;

                            'outer: for arg in args {
                                for byte_idx in 0..core::mem::size_of::<usize>() {
                                    if written >= n {
                                        break 'outer;
                                    }
                                    putc((arg >> (byte_idx * 8)) as u8);
                                    written += 1;
                                }
                            }
                        } else if id == OPCODE_PANIC_FINISHED {
                            puts("\n");
                        }
                    }

                    _ => {}
                }
            }

            _ => {
                xous::yield_slice();
            }
        }
    }
}

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    puts("PANIC in log server!\n");
    loop {
        unsafe { core::arch::asm!("wfe", options(nomem, nostack)) };
    }
}
