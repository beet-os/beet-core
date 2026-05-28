// SPDX-FileCopyrightText: 2022 Foundation Devices, Inc. <hello@foundation.xyz>
// SPDX-License-Identifier: Apache-2.0

#[cfg(feature = "platform-qemu-virt")]
pub mod qemu_virt;

#[cfg(feature = "platform-bcm2712")]
pub mod bcm2712;

#[cfg(feature = "platform-apple-t8103")]
pub mod apple_t8103;

pub mod rand;

/// Write a string to the platform framebuffer console (if available).
/// No-op on platforms without a framebuffer or before FB is initialized.
/// Only callers live in `cfg(beetos)` paths (debug/serial), so the stub is
/// gated the same way to avoid a hosted-mode dead-code warning.
#[cfg(all(beetos, feature = "platform-qemu-virt"))]
pub fn fb_write(s: &str) { self::qemu_virt::fb::write_str(s); }

#[cfg(all(beetos, not(feature = "platform-qemu-virt")))]
pub fn fb_write(_s: &str) {}

/// Platform-agnostic kernel console writer.
///
/// Dispatches to whichever character sink the current platform exposes —
/// PL011 UART on qemu_virt, mini-UART on bcm2712, framebuffer on
/// apple_t8103 once wired up. Implements [`core::fmt::Write`] so callers
/// can use `write!`/`writeln!` directly:
///
/// ```ignore
/// use core::fmt::Write;
/// let _ = writeln!(crate::platform::Console, "ABORT: pid={}", pid);
/// ```
///
/// Diagnostic call sites in `arch/aarch64` (panic, abort, SVC handlers,
/// boot logs) use this instead of platform-specific writers so that
/// `pid`, `esr`, `far`, … stay live on every platform — eliminating the
/// "unused variable on non-qemu-virt builds" warnings that the previous
/// `#[cfg(feature = "platform-qemu-virt")]` print blocks produced, and
/// giving future platforms a single seam to plug their console into.
#[cfg(beetos)]
pub struct Console;

#[cfg(beetos)]
impl core::fmt::Write for Console {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        // Send to the platform's character sink first (UART/serial), then
        // mirror onto the framebuffer when one is set up. On Apple M1
        // there is no exposed UART, so the FB is the *only* surface boot
        // diagnostics ever reach — wiring it here means every panic /
        // abort / boot log line lands on screen automatically, without
        // each call site knowing about FB at all.
        #[cfg(feature = "platform-qemu-virt")]
        self::qemu_virt::uart::puts(s);
        #[cfg(feature = "platform-bcm2712")]
        self::bcm2712::uart::puts(s);
        #[cfg(feature = "platform-apple-t8103")]
        self::apple_t8103::console::puts(s);
        fb_write(s);
        let _ = s;
        Ok(())
    }
}

/// Platform specific initialization.
///
/// `fdt_phys` is the FDT physical address passed by the bootloader (x0 on
/// AArch64).  Platforms use it to discover MMIO base addresses; hosted mode
/// ignores it.
#[cfg(feature = "platform-qemu-virt")]
pub fn init(fdt_phys: *const u8) { self::qemu_virt::init(fdt_phys); }

#[cfg(feature = "platform-bcm2712")]
pub fn init(fdt_phys: *const u8) { self::bcm2712::init(fdt_phys); }

#[cfg(feature = "platform-apple-t8103")]
pub fn init(fdt_phys: *const u8) { self::apple_t8103::init(fdt_phys); }

/// Platform init stub for hosted mode (no platform hardware).
#[cfg(not(any(feature = "platform-qemu-virt", feature = "platform-bcm2712", feature = "platform-apple-t8103")))]
#[allow(dead_code)]
pub fn init(_fdt_phys: *const u8) {}

/// Halt / shutdown the system.
#[cfg(feature = "platform-qemu-virt")]
pub fn shutdown() -> ! { self::qemu_virt::shutdown(); }

#[cfg(feature = "platform-bcm2712")]
pub fn shutdown() -> ! { self::bcm2712::shutdown(); }

#[cfg(feature = "platform-apple-t8103")]
pub fn shutdown() -> ! { self::apple_t8103::shutdown(); }

#[cfg(not(any(feature = "platform-qemu-virt", feature = "platform-bcm2712", feature = "platform-apple-t8103")))]
#[allow(dead_code)]
pub fn shutdown() -> ! { loop { core::hint::spin_loop() } }

/// Platform cache operations.
#[cfg(beetos)]
pub mod cache {
    #[allow(dead_code)]
    pub fn clean_cache_l1() {
        #[cfg(feature = "platform-qemu-virt")]
        crate::platform::qemu_virt::cache::clean_cache_l1();
        #[cfg(feature = "platform-bcm2712")]
        crate::platform::bcm2712::cache::clean_cache_l1();
        #[cfg(feature = "platform-apple-t8103")]
        crate::platform::apple_t8103::cache::clean_cache_l1();
    }
    #[allow(dead_code)]
    pub fn clean_cache_l2() {
        #[cfg(feature = "platform-qemu-virt")]
        crate::platform::qemu_virt::cache::clean_cache_l2();
        #[cfg(feature = "platform-bcm2712")]
        crate::platform::bcm2712::cache::clean_cache_l2();
        #[cfg(feature = "platform-apple-t8103")]
        crate::platform::apple_t8103::cache::clean_cache_l2();
    }
    #[allow(dead_code)]
    pub fn print_cache_stats() {
        #[cfg(feature = "platform-qemu-virt")]
        crate::platform::qemu_virt::cache::print_cache_stats();
        #[cfg(feature = "platform-bcm2712")]
        crate::platform::bcm2712::cache::print_cache_stats();
        #[cfg(feature = "platform-apple-t8103")]
        crate::platform::apple_t8103::cache::print_cache_stats();
    }
}

/// Platform watchdog.
#[cfg(beetos)]
pub mod wdt {
    #[allow(dead_code)]
    pub fn restart() {
        #[cfg(feature = "platform-qemu-virt")]
        crate::platform::qemu_virt::wdt::restart();
        #[cfg(feature = "platform-bcm2712")]
        crate::platform::bcm2712::wdt::restart();
        #[cfg(feature = "platform-apple-t8103")]
        crate::platform::apple_t8103::wdt::restart();
    }
}

/// Cancel any pending preemption timer and return the elapsed time.
#[cfg(beetos)]
#[allow(dead_code)]
pub fn cancel_preemption() -> usize {
    // TODO(M2): implement using ARM Generic Timer
    0
}

/// Set up a preemption timer to fire after `ms` milliseconds.
#[cfg(beetos)]
#[allow(dead_code)]
pub fn setup_preemption(_ms: usize) {
    // TODO(M2): implement using ARM Generic Timer
}

/// Start measuring idle time (when PID 1 / idle process is scheduled).
#[cfg(beetos)]
#[allow(dead_code)]
pub fn start_measuring_idle() {
    // TODO(M2): implement using ARM Generic Timer
}

/// Set DRAM idle / power management mode.
#[cfg(beetos)]
#[allow(dead_code)]
pub fn set_dram_idle_mode(_dram: xous::DramIdleMode) {
    // TODO: implement DRAM power management
}

/// Page zeroing background task stubs.
#[cfg(beetos)]
pub mod page_zeroer {
    use crate::mem::MemoryManager;

    /// Start zeroing freed pages in the background.
    ///
    /// On real hardware this would use DMA or a low-priority mechanism
    /// to zero pages asynchronously. For now, this is a no-op stub.
    pub fn start(_mm: &mut MemoryManager) {
        // TODO: implement background page zeroing
    }
}
