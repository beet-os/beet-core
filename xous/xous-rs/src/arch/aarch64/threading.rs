// SPDX-FileCopyrightText: 2024 BeetOS contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! AArch64 thread creation for Xous userspace.
//!
//! On hardware, threads are created via the CreateThread syscall.
//! The kernel allocates the thread ID and sets up the execution context.

use crate::{Error, MemoryRange, TID};

/// Thread initialization parameters.
/// Passed to the kernel's CreateThread syscall.
///
/// On AArch64, this contains the actual entry point and arguments.
/// In hosted mode, ThreadInit is empty because threads are OS threads.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ThreadInit {
    /// Entry point function address.
    pub call: usize,
    /// Stack memory range (or None to let the kernel allocate).
    pub stack: Option<MemoryRange>,
    /// Arguments passed in X0-X3.
    pub arg1: usize,
    pub arg2: usize,
    pub arg3: usize,
    pub arg4: usize,
}

impl Default for ThreadInit {
    fn default() -> Self {
        ThreadInit {
            call: 0,
            stack: None,
            arg1: 0,
            arg2: 0,
            arg3: 0,
            arg4: 0,
        }
    }
}

/// Convert a syscall number and ThreadInit to syscall arguments.
/// Position 0 is the syscall number, positions 1-7 are the ThreadInit fields.
pub fn thread_to_args(call: usize, init: &ThreadInit) -> [usize; 8] {
    [
        call,
        init.call,
        init.stack.map(|s| s.as_ptr() as usize).unwrap_or(0),
        init.stack.map(|s| s.len()).unwrap_or(0),
        init.arg1,
        init.arg2,
        init.arg3,
        init.arg4,
    ]
}

/// Convert syscall arguments back to a ThreadInit.
/// Takes 7 arguments (a1-a7), where a1 is the entry point, a2-a3 are stack, a4-a7 are args.
pub fn args_to_thread(
    a1: usize,
    a2: usize,
    a3: usize,
    a4: usize,
    a5: usize,
    a6: usize,
    a7: usize,
) -> core::result::Result<ThreadInit, Error> {
    Ok(ThreadInit {
        call: a1,
        stack: if a2 != 0 {
            unsafe { Some(MemoryRange::new(a2, a3)?) }
        } else {
            None
        },
        arg1: a4,
        arg2: a5,
        arg3: a6,
        arg4: a7,
    })
}

/// Wait handle for a thread.
pub struct WaitHandle<T> {
    _phantom: core::marker::PhantomData<T>,
    #[allow(dead_code)]
    tid: TID,
}

/// Get the current thread ID.
pub fn thread_id() -> TID {
    let tid: usize;
    unsafe {
        core::arch::asm!(
            "mrs {}, tpidr_el0",
            out(reg) tid,
            options(nomem, nostack, preserves_flags),
        );
    }
    // The TID is stored in the lower 16 bits of TPIDR_EL0 by the kernel
    (tid & 0xFFFF) as TID
}

// Thread creation pre/post functions.
// On hardware, these prepare the syscall arguments and handle the result.

pub fn create_thread_0_pre<U>(_f: &fn() -> U) -> core::result::Result<ThreadInit, Error>
where U: Send + 'static {
    Ok(ThreadInit { call: *_f as usize, ..Default::default() })
}

pub fn create_thread_1_pre<U>(_f: &fn(usize) -> U, a1: &usize) -> core::result::Result<ThreadInit, Error>
where U: Send + 'static {
    Ok(ThreadInit { call: *_f as usize, arg1: *a1, ..Default::default() })
}

pub fn create_thread_2_pre<U>(_f: &fn(usize, usize) -> U, a1: &usize, a2: &usize) -> core::result::Result<ThreadInit, Error>
where U: Send + 'static {
    Ok(ThreadInit { call: *_f as usize, arg1: *a1, arg2: *a2, ..Default::default() })
}

pub fn create_thread_3_pre<U>(_f: &fn(usize, usize, usize) -> U, a1: &usize, a2: &usize, a3: &usize) -> core::result::Result<ThreadInit, Error>
where U: Send + 'static {
    Ok(ThreadInit { call: *_f as usize, arg1: *a1, arg2: *a2, arg3: *a3, ..Default::default() })
}

pub fn create_thread_4_pre<U>(_f: &fn(usize, usize, usize, usize) -> U, a1: &usize, a2: &usize, a3: &usize, a4: &usize) -> core::result::Result<ThreadInit, Error>
where U: Send + 'static {
    Ok(ThreadInit { call: *_f as usize, stack: None, arg1: *a1, arg2: *a2, arg3: *a3, arg4: *a4 })
}

pub fn create_thread_0_post<U>(_f: fn() -> U, tid: TID) -> core::result::Result<WaitHandle<U>, Error>
where U: Send + 'static {
    Ok(WaitHandle { _phantom: core::marker::PhantomData, tid })
}

pub fn create_thread_1_post<U>(_f: fn(usize) -> U, _a1: usize, tid: TID) -> core::result::Result<WaitHandle<U>, Error>
where U: Send + 'static {
    Ok(WaitHandle { _phantom: core::marker::PhantomData, tid })
}

pub fn create_thread_2_post<U>(_f: fn(usize, usize) -> U, _a1: usize, _a2: usize, tid: TID) -> core::result::Result<WaitHandle<U>, Error>
where U: Send + 'static {
    Ok(WaitHandle { _phantom: core::marker::PhantomData, tid })
}

pub fn create_thread_3_post<U>(_f: fn(usize, usize, usize) -> U, _a1: usize, _a2: usize, _a3: usize, tid: TID) -> core::result::Result<WaitHandle<U>, Error>
where U: Send + 'static {
    Ok(WaitHandle { _phantom: core::marker::PhantomData, tid })
}

pub fn create_thread_4_post<U>(_f: fn(usize, usize, usize, usize) -> U, _a1: usize, _a2: usize, _a3: usize, _a4: usize, tid: TID) -> core::result::Result<WaitHandle<U>, Error>
where U: Send + 'static {
    Ok(WaitHandle { _phantom: core::marker::PhantomData, tid })
}

pub fn create_thread_simple_pre<T, U>(_f: &fn(T) -> U, _arg: &T) -> core::result::Result<ThreadInit, Error>
where T: Send + 'static, U: Send + 'static {
    Ok(ThreadInit::default())
}

pub fn create_thread_simple_post<T, U>(_f: fn(T) -> U, _arg: T, tid: TID) -> core::result::Result<WaitHandle<U>, Error>
where T: Send + 'static, U: Send + 'static {
    Ok(WaitHandle { _phantom: core::marker::PhantomData, tid })
}

pub fn create_thread_pre<F, T>(_f: &F) -> core::result::Result<ThreadInit, Error>
where F: FnOnce() -> T, F: Send + 'static, T: Send + 'static {
    Ok(ThreadInit::default())
}

pub fn create_thread_post<F, U>(_f: F, tid: TID) -> core::result::Result<WaitHandle<U>, Error>
where F: FnOnce() -> U, F: Send + 'static, U: Send + 'static {
    Ok(WaitHandle { _phantom: core::marker::PhantomData, tid })
}

/// Wait for a thread to complete.
pub fn wait_thread<T>(_joiner: WaitHandle<T>) -> crate::SysCallResult {
    // TODO(M3): Implement via WaitThread syscall
    Err(Error::InternalError)
}
