// SPDX-FileCopyrightText: 2025 BeetOS contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

// Framebuffer console lives in the shared `beetos` crate so it can be used
// by both the kernel and userspace processes (e.g. the shell).
pub use beetos::fb_console::FbConsole;
