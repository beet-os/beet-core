// SPDX-FileCopyrightText: 2024 BeetOS contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! AArch64 userspace architecture module for Xous.
//!
//! Provides syscall wrappers (SVC), thread/process primitives,
//! memory mapping helpers, and IRQ number definitions.

mod syscall_impl;
pub use syscall_impl::*;

pub mod irq;

mod mem;
pub use mem::*;

mod process;
pub use process::*;

mod threading;
pub use threading::*;

/// Maximum length of a process name in bytes.
pub const MAX_PROCESS_NAME_LEN: usize = 64;
