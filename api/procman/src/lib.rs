// SPDX-FileCopyrightText: 2024 BeetOS contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! BeetOS Process Manager API.
//!
//! Defines the IPC message types for the Process Manager service.
//! The procman service handles process lifecycle: spawn, wait, exit.
//!
//! # Architecture
//!
//! ```text
//! Shell → BlockingScalar(SpawnAndWait, name) → ProcMan (IPC)
//!   ↓
//! ProcMan → SVC SpawnByName + WaitProcess → Kernel
//!   ↓
//! Kernel: creates process, waits for exit, returns exit code
//!   ↓
//! ProcMan → ReturnScalar1(exit_code) → Shell
//! ```

#![no_std]

/// Well-known Server ID for the Process Manager service.
pub const PROCMAN_SID: [u32; 4] = [0x5052_4F43, 0x4D41_4E00, 0, 0]; // "PROCMAN\0"

/// Opcodes for procman service IPC messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(usize)]
pub enum ProcManOp {
    /// Spawn a process by name and wait for it to exit.
    /// BlockingScalar: arg1-arg4 = name bytes packed as 4×usize (max 32 bytes).
    /// Returns: Scalar1(exit_code) on success.
    SpawnAndWait = 0,
    /// Spawn a process by name, return immediately with PID.
    /// Scalar: arg1-arg4 = name bytes packed as 4×usize.
    /// Returns: Scalar1(pid) on success.
    Spawn = 1,
    /// Wait for a process to exit.
    /// BlockingScalar: arg1 = pid.
    /// Returns: Scalar1(exit_code).
    Wait = 2,
}

/// Pack a process name (up to 32 bytes) into 4 usize values for Scalar messages.
pub fn pack_name(name: &str) -> [usize; 4] {
    let bytes = name.as_bytes();
    let mut result = [0usize; 4];
    let word_size = core::mem::size_of::<usize>();
    for (i, chunk) in bytes.chunks(word_size).enumerate() {
        if i >= 4 {
            break;
        }
        let mut buf = [0u8; 8]; // max usize size
        buf[..chunk.len()].copy_from_slice(chunk);
        result[i] = usize::from_le_bytes(buf);
    }
    result
}

/// Unpack a process name from 4 usize values.
/// Returns the name as a str (up to 32 bytes, trimmed at first null).
pub fn unpack_name(args: &[usize; 4]) -> &str {
    let word_size = core::mem::size_of::<usize>();
    let ptr = args.as_ptr() as *const u8;
    let max_len = 4 * word_size;
    let bytes = unsafe { core::slice::from_raw_parts(ptr, max_len) };
    let len = bytes.iter().position(|&b| b == 0).unwrap_or(max_len);
    core::str::from_utf8(&bytes[..len]).unwrap_or("")
}
