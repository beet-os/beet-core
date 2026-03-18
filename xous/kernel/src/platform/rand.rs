// SPDX-FileCopyrightText: 2022 Foundation Devices, Inc. <hello@foundation.xyz>
// SPDX-License-Identifier: Apache-2.0

pub fn get_u32() -> u32 {
    // hosted rand code is coupled with arch code.
    #[cfg(any(windows, unix))]
    {
        crate::arch::rand::get_u32()
    }

    #[cfg(all(target_os = "none", feature = "platform-qemu-virt"))]
    {
        crate::platform::qemu_virt::rand::get_u32()
    }

    #[cfg(all(target_os = "none", feature = "platform-apple-t8103"))]
    {
        crate::platform::apple_t8103::rand::get_u32()
    }

    // Fallback for bare-metal without a specific platform feature:
    // use the arch-level RNG (RNDR or xorshift fallback).
    #[cfg(all(
        target_os = "none",
        not(feature = "platform-qemu-virt"),
        not(feature = "platform-apple-t8103")
    ))]
    {
        crate::arch::rand::get_u32()
    }
}
