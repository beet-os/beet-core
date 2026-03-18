// SPDX-FileCopyrightText: 2022 Foundation Devices, Inc. <hello@foundation.xyz>
// SPDX-License-Identifier: Apache-2.0

pub fn get_u32() -> u32 {
    // hosted rand code is coupled with arch code.
    #[cfg(any(windows, unix))]
    let rand = crate::arch::rand::get_u32();

    #[cfg(feature = "platform-qemu-virt")]
    let rand = crate::platform::qemu_virt::rand::get_u32();

    #[cfg(feature = "platform-apple-t8103")]
    let rand = crate::platform::apple_t8103::rand::get_u32();

    rand
}
