// SPDX-FileCopyrightText: 2024 BeetOS contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! BeetOS log server.
//!
//! Receives stdout/stderr/panic messages via Xous IPC and forwards them
//! to the UART and framebuffer. Implements the `xous-log-server` SID that
//! the Rust std PAL expects (see library/std/src/os/beetos/services/log.rs).
//!
//! Protocol (opcodes match xous-api-log):
//!   Borrow(1)      StandardOutput    — write buf[..valid] to UART + FB
//!   Borrow(2)      StandardError     — write buf[..valid] to UART + FB
//!   Scalar(1000)   BeginPanic        — print "PANIC: "
//!   Scalar(1100+N) AppendPanicMessage — print N bytes packed in 4 args
//!   Scalar(1200)   PanicFinished     — print "\n"

#![no_std]
#![no_main]

use core::panic::PanicInfo;

use beetos::fb_console::FbConsole;

// ============================================================================
// UART output (mapped by kernel at SHELL_UART_VA before ERET)
// ============================================================================

const UART_DR: usize = 0x00;
const UART_FR: usize = 0x18;
const UART_FR_TXFF: u32 = 1 << 5;

static mut UART_BASE: usize = 0;

// FB dimensions — must match the kernel constants in platform/qemu_virt/fb.rs.
const FB_WIDTH:  usize = 1280;
const FB_HEIGHT: usize = 800;

static mut FB_CONSOLE: Option<FbConsole> = None;

fn uart_putc(c: u8) {
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

fn putc(c: u8) {
    uart_putc(c);
    unsafe {
        if let Some(ref mut con) = FB_CONSOLE {
            con.putc(c);
        }
    }
}

/// Read the cursor from the shared cursor page and apply it to FB_CONSOLE.
/// Call before writing a message to FB so we continue from where the shell left off.
unsafe fn sync_cursor_from_shared() {
    if let Some(ref mut con) = FB_CONSOLE {
        let ptr = beetos::SHARED_CURSOR_VA as *const u32;
        let row = core::ptr::read_volatile(ptr) as usize;
        let col = core::ptr::read_volatile(ptr.add(1)) as usize;
        con.set_cursor(row, col);
    }
}

/// Write the current FB_CONSOLE cursor back to the shared cursor page.
/// Call after finishing a write so the shell picks up the new position.
unsafe fn sync_cursor_to_shared() {
    if let Some(ref con) = FB_CONSOLE {
        let (row, col) = con.cursor();
        let ptr = beetos::SHARED_CURSOR_VA as *mut u32;
        core::ptr::write_volatile(ptr, row as u32);
        core::ptr::write_volatile(ptr.add(1), col as u32);
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

/// Write to UART only — used for the log server's own startup messages
/// so they don't interfere with the shell's cursor on the framebuffer.
fn uart_puts(s: &str) {
    for b in s.bytes() {
        uart_putc(b);
    }
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
pub extern "C" fn _start(uart_base: usize, fb_base: usize) -> ! {
    // x0 = UART MMIO base VA, x1 = framebuffer base VA (0 if not available).
    // Set by kernel in launch_first_process before ERET.
    unsafe {
        UART_BASE = uart_base;
        if fb_base != 0 {
            FB_CONSOLE = Some(FbConsole::new(
                fb_base as *mut u32,
                FB_WIDTH, FB_HEIGHT, FB_WIDTH,
            ));
            // Cursor starts at (0,0); will be synced from the shared page
            // before each IPC message is written to the FB.
        }
    }

    // Startup messages go to UART only — writing to FB here would collide
    // with the shell's "bsh> " prompt that was printed at the same position.
    uart_puts("[log] starting\n");

    let sid = xous::SID::from_array(LOG_SID);
    let _server = xous::rsyscall(xous::SysCall::CreateServerWithAddress(sid, 0..0));

    uart_puts("[log] registered xous-log-server\n");

    loop {
        let msg = xous::rsyscall(xous::SysCall::ReceiveMessage(sid));

        match msg {
            Ok(xous::Result::MessageEnvelope(env)) => {
                // Sync cursor from shared page before writing so we start
                // at the right position (after shell or previous app output).
                unsafe { sync_cursor_from_shared(); }

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

                // Sync updated cursor back so the shell (or next process) starts
                // writing after our output.
                unsafe { sync_cursor_to_shared(); }
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
