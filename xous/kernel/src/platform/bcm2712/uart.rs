// SPDX-FileCopyrightText: 2024 BeetOS contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! PL011 UART driver for Raspberry Pi 5 (BCM2712).
//!
//! The BCM2712 UART0 is a standard ARM PL011 — identical register layout to
//! the QEMU virt UART. Only the base address and IRQ number differ.
//!
//! Default address (from RPi5 device tree): 0x107D001000
//!
//! Note: the RPi5 also exposes UARTs via the RP1 south bridge chip (PCIe).
//! This driver uses the native BCM2712 PL011 (UART0), which is available
//! without PCIe initialization and is suitable for early boot output.
//!
//! To use UART0 for serial console on RPi5, add to config.txt:
//!   enable_uart=1
//!   dtoverlay=disable-bt   (moves UART0 away from Bluetooth, onto GPIO 14/15)

use core::fmt;

mod regs {
    pub const DR: usize = 0x00;
    pub const FR: usize = 0x18;
    pub const CR: usize = 0x30;
    pub const IMSC: usize = 0x38;
    pub const ICR: usize = 0x44;
    pub const FR_TXFF: u32 = 1 << 5;
    pub const FR_RXFE: u32 = 1 << 4;
    pub const CR_UARTEN: u32 = 1 << 0;
    pub const CR_TXEN: u32 = 1 << 8;
    pub const CR_RXEN: u32 = 1 << 9;
    pub const IMSC_RXIM: u32 = 1 << 4;
}

static mut UART_BASE: usize = 0;

#[inline]
unsafe fn read_reg(offset: usize) -> u32 {
    core::ptr::read_volatile((UART_BASE + offset) as *const u32)
}

#[inline]
unsafe fn write_reg(offset: usize, val: u32) {
    core::ptr::write_volatile((UART_BASE + offset) as *mut u32, val);
}

pub fn init(base: usize) {
    unsafe {
        UART_BASE = base;
        write_reg(regs::CR, regs::CR_UARTEN | regs::CR_TXEN | regs::CR_RXEN);
        write_reg(regs::ICR, 0x7FF);
    }
}

/// Enable RX interrupt (requires knowing the correct GIC IRQ from FDT).
/// Currently a no-op — polled input only until IRQ number is verified.
#[allow(dead_code)]
pub fn enable_rx_interrupt() {
    // TODO(M3): verify UART_IRQ from RPi5 FDT and enable via GIC
    // unsafe {
    //     let imsc = read_reg(regs::IMSC);
    //     write_reg(regs::IMSC, imsc | regs::IMSC_RXIM);
    // }
    // super::gic::enable_irq(UART_IRQ);
}

pub fn putc(c: u8) {
    unsafe {
        if UART_BASE == 0 {
            return;
        }
        while read_reg(regs::FR) & regs::FR_TXFF != 0 {}
        write_reg(regs::DR, c as u32);
    }
}

pub fn puts(s: &str) {
    for b in s.bytes() {
        if b == b'\n' {
            putc(b'\r');
        }
        putc(b);
    }
}

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

/// GIC SPI for UART0 on BCM2712.
/// Verify from FDT: `uart0 { interrupts = <GIC_SPI N ...> }` → INTID = N + 32.
/// Placeholder until confirmed from real hardware FDT dump.
pub const UART_IRQ: u32 = 153; // SPI 121 = INTID 153 (tentative)

#[allow(dead_code)]
pub fn clear_rx_interrupt() {
    unsafe {
        write_reg(regs::ICR, regs::IMSC_RXIM);
    }
}

pub struct UartWriter;

impl fmt::Write for UartWriter {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        puts(s);
        Ok(())
    }
}
