// SPDX-FileCopyrightText: 2024 BeetOS contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Random number generation for Apple T8103 (M1).
//!
//! Apple Silicon provides a hardware TRNG accessible via system registers.

/// Return a random u32 from the hardware TRNG.
///
/// On real hardware this will read from Apple's TRNG.
/// For now, returns a value derived from the cycle counter as a placeholder.
pub fn get_u32() -> u32 {
    // TODO(M2): implement using Apple Silicon TRNG (s3_5_c15_c0_2 or similar)
    // For now, use a simple LFSR-style fallback so callers get non-zero values.
    static mut SEED: u32 = 0xDEAD_BEEF;
    // SAFETY: kernel is single-threaded during early boot; this is a temporary stub.
    unsafe {
        SEED ^= SEED << 13;
        SEED ^= SEED >> 17;
        SEED ^= SEED << 5;
        SEED
    }
}
