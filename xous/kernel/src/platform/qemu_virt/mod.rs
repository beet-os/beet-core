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

/// Default MMIO physical addresses for QEMU virt (from hw/arm/virt.c).
/// Converted to kernel VA (TTBR1 linear map) at init time via `phys_to_virt`.
mod defaults {
    pub const UART0_PHYS: usize = 0x0900_0000;
    pub const GICD_PHYS: usize = 0x0800_0000;
    pub const GICR_PHYS: usize = 0x080A_0000;
}

/// Initialize the QEMU virt platform.
///
/// Called after MMU is enabled and the kernel is running at high VA (TTBR1).
/// MMIO addresses are converted to kernel VA via `phys_to_virt` so all
/// device access goes through TTBR1, not TTBR0.
pub fn init() {
    uart::init(beetos::phys_to_virt(defaults::UART0_PHYS));
    uart::puts("BeetOS v0.1.0\n");
    uart::puts("Platform: QEMU virt (AArch64)\n");

    gic::init(beetos::phys_to_virt(defaults::GICD_PHYS), beetos::phys_to_virt(defaults::GICR_PHYS));
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
