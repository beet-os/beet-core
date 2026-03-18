// SPDX-FileCopyrightText: 2024 BeetOS contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! ARM Generic Timer driver for QEMU virt platform.
//!
//! Uses the EL1 Physical Timer (CNTP). The timer fires PPI 30 (IRQ 30)
//! on the GIC. QEMU virt uses a standard ARM generic timer.

use super::gic;

/// PPI number for the EL1 physical timer (standard ARM mapping).
pub const TIMER_IRQ: u32 = 30;

/// Timer frequency in Hz (read from CNTFRQ_EL0 at init).
static mut TIMER_FREQ: u64 = 0;

/// Tick interval in timer counts.
static mut TICK_INTERVAL: u64 = 0;

/// Number of timer ticks since boot.
static mut TICK_COUNT: u64 = 0;

/// Desired tick rate in Hz.
const TICK_RATE_HZ: u64 = 100;

/// Read the counter frequency.
#[inline]
fn read_cntfrq() -> u64 {
    let freq: u64;
    unsafe { core::arch::asm!("mrs {}, cntfrq_el0", out(reg) freq, options(nomem, nostack)) };
    freq
}

/// Read the current counter value.
#[inline]
#[allow(dead_code)]
pub fn read_counter() -> u64 {
    let cnt: u64;
    unsafe { core::arch::asm!("mrs {}, cntpct_el0", out(reg) cnt, options(nomem, nostack)) };
    cnt
}

/// Set the timer to fire after `ticks` counter ticks.
#[inline]
fn set_tval(ticks: u64) {
    unsafe {
        core::arch::asm!("msr cntp_tval_el0, {}", in(reg) ticks, options(nomem, nostack));
    }
}

/// Enable or disable the EL1 physical timer.
#[inline]
fn set_ctl(enable: bool, mask: bool) {
    let val: u64 = if enable { 1 } else { 0 } | if mask { 1 << 1 } else { 0 };
    unsafe {
        core::arch::asm!("msr cntp_ctl_el0, {}", in(reg) val, options(nomem, nostack));
        core::arch::asm!("isb", options(nomem, nostack));
    }
}

/// Initialize the ARM generic timer.
///
/// Sets up a periodic tick at TICK_RATE_HZ and enables the timer PPI on the GIC.
pub fn init() {
    let freq = read_cntfrq();
    let interval = freq / TICK_RATE_HZ;

    unsafe {
        TIMER_FREQ = freq;
        TICK_INTERVAL = interval;
        TICK_COUNT = 0;
    }

    // Set first timer deadline
    set_tval(interval);
    // Enable timer, unmask interrupt
    set_ctl(true, false);

    // Enable PPI 30 on the GIC
    gic::enable_irq(TIMER_IRQ);
}

/// Handle timer IRQ — called from the IRQ handler when PPI 30 fires.
///
/// Rearms the timer for the next tick and returns the tick count.
pub fn handle_tick() -> u64 {
    let count = unsafe {
        TICK_COUNT += 1;
        TICK_COUNT
    };

    // Rearm timer for next tick
    let interval = unsafe { TICK_INTERVAL };
    set_tval(interval);

    count
}

/// Get the current tick count.
#[allow(dead_code)]
pub fn tick_count() -> u64 {
    unsafe { TICK_COUNT }
}

/// Get timer frequency in Hz.
#[allow(dead_code)]
pub fn frequency() -> u64 {
    unsafe { TIMER_FREQ }
}
