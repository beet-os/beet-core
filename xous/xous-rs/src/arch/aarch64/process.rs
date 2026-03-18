// SPDX-FileCopyrightText: 2024 BeetOS contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! AArch64 process creation for Xous userspace.
//!
//! On hardware, process creation is handled entirely by the kernel via
//! the CreateProcess syscall. These types match the kernel's expectations.

use crate::{AppId, Error, MemoryAddress, MemoryRange, PID};

/// Describes all parameters required to start a new process.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct ProcessInit {
    /// The ELF binary to load.
    pub elf: MemoryRange,
    /// Pointer to the process name string (in kernel address space).
    pub name_addr: MemoryAddress,
    /// Application identity (used for IPC authentication).
    pub app_id: AppId,
}

impl From<&ProcessInit> for [usize; 7] {
    fn from(src: &ProcessInit) -> [usize; 7] {
        let app_id_words: [u32; 4] = (&src.app_id).into();
        [
            src.elf.as_ptr() as usize,
            src.elf.len(),
            app_id_words[0] as _,
            app_id_words[1] as _,
            app_id_words[2] as _,
            app_id_words[3] as _,
            src.name_addr.get(),
        ]
    }
}

impl ProcessInit {
    /// Free the name buffer after it has been consumed by the kernel.
    /// On AArch64, the name is passed by pointer and doesn't need separate cleanup.
    pub fn free_name_buf(&self) {
        // No-op: the name buffer is part of kernel memory
    }
}

impl TryFrom<[usize; 7]> for ProcessInit {
    type Error = Error;

    fn try_from(src: [usize; 7]) -> Result<ProcessInit, Self::Error> {
        Ok(ProcessInit {
            elf: unsafe { MemoryRange::new(src[0], src[1])? },
            name_addr: MemoryAddress::new(src[6]).ok_or(Error::BadAddress)?,
            app_id: [src[2] as u32, src[3] as u32, src[4] as u32, src[5] as u32].into(),
        })
    }
}

/// Returned when a process is created successfully.
#[derive(Debug, PartialEq)]
pub struct ProcessStartup {
    pid: PID,
}

impl ProcessStartup {
    pub fn new(pid: PID) -> Self {
        ProcessStartup { pid }
    }

    pub fn pid(&self) -> PID {
        self.pid
    }
}

impl core::fmt::Display for ProcessStartup {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}", self.pid)
    }
}

impl From<&[usize; 7]> for ProcessStartup {
    fn from(src: &[usize; 7]) -> ProcessStartup {
        ProcessStartup {
            pid: PID::new(src[0] as _).unwrap_or(unsafe { PID::new_unchecked(1) }),
        }
    }
}

impl From<[usize; 8]> for ProcessStartup {
    fn from(src: [usize; 8]) -> ProcessStartup {
        ProcessStartup {
            pid: PID::new(src[1] as _).unwrap_or(unsafe { PID::new_unchecked(1) }),
        }
    }
}

impl From<&ProcessStartup> for [usize; 7] {
    fn from(startup: &ProcessStartup) -> [usize; 7] {
        [startup.pid.get() as _, 0, 0, 0, 0, 0, 0]
    }
}

/// Process handle. On AArch64, the kernel manages processes directly,
/// so this is a unit type.
pub struct ProcessHandle;

/// Process arguments for spawning a new process.
pub struct ProcessArgs {
    pub app_id: AppId,
    pub name_addr: MemoryAddress,
    pub elf: MemoryRange,
}

impl ProcessArgs {
    pub fn new(elf: MemoryRange, name_addr: MemoryAddress, app_id: AppId) -> Self {
        ProcessArgs { app_id, name_addr, elf }
    }
}

/// Pre-syscall processing for process creation.
pub fn create_process_pre(_args: &ProcessArgs) -> core::result::Result<ProcessInit, Error> {
    Err(Error::InternalError) // Process creation is kernel-only on hardware
}

/// Post-syscall processing for process creation.
pub fn create_process_post(
    _args: ProcessArgs,
    _init: ProcessInit,
    startup: ProcessStartup,
) -> core::result::Result<(PID, ProcessHandle), Error> {
    Ok((startup.pid(), ProcessHandle))
}

/// Wait for a process to exit.
pub fn wait_process(_joiner: ProcessHandle) -> crate::SysCallResult {
    // TODO(M3): Implement process join via WaitProcess syscall
    Err(Error::InternalError)
}
