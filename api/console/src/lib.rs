// SPDX-FileCopyrightText: 2024 BeetOS contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! BeetOS Console API.
//!
//! Defines the IPC message types and client stubs for the console output
//! service. Platform drivers (PL011 UART on QEMU, framebuffer on Apple M1)
//! implement the server side; any process can use this API crate to write
//! text output.
//!
//! # Architecture
//!
//! ```text
//! [console driver (os/console)]   ←  platform-specific
//!        ↕ Xous IPC
//! [console API (api/console)]     ←  this crate (platform-independent)
//!        ↕
//! [shell / apps / log service]
//! ```

#![no_std]

/// Opcodes for console service IPC messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(usize)]
pub enum ConsoleOp {
    /// Write a string to the console (uses Borrow with UTF-8 data).
    Write = 0,
    /// Write a single character.
    Putc = 1,
    /// Clear the console screen.
    Clear = 2,
}
