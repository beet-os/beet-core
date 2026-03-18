// SPDX-FileCopyrightText: 2020 Sean Cross <sean@xobs.io>
// SPDX-License-Identifier: Apache-2.0

#![cfg_attr(beetos, no_main)]
#![cfg_attr(beetos, no_std)]

#[cfg(beetos)]
#[cfg_attr(not(beetos), macro_use)]
extern crate bitflags;

#[macro_use]
mod debug;

#[cfg(all(test, not(beetos)))]
mod test;

mod arch;

#[macro_use]
mod args;
mod io;
mod irq;
mod macros;
mod mem;
mod platform;
mod process;
mod scheduler;
mod server;
mod services;
mod syscall;

use services::SystemServices;
use xous::*;

#[cfg(beetos)]
#[no_mangle]
/// Rust entry point called from start.S after basic hardware setup.
///
/// On QEMU virt: x0 = FDT pointer from QEMU.
/// On Apple M1: x0 = FDT pointer from m1n1.
///
/// # Safety
///
/// This is safe to call only once, from the startup assembly.
pub unsafe extern "C" fn _start_rust(arg_offset: *const u32) -> ! {
    // Initialize platform hardware first (UART for output, GIC, timer)
    platform::init();

    // Store the boot arguments (FDT pointer) for later use
    args::KernelArguments::init(arg_offset);

    // At this point we have UART output, GIC, and timer running.
    // The full Xous kernel init (memory manager, services) will be
    // wired up when we integrate the loader and process infrastructure.
    //
    // For M2, we demonstrate: boot → platform init → UART output → timer ticks → idle.

    arch::init();

    // Initialize RNG (detects RNDR support, seeds from counter)
    crate::arch::rand::init();

    platform::rand::get_u32();
    platform::rand::get_u32();

    // Unmask IRQs so timer ticks are delivered
    unsafe {
        core::arch::asm!("msr daifclr, #0x2", options(nomem, nostack)); // Clear IRQ mask
    }

    #[cfg(feature = "platform-qemu-virt")]
    platform::qemu_virt::uart::puts("Kernel initialized. Entering idle loop.\n");

    kmain();

    unreachable!()
}

/// Common main function for BeetOS and hosted environments.
pub(crate) fn kmain() {
    // On hosted mode, yield_slice() triggers the scheduler via IPC.
    // On bare metal, the scheduler is driven by timer interrupts,
    // so we skip this and go straight to the idle loop.
    #[cfg(not(beetos))]
    yield_slice();
    // Special case for testing: idle can return `false` to indicate exit
    while arch::idle() {}
}

#[allow(dead_code)]
fn main() {
    kmain();
}
