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

mod kfuture;
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
/// Rust entry point called from start.S after MMU is enabled and the
/// kernel has jumped to high VA (TTBR1 space).
///
/// At this point:
///   - MMU is ON with TTBR0 (identity map) and TTBR1 (kernel linear map)
///   - VBAR_EL1 is set to exception vectors at high VA
///   - SP points to kernel stack at high VA
///   - BSS is zeroed
///   - All kernel code/data is accessible through TTBR1
///
/// `arg_offset` is the FDT physical address (from QEMU or m1n1 in x0).
///
/// # Safety
///
/// This is safe to call only once, from the startup assembly.
pub unsafe extern "C" fn _start_rust(arg_offset: *const u32) -> ! {
    // Initialize platform hardware (UART for output, GIC, timer).
    // MMIO is accessed at high VA through TTBR1 (phys_to_virt).
    platform::init();

    // Store the boot arguments (FDT pointer) for later use
    args::KernelArguments::init(arg_offset);

    arch::init();

    // Initialize RNG (detects RNDR support, seeds from counter)
    crate::arch::rand::init();

    platform::rand::get_u32();
    platform::rand::get_u32();

    // ---- Memory Manager initialization ----
    //
    // The assembly boot code already set up page tables and enabled MMU.
    // Here we parse FDT for RAM info, allocate the page tracker, and
    // initialize the MemoryManager.
    let boot_info = arch::boot::init_memory(arg_offset as *const u8);

    #[cfg(feature = "platform-qemu-virt")]
    {
        use core::fmt::Write;
        let _ = write!(platform::qemu_virt::uart::UartWriter,
            "MMU: enabled (RAM {:#x}+{:#x}, {} free pages)\n",
            boot_info.ram_base, boot_info.ram_size,
            boot_info.ram_size / beetos::PAGE_SIZE
        );
    }

    // Initialize the MemoryManager with the bootstrap page tracker
    arch::boot::init_memory_manager(&boot_info);

    // Initialize PID1 (kernel process) with all exceptions masked
    // to avoid timer interrupt storm during the large memset.
    core::arch::asm!("msr daifset, #0xf", options(nomem, nostack));
    services::SystemServices::with_mut(|ss| {
        ss.init_pid1();
    });

    // Unmask IRQs so timer ticks are delivered
    core::arch::asm!("msr daifclr, #0x2", options(nomem, nostack)); // Clear IRQ mask

    // Enable UART RX interrupt for shell input
    #[cfg(feature = "platform-qemu-virt")]
    platform::qemu_virt::uart::enable_rx_interrupt();

    // Launch user processes in EL0.
    // Creates separate address spaces (TTBR0 only — no kernel mappings),
    // then ERETs to the shell at EL0.
    //
    // From this point on, TTBR0 switches between user processes on context
    // switch. The kernel always runs through TTBR1 (stable, never changes).
    arch::boot::launch_first_process(&boot_info);
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
