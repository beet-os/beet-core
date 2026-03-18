// SPDX-FileCopyrightText: 2024 BeetOS contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! ARM GICv3 interrupt controller driver for QEMU virt platform.
//!
//! Reference: ARM GIC Architecture Specification (IHI 0069).
//! QEMU virt places GICv3 at:
//!   - Distributor (GICD): 0x0800_0000
//!   - Redistributor (GICR): 0x080A_0000

/// GIC base addresses (set from FDT at init).
static mut GICD_BASE: usize = 0;
static mut GICR_BASE: usize = 0;

// ============================================================================
// GICD (Distributor) registers
// ============================================================================
mod gicd {
    /// Distributor Control Register.
    pub const CTLR: usize = 0x0000;
    /// Interrupt Controller Type Register.
    pub const TYPER: usize = 0x0004;
    /// Interrupt Group Registers (32 bits each, 1 bit per IRQ).
    pub const IGROUPR: usize = 0x0080;
    /// Interrupt Set-Enable Registers.
    pub const ISENABLER: usize = 0x0100;
    /// Interrupt Clear-Enable Registers.
    pub const ICENABLER: usize = 0x0180;
    /// Interrupt Clear-Pending Registers.
    pub const ICPENDR: usize = 0x0280;
    /// Interrupt Priority Registers (8 bits per IRQ).
    pub const IPRIORITYR: usize = 0x0400;
    /// Interrupt Processor Targets Registers (for GICv2 compat, SPI only).
    #[allow(dead_code)]
    pub const ITARGETSR: usize = 0x0800;
    /// Interrupt Configuration Registers (2 bits per IRQ).
    #[allow(dead_code)]
    pub const ICFGR: usize = 0x0C00;

    // CTLR bits
    /// Enable Group 0 interrupts.
    pub const CTLR_ENABLE_GRP0: u32 = 1 << 0;
    /// Enable Group 1 Non-secure interrupts.
    pub const CTLR_ENABLE_GRP1_NS: u32 = 1 << 1;
    /// Affinity Routing Enable (ARE_S for secure).
    pub const CTLR_ARE_S: u32 = 1 << 4;
}

// ============================================================================
// GICR (Redistributor) registers — per-CPU
// ============================================================================
mod gicr {
    // RD_base frame (first 64KB)
    /// Redistributor Control Register.
    #[allow(dead_code)]
    pub const CTLR: usize = 0x0000;
    /// Redistributor Wake Register.
    pub const WAKER: usize = 0x0014;

    // SGI_base frame (second 64KB, offset 0x10000)
    /// Interrupt Group Register 0 (SGIs + PPIs, IRQs 0-31).
    pub const IGROUPR0: usize = 0x10080;
    /// Interrupt Set-Enable Register 0.
    pub const ISENABLER0: usize = 0x10100;
    /// Interrupt Clear-Enable Register 0.
    pub const ICENABLER0: usize = 0x10180;
    /// Interrupt Priority Registers (SGIs + PPIs).
    pub const IPRIORITYR: usize = 0x10400;

    // WAKER bits
    /// Processor Sleep.
    pub const WAKER_PROCESSOR_SLEEP: u32 = 1 << 1;
    /// Children Asleep — set by implementation when redistributor is quiescent.
    pub const WAKER_CHILDREN_ASLEEP: u32 = 1 << 2;
}

/// Read a GICD register.
#[inline]
unsafe fn gicd_read(offset: usize) -> u32 {
    core::ptr::read_volatile((GICD_BASE + offset) as *const u32)
}

/// Write a GICD register.
#[inline]
unsafe fn gicd_write(offset: usize, val: u32) {
    core::ptr::write_volatile((GICD_BASE + offset) as *mut u32, val);
}

/// Read a GICR register.
#[inline]
unsafe fn gicr_read(offset: usize) -> u32 {
    core::ptr::read_volatile((GICR_BASE + offset) as *const u32)
}

/// Write a GICR register.
#[inline]
unsafe fn gicr_write(offset: usize, val: u32) {
    core::ptr::write_volatile((GICR_BASE + offset) as *mut u32, val);
}

/// Initialize the GICv3.
///
/// Sets up distributor, redistributor, and CPU interface for single-core operation.
pub fn init(gicd_base: usize, gicr_base: usize) {
    unsafe {
        GICD_BASE = gicd_base;
        GICR_BASE = gicr_base;
    }

    init_distributor();
    init_redistributor();
    init_cpu_interface();
}

/// Initialize the GIC Distributor (GICD).
fn init_distributor() {
    unsafe {
        // Disable distributor while configuring
        gicd_write(gicd::CTLR, 0);

        // Read number of supported IRQ lines
        let typer = gicd_read(gicd::TYPER);
        let num_irqs = ((typer & 0x1F) + 1) * 32;

        // Configure all SPIs (IRQs 32+): Group 1, priority 0xA0, disabled
        let mut i: usize = 32;
        while i < num_irqs as usize {
            let reg_idx = i / 32;
            // Set all to Group 1 (non-secure)
            gicd_write(gicd::IGROUPR + reg_idx * 4, 0xFFFF_FFFF);
            // Disable all
            gicd_write(gicd::ICENABLER + reg_idx * 4, 0xFFFF_FFFF);
            // Clear pending
            gicd_write(gicd::ICPENDR + reg_idx * 4, 0xFFFF_FFFF);
            i += 32;
        }

        // Set all SPI priorities to 0xA0 (medium)
        i = 32;
        while i < num_irqs as usize {
            gicd_write(gicd::IPRIORITYR + i, 0xA0A0_A0A0);
            i += 4;
        }

        // Enable distributor with affinity routing
        gicd_write(
            gicd::CTLR,
            gicd::CTLR_ENABLE_GRP0 | gicd::CTLR_ENABLE_GRP1_NS | gicd::CTLR_ARE_S,
        );
    }
}

/// Initialize the GIC Redistributor (GICR) for the current CPU.
fn init_redistributor() {
    unsafe {
        // Wake up the redistributor
        let waker = gicr_read(gicr::WAKER);
        gicr_write(gicr::WAKER, waker & !gicr::WAKER_PROCESSOR_SLEEP);

        // Wait until ChildrenAsleep clears
        while gicr_read(gicr::WAKER) & gicr::WAKER_CHILDREN_ASLEEP != 0 {
            core::hint::spin_loop();
        }

        // Configure SGIs/PPIs (IRQs 0-31): Group 1, all disabled initially
        gicr_write(gicr::IGROUPR0, 0xFFFF_FFFF);
        gicr_write(gicr::ICENABLER0, 0xFFFF_FFFF);

        // Set SGI/PPI priorities to 0xA0
        for i in (0..32).step_by(4) {
            gicr_write(gicr::IPRIORITYR + i * 1, 0xA0A0_A0A0);
        }
    }
}

/// Initialize the GICv3 CPU Interface via system registers.
fn init_cpu_interface() {
    unsafe {
        // Enable System Register interface (ICC_SRE_EL1.SRE = 1)
        let sre: u64;
        core::arch::asm!("mrs {}, S3_0_C12_C12_5", out(reg) sre); // ICC_SRE_EL1
        core::arch::asm!("msr S3_0_C12_C12_5, {}", in(reg) sre | 1); // Set SRE bit
        core::arch::asm!("isb");

        // Set Priority Mask to allow all priorities (ICC_PMR_EL1 = 0xFF)
        core::arch::asm!("msr S3_0_C4_C6_0, {}", in(reg) 0xFFu64); // ICC_PMR_EL1

        // Set Binary Point Register (ICC_BPR1_EL1 = 0, no preemption grouping)
        core::arch::asm!("msr S3_0_C12_C12_3, {}", in(reg) 0u64); // ICC_BPR1_EL1

        // Enable Group 1 interrupts (ICC_IGRPEN1_EL1 = 1)
        core::arch::asm!("msr S3_0_C12_C12_7, {}", in(reg) 1u64); // ICC_IGRPEN1_EL1

        core::arch::asm!("isb");
    }
}

/// Enable a specific interrupt (SPI or PPI).
pub fn enable_irq(irq: u32) {
    unsafe {
        if irq < 32 {
            // SGI/PPI — use redistributor
            gicr_write(gicr::ISENABLER0, 1 << irq);
        } else {
            // SPI — use distributor
            let reg_idx = (irq / 32) as usize;
            let bit = irq % 32;
            gicd_write(gicd::ISENABLER + reg_idx * 4, 1 << bit);
        }
    }
}

/// Disable a specific interrupt.
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

/// Acknowledge the highest-priority pending interrupt.
/// Returns the interrupt ID (1023 = spurious).
pub fn ack_irq() -> u32 {
    let iar: u64;
    unsafe {
        // Read ICC_IAR1_EL1 (Group 1 interrupt acknowledge)
        core::arch::asm!("mrs {}, S3_0_C12_C12_0", out(reg) iar); // ICC_IAR1_EL1
    }
    iar as u32
}

/// Signal End-Of-Interrupt for the given interrupt ID.
pub fn eoi(irq: u32) {
    unsafe {
        // Write ICC_EOIR1_EL1
        core::arch::asm!("msr S3_0_C12_C12_1, {}", in(reg) irq as u64); // ICC_EOIR1_EL1
    }
}

/// Spurious interrupt ID — returned by ack_irq() when no interrupt is pending.
pub const IRQ_SPURIOUS: u32 = 1023;
