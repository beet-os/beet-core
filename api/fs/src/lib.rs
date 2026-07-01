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

    /// Format the encrypted `/data` area (MutableBorrow, LsBuf layout —
    /// the "path" field carries the passphrase). Creates a fresh salt,
    /// erases all slots, leaves the area unlocked.
    CryptFormat = 9,

    /// Unlock the encrypted `/data` area (MutableBorrow, passphrase in
    /// the path field). Status: Ok, Locked (bad passphrase), or
    /// NotFound (area not formatted).
    CryptOpen = 10,

    /// Lock the `/data` area (BlockingScalar, no args): drops the key.
    /// Returns Scalar1(FsError::Ok).
    CryptLock = 11,

    /// Buffer-based file write (MutableBorrow). Lifts `WriteShort`'s
    /// 15-byte scalar cap. Layout:
    ///   `[0..32]`   path (null-terminated) — client writes
    ///   `[32]`      status (`FsError` as u8) — server writes on return
    ///   `[33..35]`  content length u16 LE — client writes
    ///   `[35..]`    content bytes (may include NUL) — client writes
    /// Server reads path + length + content, performs the write, stamps
    /// the status at [32]. Content is capped at [`WRITE_MAX_CONTENT`];
    /// a longer declared length is rejected (`NoSpace`), never
    /// truncated silently.
    WriteBuf = 12,
}

/// Offset of the u16 LE content length in a `WriteBuf` buffer.
pub const WRITE_LEN_OFFSET: usize = BUF_TEXT_OFFSET; // 33
/// Offset where `WriteBuf` content begins.
pub const WRITE_CONTENT_OFFSET: usize = BUF_TEXT_OFFSET + 2; // 35
/// Largest content a single `WriteBuf` carries. Comfortably above the
/// shell's 256-byte line limit and the encrypted-slot payload (~448 B),
/// so realistic writes never truncate; a larger declared length is a
/// loud `NoSpace`, not a silent cut.
pub const WRITE_MAX_CONTENT: usize = 512;

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
    /// The encrypted `/data` area is locked (or the passphrase given
    /// to `CryptOpen` was wrong).
    Locked = 9,
    /// Authenticated decryption failed — on-disk data was tampered
    /// with or corrupted.
    Corrupt = 10,
}

// ============================================================================
// Encrypted /data slot-entry format
// ============================================================================
//
// One file = one cryptblock slot. Inside the slot's plaintext payload:
//
//   [ name_len u8 | reserved u8 | data_len u16 LE | name | data ]
//
// No central directory: lookup scans slots and matches names, so there
// is no metadata sector to corrupt and every file write is a single
// atomic sealed-sector write.

/// Max file-name length inside a `/data` slot entry.
pub const DATA_NAME_MAX: usize = 32;
/// Entry header bytes before the name.
pub const DATA_ENTRY_HDR: usize = 4;

/// Pack `(name, data)` into `out` (a slot plaintext buffer). Returns
/// the packed length, or `None` if name/data don't fit.
pub fn pack_data_entry(name: &str, data: &[u8], out: &mut [u8]) -> Option<usize> {
    let name = name.as_bytes();
    if name.is_empty() || name.len() > DATA_NAME_MAX {
        return None;
    }
    let total = DATA_ENTRY_HDR + name.len() + data.len();
    if total > out.len() {
        return None;
    }
    out[0] = name.len() as u8;
    out[1] = 0;
    out[2..4].copy_from_slice(&(data.len() as u16).to_le_bytes());
    out[4..4 + name.len()].copy_from_slice(name);
    out[4 + name.len()..total].copy_from_slice(data);
    // Zero the tail so re-used slots don't leak previous plaintext
    // lengths through the (encrypted) payload.
    for b in &mut out[total..] {
        *b = 0;
    }
    Some(total)
}

/// Unpack a slot entry: returns `(name, data)` or `None` if the
/// payload doesn't parse (wrong lengths, non-UTF8 name).
pub fn unpack_data_entry(payload: &[u8]) -> Option<(&str, &[u8])> {
    if payload.len() < DATA_ENTRY_HDR {
        return None;
    }
    let name_len = payload[0] as usize;
    if name_len == 0 || name_len > DATA_NAME_MAX {
        return None;
    }
    let data_len = u16::from_le_bytes([payload[2], payload[3]]) as usize;
    let total = DATA_ENTRY_HDR + name_len + data_len;
    if total > payload.len() {
        return None;
    }
    let name = core::str::from_utf8(&payload[4..4 + name_len]).ok()?;
    Some((name, &payload[4 + name_len..total]))
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

#[cfg(test)]
mod data_entry_tests {
    use super::*;

    #[test]
    fn entry_roundtrip() {
        let mut buf = [0xAAu8; 484];
        let n = pack_data_entry("secret", b"hello world", &mut buf).unwrap();
        assert_eq!(n, 4 + 6 + 11);
        let (name, data) = unpack_data_entry(&buf).unwrap();
        assert_eq!(name, "secret");
        assert_eq!(data, b"hello world");
        // Tail must be zeroed (no stale-plaintext leak on slot reuse).
        assert!(buf[n..].iter().all(|&b| b == 0));
    }

    #[test]
    fn empty_data_ok() {
        let mut buf = [0u8; 484];
        pack_data_entry("touch", b"", &mut buf).unwrap();
        let (name, data) = unpack_data_entry(&buf).unwrap();
        assert_eq!(name, "touch");
        assert!(data.is_empty());
    }

    #[test]
    fn oversize_rejected() {
        let mut buf = [0u8; 484];
        let big = [0u8; 481]; // 4 + 1 + 481 > 484 with 1-char name? 486 > 484
        assert!(pack_data_entry("x", &big, &mut buf).is_none());
        let max = [0u8; 479]; // 4 + 1 + 479 = 484 exactly
        assert!(pack_data_entry("x", &max, &mut buf).is_some());
    }

    #[test]
    fn bad_names_rejected() {
        let mut buf = [0u8; 484];
        assert!(pack_data_entry("", b"x", &mut buf).is_none());
        let long = "ñ".repeat(20); // 40 bytes > DATA_NAME_MAX
        assert!(pack_data_entry(&long, b"x", &mut buf).is_none());
    }

    #[test]
    fn garbage_unparses_cleanly() {
        assert!(unpack_data_entry(&[0u8; 484]).is_none()); // name_len 0
        assert!(unpack_data_entry(&[40, 0, 0, 0]).is_none()); // name_len > max... 40 > 32
        let mut bad = [0u8; 16];
        bad[0] = 4;
        bad[2] = 0xFF; bad[3] = 0xFF; // data_len way past payload
        assert!(unpack_data_entry(&bad).is_none());
    }
}
