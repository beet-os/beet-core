// SPDX-FileCopyrightText: 2025 BeetOS contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Minimal PL011 UART driver for userspace services.
//!
//! The QEMU virt platform (and any other system that maps a PL011 at
//! a known MMIO address) lets userspace processes share the boot UART
//! by having the kernel map the MMIO page into the process and pass
//! the VA in x0. Every service that wants stdout-style output does the
//! same dance:
//!
//! ```rust,ignore
//! beetos::pl011::init(uart_base);     // once, at _start
//! beetos::pl011::puts("hello\n");     // anywhere after
//! write!(beetos::pl011::Writer, ...)  // for formatted output
//! ```
//!
//! Before this module existed each of shell / fs / block had its own
//! copy of the same 25-line PL011 driver. They all reuse this one now.
//!
//! Apple M1 hardware uses a different UART (S5L) and will get its own
//! sibling module; the public shape (`init` / `putc` / `puts` /
//! `Writer`) stays the same so callers don't care which platform they
//! are on.

use core::fmt;

const UART_DR:      usize = 0x00;
const UART_FR:      usize = 0x18;
const UART_FR_TXFF: u32   = 1 << 5;

static mut BASE: usize = 0;

/// Install the MMIO virtual address of the PL011 for this process.
/// Subsequent [`putc`] / [`puts`] / [`Writer`] calls use it.
///
/// Safe to call multiple times (later calls win); the typical pattern
/// is exactly one call at the top of `_start` with the value the
/// kernel placed in x0.
pub fn init(base: usize) {
    // SAFETY: BASE is a single `usize`; userspace services are
    // single-threaded (the kernel doesn't wire CreateThread yet), so
    // there's no race. Even if it were multi-threaded the worst
    // outcome of a torn write would be writing to a half-updated VA,
    // which produces garbage output, not memory corruption.
    unsafe { BASE = base; }
}

/// Write a single byte to the UART, translating `\n` to `\r\n` so the
/// host terminal renders newlines correctly. No-op if [`init`] was
/// never called.
pub fn putc(c: u8) {
    // SAFETY: BASE is either 0 (we early-out) or a kernel-mapped
    // PL011 MMIO page valid for the lifetime of the process.
    unsafe {
        if BASE == 0 { return; }
        let base = BASE;
        while (core::ptr::read_volatile((base + UART_FR) as *const u32) & UART_FR_TXFF) != 0 {}
        if c == b'\n' {
            core::ptr::write_volatile((base + UART_DR) as *mut u32, b'\r' as u32);
            while (core::ptr::read_volatile((base + UART_FR) as *const u32) & UART_FR_TXFF) != 0 {}
        }
        core::ptr::write_volatile((base + UART_DR) as *mut u32, c as u32);
    }
}

/// Write an entire string, one byte at a time via [`putc`].
pub fn puts(s: &str) {
    for b in s.bytes() { putc(b); }
}

/// Zero-sized [`core::fmt::Write`] adapter so `write!`/`writeln!`
/// macros can target the UART without an intermediate buffer.
pub struct Writer;

impl fmt::Write for Writer {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        puts(s);
        Ok(())
    }
}
