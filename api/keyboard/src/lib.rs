// SPDX-FileCopyrightText: 2024 BeetOS contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! BeetOS Keyboard API.
//!
//! Defines the IPC message types and client stubs for the keyboard input
//! service. Platform drivers (UART on QEMU, SPI HID on Apple M1) implement
//! the server side; any process can use this API crate to receive key events.
//!
//! # Architecture
//!
//! ```text
//! [keyboard driver (os/keyboard)]  ←  platform-specific
//!        ↕ Xous IPC
//! [keyboard API (api/keyboard)]    ←  this crate (platform-independent)
//!        ↕
//! [shell / apps]
//! ```

#![no_std]

/// Key event types sent from the keyboard service to subscribers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyEvent {
    /// A key was pressed. Contains the ASCII value for printable keys.
    Pressed(u8),
    /// A key was released.
    Released(u8),
}

/// Opcodes for keyboard service IPC messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(usize)]
pub enum KeyboardOp {
    /// Subscribe to key events (sends a callback CID).
    Subscribe = 0,
    /// Unsubscribe from key events.
    Unsubscribe = 1,
    /// A key event notification (sent from server to subscriber).
    KeyEvent = 2,
}
