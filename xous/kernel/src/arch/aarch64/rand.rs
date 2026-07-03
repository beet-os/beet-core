// SPDX-FileCopyrightText: 2024 BeetOS contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! AArch64 random number generation for the Xous kernel.
//!
//! Uses the ARMv8.5-RNG extension (RNDR register) if available. Apple M1
//! (FEAT_RNG present) always takes this path, so hardware gets a true HWRNG.
//!
//! When RNDR is absent — notably QEMU's default `neoverse-n1` CPU, which
//! predates ARMv8.5 — we fall back to a xorshift64 PRNG. To avoid producing
//! a *reproducible* stream across reboots (which would be catastrophic for
//! the AES-GCM nonces in `api/cryptblock`: same key + repeated nonce leaks
//! plaintext and forges tags), every draw re-mixes the live virtual counter
//! (`CNTVCT_EL0`) into the state. The counter advances by a boot-timing- and
//! workload-dependent amount between the reset vector and the first nonce
//! draw, so two boots do not replay the same first nonce. This is a
//! best-effort entropy source, NOT a CSPRNG.
//!
//! NOTE: entropy consumers must go through `crate::platform::rand::get_u32`
//! rather than calling this module directly — the platform seam upgrades
//! the source to a hardware RNG where one exists (virtio-rng on QEMU
//! virt), and only lands here as the last resort.

use core::sync::atomic::{AtomicU64, Ordering};

static RNG_STATE: AtomicU64 = AtomicU64::new(0xDEAD_BEEF_CAFE_BABE);

/// Whether the CPU supports FEAT_RNG (RNDR instruction).
static mut HAS_RNDR: bool = false;

/// Read the EL0-accessible virtual counter. Monotonic, cheap, and varies
/// with elapsed cycles — used as a cheap timing-entropy source for the
/// PRNG fallback (never as the sole seed).
#[inline]
fn cntvct() -> u64 {
    let v: u64;
    unsafe { core::arch::asm!("mrs {}, cntvct_el0", out(reg) v, options(nomem, nostack)) };
    v
}

/// Initialize the RNG state from the cycle counter and detect RNDR support.
pub fn init() {
    // Accumulate a few counter reads separated by cheap work so a single
    // near-constant CNTPCT reading is not the whole seed.
    let mut seed: u64 = RNG_STATE.load(Ordering::SeqCst);
    for _ in 0..8 {
        let cnt: u64;
        unsafe {
            core::arch::asm!("mrs {}, cntpct_el0", out(reg) cnt, options(nomem, nostack));
        }
        seed = seed.rotate_left(7) ^ cnt.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    }
    // Never let the state collapse to zero (xorshift's fixed point).
    if seed == 0 {
        seed = 0xDEAD_BEEF_CAFE_BABE;
    }
    RNG_STATE.store(seed, Ordering::SeqCst);

    // Check ID_AA64ISAR0_EL1.RNDR (bits [63:60])
    let isar0: u64;
    unsafe { core::arch::asm!("mrs {}, id_aa64isar0_el1", out(reg) isar0, options(nomem, nostack)) };
    unsafe { HAS_RNDR = ((isar0 >> 60) & 0xF) >= 1 };
}

/// Return a pseudo-random u32.
///
/// Uses RNDR if available (Apple M1 supports FEAT_RNG), otherwise a
/// counter-mixed xorshift64 (see module docs).
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

    // Fallback: xorshift64, re-mixing the live virtual counter each draw so
    // the stream is not a pure function of the boot-time seed.
    let mut state = RNG_STATE.load(Ordering::SeqCst) ^ cntvct().wrapping_mul(0x2545_F491_4F6C_DD1D);
    if state == 0 {
        state = 0xDEAD_BEEF_CAFE_BABE;
    }
    state ^= state << 13;
    state ^= state >> 7;
    state ^= state << 17;
    RNG_STATE.store(state, Ordering::SeqCst);
    state as u32
}
