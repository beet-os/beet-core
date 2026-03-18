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
/// This function is called from the bootloader to initialize the kernel.
/// On BeetOS, m1n1 passes the FDT pointer in x0.
///
/// # Safety
///
/// This is safe to call only to initialize the kernel.
pub unsafe extern "C" fn init(arg_offset: *const u32) -> ! {
    // TODO(M2): Initialize platform from FDT
    // platform::apple_t8103::uart::init();
    // platform::apple_t8103::rand::init();

    args::KernelArguments::init(arg_offset);
    let args = args::KernelArguments::get();

    crate::mem::MemoryManager::with_mut(|mm| {
        mm.init_from_memory(beetos::ALLOCATION_TRACKER_OFFSET as _, &args)
            .expect("couldn't initialize memory manager");
    });

    SystemServices::with_mut(|system_services| system_services.init_from_memory(&args));

    arch::init();
    platform::init();

    platform::rand::get_u32();
    platform::rand::get_u32();

    kmain();

    unreachable!()
}

/// Common main function for BeetOS and hosted environments.
pub(crate) fn kmain() {
    #[cfg(beetos)]
    yield_slice();
    // Special case for testing: idle can return `false` to indicate exit
    while arch::idle() {}
}

fn main() {
    kmain();
}
