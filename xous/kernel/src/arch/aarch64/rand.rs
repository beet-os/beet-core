// SPDX-FileCopyrightText: 2024 BeetOS contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! AArch64 random number generation for the Xous kernel.
//!
//! Uses the ARMv8.5-RNG extension (RNDR register) if available,
//! otherwise falls back to a simple xorshift PRNG seeded from
//! the cycle counter.

use core::sync::atomic::{AtomicU64, Ordering};

static RNG_STATE: AtomicU64 = AtomicU64::new(0xDEAD_BEEF_CAFE_BABE);

/// Initialize the RNG state from the cycle counter.
pub fn init() {
    let cntpct: u64;
    unsafe { core::arch::asm!("mrs {}, cntpct_el0", out(reg) cntpct, options(nomem, nostack)) };
    if cntpct != 0 {
        RNG_STATE.store(cntpct, Ordering::SeqCst);
    }
}

/// Return a pseudo-random u32.
///
/// First attempts to use the ARMv8.5-RNG RNDR instruction.
/// Falls back to xorshift64 if RNDR is not available or fails.
pub fn get_u32() -> u32 {
    // Try RNDR (may not be available on all cores)
    // Apple M1 does support FEAT_RNG
    let val: u64;
    let success: u64;
    unsafe {
        core::arch::asm!(
            "mrs {val}, s3_3_c2_c4_0",  // RNDR
            "cset {ok}, ne",              // NZCV.Z=0 means success
            val = out(reg) val,
            ok = out(reg) success,
            options(nomem, nostack),
        );
    }
    if success != 0 {
        return val as u32;
    }

    // Fallback: xorshift64
    let mut state = RNG_STATE.load(Ordering::SeqCst);
    state ^= state << 13;
    state ^= state >> 7;
    state ^= state << 17;
    RNG_STATE.store(state, Ordering::SeqCst);
    state as u32
}
