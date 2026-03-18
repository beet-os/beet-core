// SPDX-FileCopyrightText: 2024 BeetOS contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! AArch64 backtrace support for the Xous kernel.
//!
//! Walks the frame pointer chain (X29/FP) to produce stack traces for
//! crash diagnostics. On AArch64, the frame layout is:
//!   [FP+0]  = saved FP (previous frame pointer)
//!   [FP+8]  = saved LR (return address)

use core::arch::asm;

/// Print a backtrace of the current process for debugging.
pub fn print_current_process_backtrace() {
    let mut fp: usize;
    unsafe { asm!("mov {}, x29", out(reg) fp, options(nomem, nostack)) };

    println!("  Backtrace:");
    let mut depth = 0;
    const MAX_DEPTH: usize = 32;

    while fp != 0 && depth < MAX_DEPTH {
        // Validate the frame pointer
        if fp & 0x7 != 0 {
            // Not 8-byte aligned
            break;
        }

        // Read saved FP and LR
        let saved_fp = unsafe { core::ptr::read_volatile(fp as *const usize) };
        let saved_lr = unsafe { core::ptr::read_volatile((fp + 8) as *const usize) };

        if saved_lr == 0 {
            break;
        }

        println!("    #{}: {:#018x}", depth, saved_lr);

        // Move to previous frame
        if saved_fp <= fp {
            // FP should always go up the stack (toward higher addresses)
            break;
        }
        fp = saved_fp;
        depth += 1;
    }

    if depth == 0 {
        println!("    (no frames)");
    }
}
