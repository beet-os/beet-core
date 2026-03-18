// SPDX-FileCopyrightText: 2024 BeetOS contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! QEMU virt platform support for BeetOS.
//!
//! QEMU virt is a standard ARM virtual machine with well-documented
//! hardware: GICv3 interrupt controller, PL011 UART, ARM generic timer,
//! and virtio devices. It's the primary development and CI platform.
//!
//! Default QEMU virt memory map (from hw/arm/virt.c):
//!   0x0800_0000  GIC Distributor (GICD)
//!   0x080A_0000  GIC Redistributor (GICR)
//!   0x0900_0000  PL011 UART0
//!   0x0A00_0000  RTC (PL031)
//!   0x4000_0000  RAM base

pub mod gic;
pub mod timer;
pub mod uart;

/// Default MMIO addresses for QEMU virt (used when FDT parsing is not yet available).
/// These match QEMU's hw/arm/virt.c defaults.
mod defaults {
    pub const UART0_BASE: usize = 0x0900_0000;
    pub const GICD_BASE: usize = 0x0800_0000;
    pub const GICR_BASE: usize = 0x080A_0000;
}

/// Initialize the QEMU virt platform.
///
/// This is called early in the boot process, before the Xous kernel services start.
/// We use QEMU virt default addresses since QEMU's memory map is fixed and well-known.
///
/// Future improvement: parse FDT for addresses (makes this work with non-default QEMU configs).
pub fn init() {
    uart::init(defaults::UART0_BASE);
    uart::puts("BeetOS v0.1.0\n");
    uart::puts("Platform: QEMU virt (AArch64)\n");

    gic::init(defaults::GICD_BASE, defaults::GICR_BASE);
    uart::puts("GIC: initialized\n");

    timer::init();
    uart::puts("Timer: initialized\n");
}

/// Halt the system.
pub fn shutdown() -> ! {
    uart::puts("System halted.\n");
    loop {
        unsafe { core::arch::asm!("wfi", options(nomem, nostack)) };
    }
}

/// Platform-specific random number using arch RNDR or counter-based fallback.
pub mod rand {
    pub fn get_u32() -> u32 {
        crate::arch::rand::get_u32()
    }
}

/// Cache operations (no-op on QEMU — caches are coherent and managed by QEMU).
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

/// Watchdog (no-op on QEMU virt — no watchdog hardware).
pub mod wdt {
    #[allow(dead_code)]
    pub fn restart() {}
}
