// SPDX-FileCopyrightText: 2024 BeetOS contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! BeetOS Keyboard API — Exclusive Ownership Model.
//!
//! The keyboard is an exclusive resource. At any given time, exactly ONE
//! process owns it and receives key events. All others are blocked. This
//! is an architectural guarantee against keylogging — there is no
//! "subscribe" or "read-only" mode. A process that hasn't called `Claim`
//! simply never receives any key events.
//!
//! # Security Model
//!
//! - PID 1 (init/shell) gets initial ownership at boot.
//! - Only the current focus manager can transfer ownership.
//! - When a process dies, it's automatically removed from the focus stack
//!   and the next process regains ownership.
//! - No broadcast mode. Events go to exactly one process.
//!
//! # Architecture
//!
//! ```text
//! [keyboard driver (os/keyboard)]  ←  platform-specific (UART, SPI HID)
//!        ↕ Xous IPC
//! [keyboard API (api/keyboard)]    ←  this crate (platform-independent)
//!        ↕
//! [shell / apps]                   ←  only the current owner receives events
//! ```
//!
//! # Reference
//!
//! This design follows the Xous GAM (Graphical Abstraction Manager) pattern
//! from `xous-core/services/gam/`. See `gam/src/tokens.rs` for the trust
//! level system that inspired this exclusive ownership model.

#![no_std]

/// Key event types sent from the keyboard service to the current owner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyEvent {
    /// A key was pressed. Contains the ASCII value for printable keys.
    Pressed(u8),
    /// A key was released.
    Released(u8),
}

/// Opcodes for keyboard service IPC messages.
///
/// The keyboard server maintains a focus stack of PIDs. Only the process
/// at the top of the stack receives key events. This prevents keylogging
/// by making it architecturally impossible for non-owners to observe input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(usize)]
pub enum KeyboardOp {
    /// Request exclusive keyboard ownership.
    ///
    /// Pushes the caller's PID onto the focus stack. Only succeeds if:
    /// - No one currently owns it, OR
    /// - The caller is the current focus manager (shell/window manager)
    ///
    /// Returns: `Ok` or `Error(AccessDenied)`
    Claim = 0,

    /// Release keyboard ownership.
    ///
    /// Pops the current owner from the focus stack. The previous owner
    /// (next in stack) automatically regains focus and starts receiving
    /// key events again.
    Release = 1,

    /// Key event notification (sent ONLY to current owner).
    ///
    /// The keyboard driver pushes raw key events to the server, which
    /// routes them exclusively to `focus_stack.last()`. If no process
    /// owns the keyboard, events are dropped.
    KeyEvent = 2,

    /// Query who currently owns the keyboard (for debugging only).
    ///
    /// Returns the PID of the current owner, or 0 if no one owns it.
    /// This is a read-only query — it does not grant any access.
    QueryOwner = 3,
}

/// Error types for keyboard operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyboardError {
    /// The caller is not authorized to claim the keyboard.
    /// Only the current focus manager or PID 1 can claim ownership.
    AccessDenied,

    /// The caller tried to release ownership but doesn't own the keyboard.
    NotOwner,

    /// Internal server error.
    InternalError,
}
