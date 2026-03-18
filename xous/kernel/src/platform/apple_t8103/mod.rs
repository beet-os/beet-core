// SPDX-FileCopyrightText: 2024 BeetOS contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Apple T8103 (M1) platform backend for the Xous kernel.
//!
//! This is a stub for Milestone 0. Actual implementation comes in M2.
//! All MMIO addresses come from the FDT passed by m1n1 at boot.

pub mod cache;
pub mod rand;
pub mod wdt;

// TODO(M2): implement these modules
// pub mod aic;         // Apple Interrupt Controller
// pub mod timer;       // ARM Generic Timer
// pub mod framebuffer; // SimpleFB from m1n1
// pub mod uart;        // Debug UART (optional)
// pub mod systemview;  // System view / debug

/// Platform-specific initialization for Apple T8103 (M1).
///
/// Called early in boot after the kernel has set up basic memory.
/// Will eventually initialize AIC, timers, framebuffer, etc.
pub fn init() {
    // TODO(M2): implement platform init
    // - Parse FDT for MMIO addresses
    // - Initialize AIC (interrupt controller)
    // - Initialize ARM Generic Timer
    // - Initialize framebuffer (SimpleFB from m1n1)
}

/// Shut down the platform.
///
/// On real hardware this would power off or reboot via PMGR.
#[allow(dead_code)]
pub fn shutdown() {
    // TODO(M2): implement platform shutdown via Apple PMGR
    loop {
        // Halt the CPU; on real hardware we would issue a power-off command
        #[cfg(target_arch = "aarch64")]
        unsafe {
            core::arch::asm!("wfi");
        }
    }
}
