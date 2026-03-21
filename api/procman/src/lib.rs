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

/// User-spawnable programs embedded in the kernel binary table.
/// Internal services (log, idle, shell, procman, fs) are excluded.
/// Keep in sync with BINARY_TABLE in xous/kernel/src/arch/aarch64/boot.rs.
pub const PROGRAMS: &[&str] = &[
    "hello-nostd",
    "hello-std",
];

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
    /// Spawn a process by name with arguments and wait for it to exit.
    /// MutableBorrow: buffer contains "name\0arg1\0arg2\0..." (null-separated).
    /// The `id` field = SpawnAndWaitWithArgs as usize.
    /// The `valid` field = total byte length used in the buffer.
    /// Returns: MemoryReturned, then caller checks exit code from `offset` field.
    SpawnAndWaitWithArgs = 3,
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

/// Maximum command line length for SpawnAndWaitWithArgs Borrow messages.
pub const MAX_CMDLINE: usize = 256;

/// Format a command line into a buffer for SpawnAndWaitWithArgs.
///
/// Writes "name\0arg1\0arg2\0..." into `buf` and returns the number of bytes written.
/// The name is the first null-separated field, followed by each argument.
pub fn format_cmdline(buf: &mut [u8], name: &str, args: &[&str]) -> usize {
    let mut pos = 0;
    let name_bytes = name.as_bytes();
    let copy_len = name_bytes.len().min(buf.len());
    buf[..copy_len].copy_from_slice(&name_bytes[..copy_len]);
    pos += copy_len;

    for arg in args {
        if pos >= buf.len() {
            break;
        }
        buf[pos] = 0; // null separator
        pos += 1;
        let arg_bytes = arg.as_bytes();
        let copy_len = arg_bytes.len().min(buf.len() - pos);
        if copy_len > 0 {
            buf[pos..pos + copy_len].copy_from_slice(&arg_bytes[..copy_len]);
            pos += copy_len;
        }
    }
    pos
}

/// Parse a command line buffer from SpawnAndWaitWithArgs.
///
/// Returns `(name, args_start, args_len)` where:
/// - `name` is the process name (bytes before first null)
/// - `args_start` is the offset of the first argument byte (after the name's null)
/// - `args_len` is the total length of the remaining args data
pub fn parse_cmdline(buf: &[u8], valid_len: usize) -> (&str, usize, usize) {
    let data = &buf[..valid_len.min(buf.len())];
    let name_end = data.iter().position(|&b| b == 0).unwrap_or(data.len());
    let name = core::str::from_utf8(&data[..name_end]).unwrap_or("");
    let args_start = if name_end < data.len() { name_end + 1 } else { data.len() };
    let args_len = data.len().saturating_sub(args_start);
    (name, args_start, args_len)
}

/// Pack a process name into 2 usize values (max 16 bytes on 64-bit).
/// Used for the SpawnByNameWithArgs kernel syscall.
pub fn pack_name_short(name: &str) -> [usize; 2] {
    let bytes = name.as_bytes();
    let mut result = [0usize; 2];
    let word_size = core::mem::size_of::<usize>();
    for (i, chunk) in bytes.chunks(word_size).enumerate() {
        if i >= 2 {
            break;
        }
        let mut buf = [0u8; core::mem::size_of::<usize>()];
        buf[..chunk.len()].copy_from_slice(chunk);
        result[i] = usize::from_le_bytes(buf);
    }
    result
}

/// Unpack a process name from 2 usize values.
pub fn unpack_name_short(args: &[usize; 2]) -> &str {
    let word_size = core::mem::size_of::<usize>();
    let ptr = args.as_ptr() as *const u8;
    let max_len = 2 * word_size;
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
        assert_ne!(ProcManOp::SpawnAndWaitWithArgs as usize, ProcManOp::SpawnAndWait as usize);
    }

    #[test]
    fn format_parse_cmdline_no_args() {
        let mut buf = [0u8; 64];
        let len = format_cmdline(&mut buf, "hello", &[]);
        assert_eq!(len, 5);
        let (name, _, args_len) = parse_cmdline(&buf, len);
        assert_eq!(name, "hello");
        assert_eq!(args_len, 0);
    }

    #[test]
    fn format_parse_cmdline_with_args() {
        let mut buf = [0u8; 64];
        let len = format_cmdline(&mut buf, "hello", &["world", "foo"]);
        let (name, args_start, args_len) = parse_cmdline(&buf, len);
        assert_eq!(name, "hello");
        // args data = "world\0foo"
        let args_data = &buf[args_start..args_start + args_len];
        assert_eq!(args_data, b"world\0foo");
    }

    #[test]
    fn pack_unpack_name_short_roundtrip() {
        for name in &["hello", "cat", "ls", "procman", "a"] {
            let packed = pack_name_short(name);
            assert_eq!(unpack_name_short(&packed), *name);
        }
    }
}
