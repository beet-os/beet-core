// SPDX-FileCopyrightText: 2024 BeetOS contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Watchdog timer stubs for Apple T8103 (M1).
//!
//! Apple Silicon has a watchdog timer in the PMGR block.

/// Restart (pet) the watchdog timer.
pub fn restart() {
    // TODO(M2): implement watchdog restart via Apple PMGR WDT registers
}
