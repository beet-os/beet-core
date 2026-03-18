// SPDX-FileCopyrightText: 2024 BeetOS contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! PL011 UART driver for QEMU virt platform.
//!
//! Reference: ARM PL011 Technical Reference Manual (DDI 0183).
//! QEMU virt places the first UART at 0x0900_0000.

use core::fmt;

/// PL011 register offsets.
mod regs {
    /// Data Register — write to transmit, read to receive.
    pub const DR: usize = 0x00;
    /// Flag Register — TX/RX status bits.
    pub const FR: usize = 0x18;
    /// Control Register — enable UART, TX, RX.
    pub const CR: usize = 0x30;
    /// Interrupt Mask Set/Clear Register.
    pub const IMSC: usize = 0x38;
    /// Interrupt Clear Register.
    pub const ICR: usize = 0x44;

    // Flag Register bits
    /// TX FIFO full.
    pub const FR_TXFF: u32 = 1 << 5;
    /// RX FIFO empty.
    pub const FR_RXFE: u32 = 1 << 4;

    // Control Register bits
    /// UART enable.
    pub const CR_UARTEN: u32 = 1 << 0;
    /// TX enable.
    pub const CR_TXEN: u32 = 1 << 8;
    /// RX enable.
    pub const CR_RXEN: u32 = 1 << 9;

    // Interrupt bits
    /// RX interrupt.
    pub const IMSC_RXIM: u32 = 1 << 4;
}

/// UART base address. Set at runtime from FDT (or QEMU virt default).
static mut UART_BASE: usize = 0;

/// Read a PL011 register.
///
/// # Safety
/// UART_BASE must be a valid MMIO address and the UART must be mapped.
#[inline]
unsafe fn read_reg(offset: usize) -> u32 {
    core::ptr::read_volatile((UART_BASE + offset) as *const u32)
}

/// Write a PL011 register.
///
/// # Safety
/// UART_BASE must be a valid MMIO address and the UART must be mapped.
#[inline]
unsafe fn write_reg(offset: usize, val: u32) {
    core::ptr::write_volatile((UART_BASE + offset) as *mut u32, val);
}

/// Initialize the PL011 UART.
///
/// On QEMU, the UART is already configured by the firmware/QEMU itself,
/// so we just need to ensure TX is enabled. No baud rate setup needed.
pub fn init(base: usize) {
    unsafe {
        UART_BASE = base;
        // Enable UART, TX, and RX
        write_reg(regs::CR, regs::CR_UARTEN | regs::CR_TXEN | regs::CR_RXEN);
        // Clear all pending interrupts
        write_reg(regs::ICR, 0x7FF);
    }
}

/// Enable RX interrupt for character input.
pub fn enable_rx_interrupt() {
    unsafe {
        let imsc = read_reg(regs::IMSC);
        write_reg(regs::IMSC, imsc | regs::IMSC_RXIM);
    }
    // Enable UART IRQ in the GIC
    super::gic::enable_irq(UART_IRQ);
}

/// Write a single byte to the UART (polled).
pub fn putc(c: u8) {
    unsafe {
        if UART_BASE == 0 {
            return;
        }
        // Wait until TX FIFO has space
        while read_reg(regs::FR) & regs::FR_TXFF != 0 {}
        write_reg(regs::DR, c as u32);
    }
}

/// Write a string to the UART.
pub fn puts(s: &str) {
    for b in s.bytes() {
        if b == b'\n' {
            putc(b'\r');
        }
        putc(b);
    }
}

/// Try to read a byte from the UART (non-blocking).
/// Returns `None` if the RX FIFO is empty.
#[allow(dead_code)]
pub fn try_getc() -> Option<u8> {
    unsafe {
        if UART_BASE == 0 {
            return None;
        }
        if read_reg(regs::FR) & regs::FR_RXFE != 0 {
            None
        } else {
            Some((read_reg(regs::DR) & 0xFF) as u8)
        }
    }
}

/// GIC IRQ number for UART0 on QEMU virt (SPI 1 = INTID 33).
pub const UART_IRQ: u32 = 33;

/// Clear the RX interrupt (call after reading all pending characters).
pub fn clear_rx_interrupt() {
    unsafe {
        write_reg(regs::ICR, regs::IMSC_RXIM);
    }
}

/// fmt::Write implementation for the UART, enabling `write!()` / `writeln!()`.
pub struct UartWriter;

impl fmt::Write for UartWriter {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        puts(s);
        Ok(())
    }
}
