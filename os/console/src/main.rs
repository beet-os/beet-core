// SPDX-FileCopyrightText: 2025 BeetOS contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! BeetOS console output service.
//!
//! Owns "where does stdout go". Registers under [`CONSOLE_OUT_SID`] and
//! receives [`ConsoleOp::Write`] / [`ConsoleOp::Putc`] from any process
//! that wants its output mirrored. Today it fans out to two sinks:
//!
//!   1. **UART (PL011)** — same MMIO page the kernel maps into every
//!      service. Existing local behaviour: serial console + QEMU
//!      stdout.
//!   2. **TCP remote console** — pushes bytes into the kernel's
//!      [`SysCall::NetConsolePush`] ring. Drained into the next ACK
//!      whenever a client is connected on port 2323. No-op when no
//!      one's listening.
//!
//! ## Why a service?
//!
//! Before this crate existed, the shell wrote PL011 MMIO directly from
//! EL0. That's fast, but it means the kernel has zero visibility into
//! "what did the user just see on screen?" — and there's no way to tee
//! it to a second sink. Centralising stdout here lets the remote
//! console mirror local output without modifying every printer in the
//! tree (shell, fs, block, log all write through `beetos::pl011` —
//! they migrate one at a time).
//!
//! Phase 1: the shell sends a **tap copy** of every `puts()` to this
//! service in parallel with its existing direct UART/FB write. That
//! keeps the local terminal pixel-identical while still giving the
//! remote client a real view. Phase 2 will retire the direct write.

#![no_std]
#![no_main]

use core::panic::PanicInfo;

use beetos_api_console::{
    decode_write_id, pack_write, unpack_write, ConsoleOp, CONSOLE_OUT_SID, WRITE_CHUNK,
};

// ============================================================================
// UART output (mapped at SHELL_UART_VA by the kernel before our ERET)
// ============================================================================

const UART_DR: usize = 0x00;
const UART_FR: usize = 0x18;
const UART_FR_TXFF: u32 = 1 << 5;

static mut UART_BASE: usize = 0;

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

fn write_bytes(buf: &[u8]) {
    for &b in buf {
        uart_putc(b);
    }
    // Push the same bytes to the kernel's TCP TX ring in 32-byte
    // chunks, inline-packed (no user pointer crosses the syscall
    // boundary — that keeps the kernel handler PAN-safe).
    let mut i = 0;
    while i < buf.len() {
        let end = (i + WRITE_CHUNK).min(buf.len());
        let (args, len) = pack_write(&buf[i..end]);
        let _ = xous::rsyscall(xous::SysCall::NetConsolePush(
            len, args[0], args[1], args[2], args[3],
        ));
        i = end;
    }
}

/// Forward already-packed bytes (received as a `ConsoleOp::Write`
/// Scalar) straight into the kernel's TCP ring. Mirrors what
/// `write_bytes` does on its own pack, minus the unpacking round-trip.
fn forward_packed(len: usize, args: [usize; 4]) {
    // Local UART mirror: unpack and write byte-by-byte.
    let mut buf = [0u8; WRITE_CHUNK];
    unpack_write(args, &mut buf);
    for &b in &buf[..len.min(WRITE_CHUNK)] {
        uart_putc(b);
    }
    // TCP mirror: forward the original packed payload verbatim.
    let _ = xous::rsyscall(xous::SysCall::NetConsolePush(
        len.min(WRITE_CHUNK),
        args[0], args[1], args[2], args[3],
    ));
}

// ============================================================================
// Service entry point
// ============================================================================

#[no_mangle]
pub extern "C" fn _start() -> ! {
    let uart_base: usize;
    unsafe {
        core::arch::asm!(
            "mov {0}, x0",
            out(reg) uart_base,
            options(nomem, nostack),
        );
        UART_BASE = uart_base;
    }

    // Hello — proves the service started and the UART map is correct.
    write_bytes(b"[console] up\n");

    let sid = xous::SID::from_array(CONSOLE_OUT_SID);
    let _ = xous::rsyscall(xous::SysCall::CreateServerWithAddress(sid, 0..0));

    loop {
        let msg = xous::rsyscall(xous::SysCall::ReceiveMessage(sid));
        let env = match msg {
            Ok(xous::Result::MessageEnvelope(e)) => e,
            _ => {
                xous::yield_slice();
                continue;
            }
        };

        match env.body {
            xous::Message::Scalar(s) => {
                // Two opcodes share the Scalar shape: Putc (id = 2,
                // arg1 = byte) and Write (id low byte = 1, high byte
                // = length, args = packed bytes).
                if s.id == ConsoleOp::Putc as usize {
                    let b = s.arg1 as u8;
                    uart_putc(b);
                    let _ = xous::rsyscall(xous::SysCall::NetConsolePush(
                        1, b as usize, 0, 0, 0,
                    ));
                } else if let Some(len) = decode_write_id(s.id) {
                    forward_packed(len, [s.arg1, s.arg2, s.arg3, s.arg4]);
                }
            }
            _ => {
                // Ignore other shapes for now. Borrow/MutableBorrow
                // will land when long-string output moves off the
                // scalar fast-path.
            }
        }
    }
}

// ============================================================================
// Panic handler
// ============================================================================

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    // We can't reliably print here (the IPC machinery may be
    // mid-receive). Yield-and-spin keeps the process alive enough that
    // the kernel sees it stop replying — easier to diagnose than a
    // crashed PID.
    loop {
        xous::yield_slice();
    }
}
