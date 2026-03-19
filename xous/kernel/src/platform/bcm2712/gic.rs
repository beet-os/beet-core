// SPDX-FileCopyrightText: 2024 BeetOS contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! ARM GICv3 (GIC-600) driver for Raspberry Pi 5 (BCM2712).
//!
//! The BCM2712 embeds an ARM GIC-600, which is a GICv3-compatible controller.
//! Register layout is identical to the GICv3 spec — only the MMIO base addresses differ.
//!
//! Default addresses (from RPi5 device tree, ARM physical address space):
//!   - Distributor (GICD): 0x107FFF9000
//!   - Redistributor (GICR): 0x107FFD0000  (4 redistributors × 128KB each)

/// GIC base addresses (set from FDT at init).
static mut GICD_BASE: usize = 0;
static mut GICR_BASE: usize = 0;

mod gicd {
    pub const CTLR: usize = 0x0000;
    pub const TYPER: usize = 0x0004;
    pub const IGROUPR: usize = 0x0080;
    pub const ISENABLER: usize = 0x0100;
    pub const ICENABLER: usize = 0x0180;
    pub const ICPENDR: usize = 0x0280;
    pub const IPRIORITYR: usize = 0x0400;
    pub const CTLR_ENABLE_GRP0: u32 = 1 << 0;
    pub const CTLR_ENABLE_GRP1_NS: u32 = 1 << 1;
    pub const CTLR_ARE_S: u32 = 1 << 4;
}

mod gicr {
    pub const WAKER: usize = 0x0014;
    pub const IGROUPR0: usize = 0x10080;
    pub const ISENABLER0: usize = 0x10100;
    pub const ICENABLER0: usize = 0x10180;
    pub const IPRIORITYR: usize = 0x10400;
    pub const WAKER_PROCESSOR_SLEEP: u32 = 1 << 1;
    pub const WAKER_CHILDREN_ASLEEP: u32 = 1 << 2;
}

#[inline]
unsafe fn gicd_read(offset: usize) -> u32 {
    core::ptr::read_volatile((GICD_BASE + offset) as *const u32)
}

#[inline]
unsafe fn gicd_write(offset: usize, val: u32) {
    core::ptr::write_volatile((GICD_BASE + offset) as *mut u32, val);
}

#[inline]
unsafe fn gicr_read(offset: usize) -> u32 {
    core::ptr::read_volatile((GICR_BASE + offset) as *const u32)
}

#[inline]
unsafe fn gicr_write(offset: usize, val: u32) {
    core::ptr::write_volatile((GICR_BASE + offset) as *mut u32, val);
}

pub fn init(gicd_base: usize, gicr_base: usize) {
    unsafe {
        GICD_BASE = gicd_base;
        GICR_BASE = gicr_base;
    }
    init_distributor();
    init_redistributor();
    init_cpu_interface();
}

fn init_distributor() {
    unsafe {
        gicd_write(gicd::CTLR, 0);

        let typer = gicd_read(gicd::TYPER);
        let num_irqs = ((typer & 0x1F) + 1) * 32;

        let mut i: usize = 32;

        while i < num_irqs as usize {
            let reg_idx = i / 32;
            gicd_write(gicd::IGROUPR + reg_idx * 4, 0xFFFF_FFFF);
            gicd_write(gicd::ICENABLER + reg_idx * 4, 0xFFFF_FFFF);
            gicd_write(gicd::ICPENDR + reg_idx * 4, 0xFFFF_FFFF);
            i += 32;
        }

        i = 32;

        while i < num_irqs as usize {
            gicd_write(gicd::IPRIORITYR + i, 0xA0A0_A0A0);
            i += 4;
        }

        gicd_write(
            gicd::CTLR,
            gicd::CTLR_ENABLE_GRP0 | gicd::CTLR_ENABLE_GRP1_NS | gicd::CTLR_ARE_S,
        );
    }
}

fn init_redistributor() {
    unsafe {
        let waker = gicr_read(gicr::WAKER);
        gicr_write(gicr::WAKER, waker & !gicr::WAKER_PROCESSOR_SLEEP);

        while gicr_read(gicr::WAKER) & gicr::WAKER_CHILDREN_ASLEEP != 0 {
            core::hint::spin_loop();
        }

        gicr_write(gicr::IGROUPR0, 0xFFFF_FFFF);
        gicr_write(gicr::ICENABLER0, 0xFFFF_FFFF);

        for i in (0..32).step_by(4) {
            gicr_write(gicr::IPRIORITYR + i * 1, 0xA0A0_A0A0);
        }
    }
}

fn init_cpu_interface() {
    unsafe {
        let sre: u64;
        core::arch::asm!("mrs {}, S3_0_C12_C12_5", out(reg) sre);
        core::arch::asm!("msr S3_0_C12_C12_5, {}", in(reg) sre | 1);
        core::arch::asm!("isb");
        core::arch::asm!("msr S3_0_C4_C6_0, {}", in(reg) 0xFFu64);
        core::arch::asm!("msr S3_0_C12_C12_3, {}", in(reg) 0u64);
        core::arch::asm!("msr S3_0_C12_C12_7, {}", in(reg) 1u64);
        core::arch::asm!("isb");
    }
}

pub fn enable_irq(irq: u32) {
    unsafe {
        if irq < 32 {
            gicr_write(gicr::ISENABLER0, 1 << irq);
        } else {
            let reg_idx = (irq / 32) as usize;
            let bit = irq % 32;
            gicd_write(gicd::ISENABLER + reg_idx * 4, 1 << bit);
        }
    }
}

#[allow(dead_code)]
pub fn disable_irq(irq: u32) {
    unsafe {
        if irq < 32 {
            gicr_write(gicr::ICENABLER0, 1 << irq);
        } else {
            let reg_idx = (irq / 32) as usize;
            let bit = irq % 32;
            gicd_write(gicd::ICENABLER + reg_idx * 4, 1 << bit);
        }
    }
}

pub fn ack_irq() -> u32 {
    let iar: u64;
    unsafe {
        core::arch::asm!("mrs {}, S3_0_C12_C12_0", out(reg) iar);
    }
    iar as u32
}

pub fn eoi(irq: u32) {
    unsafe {
        core::arch::asm!("msr S3_0_C12_C12_1, {}", in(reg) irq as u64);
    }
}

pub const IRQ_SPURIOUS: u32 = 1023;
