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
        let mut buf = [0u8; core::mem::size_of::<usize>()];
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pack_unpack_roundtrip_short_name() {
        let name = "hello";
        let packed = pack_name(name);
        assert_eq!(unpack_name(&packed), name);
    }

    #[test]
    fn pack_unpack_roundtrip_empty() {
        let packed = pack_name("");
        assert_eq!(unpack_name(&packed), "");
    }

    #[test]
    fn pack_unpack_roundtrip_single_char() {
        let packed = pack_name("x");
        assert_eq!(unpack_name(&packed), "x");
    }

    #[test]
    fn pack_unpack_roundtrip_word_boundary() {
        // Exactly one usize worth of bytes
        let word_size = core::mem::size_of::<usize>();
        let name: &str = &"abcdefgh"[..word_size];
        let packed = pack_name(name);
        assert_eq!(unpack_name(&packed), name);
    }

    #[test]
    fn pack_unpack_roundtrip_two_words() {
        let word_size = core::mem::size_of::<usize>();
        let full = "abcdefghijklmnop";
        let name = &full[..2 * word_size];
        let packed = pack_name(name);
        assert_eq!(unpack_name(&packed), name);
    }

    #[test]
    fn pack_unpack_max_length() {
        // 4 * sizeof(usize) = 32 bytes on 64-bit
        // Use a fixed 32-byte ASCII name
        let name = "ABCDEFGHIJKLMNOPQRSTUVWXYZ012345"; // 32 chars
        let word_size = core::mem::size_of::<usize>();
        let max_len = 4 * word_size;
        let name = &name[..max_len]; // trim to actual max
        let packed = pack_name(name);
        assert_eq!(unpack_name(&packed), name);
    }

    #[test]
    fn pack_truncates_beyond_max() {
        // Names longer than 4*sizeof(usize) are silently truncated
        let word_size = core::mem::size_of::<usize>();
        let max_len = 4 * word_size;
        let long_name = "abcdefghijklmnopqrstuvwxyz0123456789ABCD"; // 40 chars > 32
        let packed = pack_name(long_name);
        let result = unpack_name(&packed);
        assert_eq!(result.len(), max_len);
        assert_eq!(result, &long_name[..max_len]);
    }

    #[test]
    fn pack_name_zeros_unused_words() {
        let packed = pack_name("hi");
        // Words beyond the name should be zero
        assert_eq!(packed[1], 0);
        assert_eq!(packed[2], 0);
        assert_eq!(packed[3], 0);
    }

    #[test]
    fn pack_unpack_realistic_names() {
        for name in &["shell", "hello", "procman", "idle", "nvme", "wifi", "keyboard"] {
            let packed = pack_name(name);
            assert_eq!(unpack_name(&packed), *name, "round-trip failed for '{}'", name);
        }
    }

    #[test]
    fn procman_sid_is_valid() {
        // Verify the SID encodes "PROCMAN\0" as expected
        assert_ne!(PROCMAN_SID, [0, 0, 0, 0]);
        assert_eq!(PROCMAN_SID[2], 0);
        assert_eq!(PROCMAN_SID[3], 0);
    }

    #[test]
    fn procman_op_values_are_distinct() {
        assert_ne!(ProcManOp::SpawnAndWait as usize, ProcManOp::Spawn as usize);
        assert_ne!(ProcManOp::Spawn as usize, ProcManOp::Wait as usize);
        assert_ne!(ProcManOp::SpawnAndWait as usize, ProcManOp::Wait as usize);
    }
}
