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

/// Well-known Server ID for the shell's *input* channel.
/// The kernel's UART (and TCP) IRQ handlers send received characters
/// to whichever process holds input focus on this SID — that's the shell.
pub const CONSOLE_SID: [u32; 4] = [0x434F_4E53, 0x4F4C_4500, 0, 0]; // "CONSOLE\0"

/// Well-known Server ID for the console *output* service (`os/console`).
/// Any process that wants its stdout mirrored — to UART today, to the
/// TCP remote console once connected, to the framebuffer eventually —
/// sends `ConsoleOp::Write` to this SID. The service runs as PID 7 and
/// is the architectural seam between "a process printed something" and
/// "those bytes left the box".
pub const CONSOLE_OUT_SID: [u32; 4] = [0x434F_4E4F, 0x5554_0000, 0, 0]; // "CONOUT\0\0"

/// Opcodes for console service IPC messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(usize)]
pub enum ConsoleOp {
    /// A character received from UART (Scalar: arg1 = char as usize).
    /// Sent to [`CONSOLE_SID`] (the shell's input channel).
    Char = 0,
    /// Write up to 32 bytes packed into the four non-blocking Scalar
    /// args (`arg1..arg4` little-endian, NUL-padded). The payload
    /// **length** rides in the high bits of `id` (see
    /// [`encode_write_id`] / [`decode_write_id`]) so all four args are
    /// payload. Sent to [`CONSOLE_OUT_SID`]. Used as a non-allocating
    /// fast path for short strings — the common case (one prompt, one
    /// command result line). Non-blocking so the shell never stalls on
    /// console output.
    Write = 1,
    /// Write a single character (Scalar: arg1 = char). Sent to
    /// [`CONSOLE_OUT_SID`].
    Putc = 2,
    /// Clear the console screen. Sent to [`CONSOLE_OUT_SID`].
    Clear = 3,
}

/// Maximum bytes carried in one [`ConsoleOp::Write`] message: four
/// 8-byte scalar args = 32 bytes. Longer strings ship in 32-byte chunks.
pub const WRITE_CHUNK: usize = 32;

/// Encode `(op, len)` into a Scalar `id`. We have plenty of bits — the
/// opcode lives in `0..8`, the length in `8..16`. Length is bounded by
/// [`WRITE_CHUNK`] (32) so 8 bits is overkill but cheap.
#[inline]
pub fn encode_write_id(len: usize) -> usize {
    (ConsoleOp::Write as usize) | ((len & 0xff) << 8)
}

/// Decode the `(op, len)` pair from a Scalar `id`. Returns `None` if
/// the low byte isn't [`ConsoleOp::Write`].
#[inline]
pub fn decode_write_id(id: usize) -> Option<usize> {
    if (id & 0xff) == ConsoleOp::Write as usize {
        Some((id >> 8) & 0xff)
    } else {
        None
    }
}

/// Pack the first up-to-32 bytes of `s` into four little-endian u64s
/// suitable for [`ConsoleOp::Write`] Scalar args. Returns the number
/// of bytes packed (always `min(s.len(), 32)`).
pub fn pack_write(s: &[u8]) -> ([usize; 4], usize) {
    let n = s.len().min(WRITE_CHUNK);
    let mut words = [0u8; 32];
    words[..n].copy_from_slice(&s[..n]);
    let mut out = [0usize; 4];
    for i in 0..4 {
        let mut word = 0u64;
        for j in 0..8 {
            word |= (words[i * 8 + j] as u64) << (j * 8);
        }
        out[i] = word as usize;
    }
    (out, n)
}

/// Inverse of [`pack_write`]: unpack four scalar args back into bytes.
pub fn unpack_write(args: [usize; 4], out: &mut [u8; WRITE_CHUNK]) {
    for i in 0..4 {
        let word = args[i] as u64;
        for j in 0..8 {
            out[i * 8 + j] = ((word >> (j * 8)) & 0xff) as u8;
        }
    }
}
