// SPDX-FileCopyrightText: 2024 BeetOS contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Raspberry Pi 5 (BCM2712) platform support for BeetOS.
//!
//! BCM2712 (Cortex-A76, GIC-600) peripheral map as seen from ARM cores:
//!   0x107D001000  UART0 (PL011, native BCM2712)
//!   0x107FFD0000  GIC Redistributors (GICR, 4 × 128KB)
//!   0x107FFF9000  GIC Distributor (GICD)
//!
//! RAM starts at 0x0. Peripherals are above 64 GiB (different L1 region).
//!
//! Boot chain: RPi5 firmware (start.elf) loads kernel8.img at 0x80000,
//! jumps to _start at EL2. Our start.S drops to EL1 before calling Rust.

pub mod gic;
pub mod timer;
pub mod uart;

mod defaults {
    /// UART0 (PL011) on BCM2712, ARM physical address.
    pub const UART0_BASE: usize = 0x107D001000;
    /// GIC Distributor.
    pub const GICD_BASE: usize = 0x107FFF9000;
    /// GIC Redistributor (CPU0).
    pub const GICR_BASE: usize = 0x107FFD0000;
}

/// Initialize the BCM2712 platform.
///
/// Uses hardcoded defaults from the RPi5 device tree. FDT parsing for
/// dynamic address discovery is a future improvement.
pub fn init() {
    uart::init(defaults::UART0_BASE);
    uart::puts("BeetOS v0.1.0\n");
    uart::puts("Platform: Raspberry Pi 5 (BCM2712 / AArch64)\n");

    gic::init(defaults::GICD_BASE, defaults::GICR_BASE);
    uart::puts("GIC: initialized\n");

    timer::init();
    uart::puts("Timer: initialized\n");
}

pub fn shutdown() -> ! {
    uart::puts("System halted.\n");
    loop {
        unsafe { core::arch::asm!("wfi", options(nomem, nostack)) };
    }
}

pub mod rand {
    pub fn get_u32() -> u32 {
        crate::arch::rand::get_u32()
    }
}

pub mod cache {
    #[allow(dead_code)]
    pub fn clean_cache() {}
    #[allow(dead_code)]
    pub fn clean_cache_l1() {}
    #[allow(dead_code)]
    pub fn clean_cache_l2() {}
    #[allow(dead_code)]
    pub fn print_cache_stats() {}
}

pub mod wdt {
    #[allow(dead_code)]
    pub fn restart() {}
}
