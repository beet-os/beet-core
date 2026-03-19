// SPDX-FileCopyrightText: 2024 BeetOS contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! ARM Generic Timer driver for Raspberry Pi 5 (BCM2712).
//!
//! The ARM generic timer is part of the Cortex-A76 core — it uses system
//! registers (CNTFRQ_EL0, CNTP_TVAL_EL0, CNTP_CTL_EL0) that are identical
//! across all AArch64 platforms. This driver is functionally identical to the
//! QEMU virt timer; only the GIC routing is confirmed by FDT.
//!
//! On BCM2712 the EL1 physical timer fires PPI 30 (INTID 30), routed through
//! the GIC-600 redistributor — same as on QEMU virt.

use super::gic;

/// PPI for EL1 physical timer (standard ARM, same on all platforms).
pub const TIMER_IRQ: u32 = 30;

static mut TIMER_FREQ: u64 = 0;
static mut TICK_INTERVAL: u64 = 0;
static mut TICK_COUNT: u64 = 0;

const TICK_RATE_HZ: u64 = 100;

#[inline]
fn read_cntfrq() -> u64 {
    let freq: u64;
    unsafe { core::arch::asm!("mrs {}, cntfrq_el0", out(reg) freq, options(nomem, nostack)) };
    freq
}

#[inline]
fn set_tval(ticks: u64) {
    unsafe {
        core::arch::asm!("msr cntp_tval_el0, {}", in(reg) ticks, options(nomem, nostack));
    }
}

#[inline]
fn set_ctl(enable: bool, mask: bool) {
    let val: u64 = if enable { 1 } else { 0 } | if mask { 1 << 1 } else { 0 };
    unsafe {
        core::arch::asm!("msr cntp_ctl_el0, {}", in(reg) val, options(nomem, nostack));
        core::arch::asm!("isb", options(nomem, nostack));
    }
}

pub fn init() {
    let freq = read_cntfrq();
    let interval = freq / TICK_RATE_HZ;

    unsafe {
        TIMER_FREQ = freq;
        TICK_INTERVAL = interval;
        TICK_COUNT = 0;
    }

    set_tval(interval);
    set_ctl(true, false);

    gic::enable_irq(TIMER_IRQ);
}

pub fn handle_tick() -> u64 {
    let count = unsafe {
        TICK_COUNT += 1;
        TICK_COUNT
    };
    let interval = unsafe { TICK_INTERVAL };
    set_tval(interval);
    count
}

#[allow(dead_code)]
pub fn tick_count() -> u64 {
    unsafe { TICK_COUNT }
}
