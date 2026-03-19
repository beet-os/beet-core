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

#[cfg(any(beetos, test))]
mod shell;

#[cfg(not(beetos))]
use services::SystemServices;
#[cfg(not(beetos))]
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

    arch::init();

    // Initialize RNG (detects RNDR support, seeds from counter)
    crate::arch::rand::init();

    platform::rand::get_u32();
    platform::rand::get_u32();

    // Initialize the Xous kernel services infrastructure.
    //
    // Full process loading requires:
    //   1. MemoryManager initialized with RAM page tracker (from loader)
    //   2. KernelArguments with BElf/PMem/PSys tags (from loader)
    //   3. MMU enabled with identity-mapped kernel pages
    //
    // On QEMU virt without a loader, we skip process infra setup.
    // The shell runs directly in kernel context (EL1). When a loader
    // is integrated, this will call:
    //   mm.init_from_memory(arg_offset, &args);
    //   ss.init_from_memory(&args);  // creates PID1 + loads BElf services
    //   scheduler.activate_current(); // start first user process

    // Unmask IRQs so timer ticks are delivered
    core::arch::asm!("msr daifclr, #0x2", options(nomem, nostack)); // Clear IRQ mask

    // Enable UART RX interrupt for shell input
    #[cfg(feature = "platform-qemu-virt")]
    platform::qemu_virt::uart::enable_rx_interrupt();

    // Initialize the interactive shell
    shell::init();

    kmain();

    unreachable!()
}

/// Common main function for BeetOS and hosted environments.
pub(crate) fn kmain() {
    // On bare metal, scheduling is driven by timer interrupts — no yield needed.
    // In hosted mode, arch::idle() drives the event loop.
    while arch::idle() {}
}

#[allow(dead_code)]
fn main() {
    kmain();
}
