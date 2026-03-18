// SPDX-FileCopyrightText: 2022 Foundation Devices, Inc. <hello@foundation.xyz>
// SPDX-License-Identifier: Apache-2.0

#[cfg(beetos)]
pub mod apple_t8103;

pub mod rand;

/// Platform specific initialization.
#[cfg(beetos)]
pub fn init() { self::apple_t8103::init(); }

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
    // TODO(M2): implement DRAM power management for Apple Silicon
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
        // TODO(M2): implement background page zeroing
    }
}
