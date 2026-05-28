// SPDX-FileCopyrightText: 2024 BeetOS contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Apple T8103 console output stub.
//!
//! M1 hardware has no exposed UART; once the framebuffer driver lands the
//! body of `puts` should forward to it (m1n1 always hands BeetOS a
//! SimpleFB region). Until then, kernel diagnostics on this platform are
//! silently discarded — the existing `cfg(feature = "platform-qemu-virt")`
//! gates dropped them too, so there is no regression.

pub fn puts(_s: &str) {
    // TODO: forward to framebuffer console once apple_t8103::framebuffer
    // is wired up.
}
