// SPDX-FileCopyrightText: 2025 BeetOS contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

// Framebuffer console lives in the shared `beetos` crate so it can be used
// by both the kernel and userspace processes (e.g. the shell). Only the
// qemu_virt platform consumes this re-export from kernel-side today.
#[cfg(feature = "platform-qemu-virt")]
pub use beetos::fb_console::FbConsole;
