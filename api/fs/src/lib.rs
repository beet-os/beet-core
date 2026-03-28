// SPDX-FileCopyrightText: 2024 BeetOS contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! BeetOS Filesystem API.
//!
//! IPC protocol for the filesystem service. The FS service owns the ramfs
//! and disk (tar) data. Clients send requests via Xous IPC.
//!
//! # Protocol
//!
//! Small metadata ops use BlockingScalar (path packed into 4×usize).
//! Data transfer uses MutableBorrow: the client lends a page-aligned buffer
//! to the server, the server fills it, then the kernel returns the page.

#![no_std]

/// Well-known Server ID for the filesystem service.
pub const FS_SID: [u32; 4] = [0x4245_4554, 0x4F53_4653, 0, 0]; // "BEETOSFS"

/// Maximum path length that fits in 4×usize (32 bytes on 64-bit).
pub const MAX_PATH_LEN: usize = 4 * core::mem::size_of::<usize>();

/// Byte offset of the status byte in a MutableBorrow buffer (LsBuf, CatBuf).
/// Layout: [0..32] = path input (null-terminated), [32] = FsError as u8, [33..] = output text.
pub const BUF_STATUS_OFFSET: usize = MAX_PATH_LEN;

/// Byte offset where output text starts in a MutableBorrow buffer.
pub const BUF_TEXT_OFFSET: usize = MAX_PATH_LEN + 1;

/// Opcodes for FS service IPC messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(usize)]
pub enum FsOp {
    /// Print file contents to UART.
    /// BlockingScalar: arg1-arg4 = path packed.
    /// Returns Scalar1(0=ok, 1=not found, 3=is directory).
    Cat = 0,

    /// Print directory listing to UART.
    /// BlockingScalar: arg1-arg4 = path packed.
    /// Returns Scalar1(0=ok, 1=not found, 4=not directory).
    Ls = 1,

    /// Create a directory.
    /// BlockingScalar: arg1-arg4 = path packed.
    /// Returns Scalar1(FsError code).
    Mkdir = 2,

    /// Remove a file or empty directory.
    /// BlockingScalar: arg1-arg4 = path packed.
    /// Returns Scalar1(FsError code).
    Remove = 3,

    /// Write a short string to a file.
    /// BlockingScalar: arg1 = path_word0, arg2 = path_word1,
    ///   arg3 = content_word0, arg4 = content_word1.
    /// Path: max 16 bytes. Content: max 16 bytes.
    /// Returns Scalar1(FsError code).
    WriteShort = 4,

    /// Get filesystem stats.
    /// BlockingScalar: arg1-arg4 = 0.
    /// Returns Scalar5(used_files, max_files, used_bytes, disk_size, disk_files).
    Stats = 5,

    /// Check if a path is a directory (used by the shell's `cd` command).
    /// BlockingScalar: arg1-arg4 = path packed.
    /// Returns Scalar1(FsError): Ok=directory, NotFound, NotDirectory.
    IsDir = 6,

    /// Buffer-based directory listing (MutableBorrow).
    /// Buffer layout: [0..32] path input, [32] status (FsError as u8), [33..] output text (null-terminated).
    LsBuf = 7,

    /// Buffer-based file read (MutableBorrow).
    /// Same layout as LsBuf.
    CatBuf = 8,
}

/// Error codes returned by the FS service.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(usize)]
pub enum FsError {
    Ok = 0,
    NotFound = 1,
    AlreadyExists = 2,
    IsDirectory = 3,
    NotDirectory = 4,
    NotEmpty = 5,
    NoSpace = 6,
    ReadOnly = 7,
    InvalidPath = 8,
}

/// Pack a path (up to 32 bytes) into 4 usize values for Scalar messages.
pub fn pack_path(path: &str) -> [usize; 4] {
    let bytes = path.as_bytes();
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

/// Unpack a path from 4 usize values.
pub fn unpack_path(args: &[usize; 4]) -> &str {
    let word_size = core::mem::size_of::<usize>();
    let ptr = args.as_ptr() as *const u8;
    let max_len = 4 * word_size;
    let bytes = unsafe { core::slice::from_raw_parts(ptr, max_len) };
    let len = bytes.iter().position(|&b| b == 0).unwrap_or(max_len);
    core::str::from_utf8(&bytes[..len]).unwrap_or("")
}

/// Write a path into the first 32 bytes of a buffer. Returns bytes written.
pub fn write_path_to_buf(buf: &mut [u8], path: &str) -> usize {
    let bytes = path.as_bytes();
    let len = bytes.len().min(MAX_PATH_LEN - 1).min(buf.len() - 1);
    buf[..len].copy_from_slice(&bytes[..len]);
    buf[len] = 0; // null-terminate
    len + 1
}

/// Read a path from the first 32 bytes of a buffer.
pub fn read_path_from_buf(buf: &[u8]) -> &str {
    let max = MAX_PATH_LEN.min(buf.len());
    let len = buf[..max].iter().position(|&b| b == 0).unwrap_or(max);
    core::str::from_utf8(&buf[..len]).unwrap_or("")
}
