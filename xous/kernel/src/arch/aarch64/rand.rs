// SPDX-FileCopyrightText: 2024 BeetOS contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! AArch64 random number generation for the Xous kernel.
//!
//! Uses the ARMv8.5-RNG extension (RNDR register) if available,
//! otherwise falls back to a simple xorshift PRNG seeded from
//! the cycle counter.

use core::sync::atomic::{AtomicU64, Ordering};

static RNG_STATE: AtomicU64 = AtomicU64::new(0xDEAD_BEEF_CAFE_BABE);

/// Whether the CPU supports FEAT_RNG (RNDR instruction).
static mut HAS_RNDR: bool = false;

/// Initialize the RNG state from the cycle counter and detect RNDR support.
pub fn init() {
    let cntpct: u64;
    unsafe { core::arch::asm!("mrs {}, cntpct_el0", out(reg) cntpct, options(nomem, nostack)) };
    if cntpct != 0 {
        RNG_STATE.store(cntpct, Ordering::SeqCst);
    }

    // Check ID_AA64ISAR0_EL1.RNDR (bits [63:60])
    let isar0: u64;
    unsafe { core::arch::asm!("mrs {}, id_aa64isar0_el1", out(reg) isar0, options(nomem, nostack)) };
    unsafe { HAS_RNDR = ((isar0 >> 60) & 0xF) >= 1 };
}

/// Return a pseudo-random u32.
///
/// Uses RNDR if available (Apple M1 supports FEAT_RNG), otherwise xorshift64.
pub fn get_u32() -> u32 {
    // Try RNDR only if the CPU supports it
    if unsafe { HAS_RNDR } {
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
    }

    // Fallback: xorshift64
    let mut state = RNG_STATE.load(Ordering::SeqCst);
    state ^= state << 13;
    state ^= state >> 7;
    state ^= state << 17;
    RNG_STATE.store(state, Ordering::SeqCst);
    state as u32
}
