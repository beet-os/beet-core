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
#[cfg(feature = "platform-qemu-virt")]
pub fn fb_write(s: &str) { self::qemu_virt::fb::write_str(s); }

#[cfg(not(feature = "platform-qemu-virt"))]
pub fn fb_write(_s: &str) {}

/// Platform specific initialization.
#[cfg(feature = "platform-qemu-virt")]
pub fn init() { self::qemu_virt::init(); }

#[cfg(feature = "platform-bcm2712")]
pub fn init() { self::bcm2712::init(); }

#[cfg(feature = "platform-apple-t8103")]
pub fn init() { self::apple_t8103::init(); }

/// Platform init stub for hosted mode (no platform hardware).
#[cfg(not(any(feature = "platform-qemu-virt", feature = "platform-bcm2712", feature = "platform-apple-t8103")))]
#[allow(dead_code)]
pub fn init() {}

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
