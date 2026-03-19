// SPDX-FileCopyrightText: 2024 BeetOS contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! AArch64 syscall invocation via SVC #0.
//!
//! Xous syscall ABI for AArch64:
//!   Arguments:  X0-X5, X8-X9 (8 registers)
//!   Return:     X0-X5, X8-X9 (8 registers)
//!   Instruction: SVC #0

use core::arch::asm;

use crate::{SysCall, SysCallResult};

/// Issue a Xous syscall via SVC #0.
///
/// Converts the `SysCall` to 8 register arguments, issues SVC #0,
/// and converts the 8 return values back to a `Result`.
pub fn syscall(call: SysCall) -> SysCallResult {
    let args = call.as_args();
    let r0: usize;
    let r1: usize;
    let r2: usize;
    let r3: usize;
    let r4: usize;
    let r5: usize;
    let r8: usize;
    let r9: usize;
    unsafe {
        asm!(
            "svc #0",
            inout("x0") args[0] => r0,
            inout("x1") args[1] => r1,
            inout("x2") args[2] => r2,
            inout("x3") args[3] => r3,
            inout("x4") args[4] => r4,
            inout("x5") args[5] => r5,
            inout("x8") args[6] => r8,
            inout("x9") args[7] => r9,
            // Clobber ALL caller-saved registers not used for args/results.
            // The SVC transitions to EL1 where the kernel may use any register.
            // Even though the kernel saves/restores the full context via
            // PROCESS_TABLE, a blocking syscall (SendMessage with BlockingScalar)
            // may not return until much later — after other processes have run.
            // The kernel restores registers from PROCESS_TABLE, but x18 and
            // NEON/FP state could differ if not listed as clobbers.
            lateout("x6") _,
            lateout("x7") _,
            lateout("x10") _,
            lateout("x11") _,
            lateout("x12") _,
            lateout("x13") _,
            lateout("x14") _,
            lateout("x15") _,
            lateout("x16") _,
            lateout("x17") _,
            lateout("x18") _,
        );
    }

    let ret = [r0, r1, r2, r3, r4, r5, r8, r9];
    let result = crate::Result::from_args(ret);
    match result {
        crate::Result::Error(e) => Err(e),
        other => Ok(other),
    }
}
