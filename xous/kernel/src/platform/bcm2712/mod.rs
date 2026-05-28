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

pub mod fb;
pub mod gic;
pub mod mailbox;
pub mod pcie_brcm;
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
/// `fdt_phys` is the FDT physical address passed by the RPi firmware
/// (kernel8.img is jumped to with x0 = FDT phys per the standard
/// Linux ARM64 boot protocol). We discover the UART and GIC MMIO
/// addresses from the FDT — RPi5's device tree advertises
/// `arm,pl011` and `arm,gic-v3` exactly like QEMU virt does, so the
/// same parser in arch/aarch64/boot.rs handles both. If FDT parsing
/// finds nothing (e.g. when chain-loaded from a non-Linux bootloader
/// that doesn't pass a DTB), we fall back to the compiled-in
/// BCM2712-defaults so the boot at least tries something sensible.
pub fn init(fdt_phys: *const u8) {
    let mmio = unsafe {
        crate::arch::boot::parse_fdt_mmio(
            beetos::phys_to_virt(fdt_phys as usize) as *const u8,
        )
    };

    let uart0_phys = mmio.uart0_phys.unwrap_or(defaults::UART0_BASE);
    let gicd_phys  = mmio.gicd_phys.unwrap_or(defaults::GICD_BASE);
    let gicr_phys  = mmio.gicr_phys.unwrap_or(defaults::GICR_BASE);

    uart::init(uart0_phys);
    uart::puts("BeetOS v0.1.0\n");
    uart::puts("Platform: Raspberry Pi 5 (BCM2712 / AArch64)\n");

    if mmio.uart0_phys.is_some() {
        uart::puts("UART: address from FDT\n");
    } else {
        uart::puts("UART: address from default (FDT not found)\n");
    }

    gic::init(gicd_phys, gicr_phys);

    if mmio.gicd_phys.is_some() {
        uart::puts("GIC: initialized (address from FDT)\n");
    } else {
        uart::puts("GIC: initialized (address from default)\n");
    }

    timer::init();
    uart::puts("Timer: initialized\n");

    // Ask the firmware for a 1280x720 framebuffer via the BCM2835
    // mailbox property interface. On success we hand it to the
    // beetos::gui compositor exactly like the qemu_virt ramfb path
    // — same WindowManager, same widgets, same demo desktop.
    if fb::init(1280, 720) {
        uart::puts("FB: mailbox allocated\n");
        fb::populate_demo_desktop();
    } else {
        uart::puts("FB: mailbox FB unavailable (no display? wrong res?) - UART only\n");
    }
}

pub fn shutdown() -> ! {
    uart::puts("System halted.\n");
    loop {
        unsafe { core::arch::asm!("wfi", options(nomem, nostack)) };
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
