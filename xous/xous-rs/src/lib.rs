#![cfg_attr(any(target_os = "none", target_os = "beetos", beetos), no_std)]

pub mod arch;

pub mod carton;
pub mod definitions;

pub mod drop_deallocate;
pub mod process;
pub mod string;
pub mod stringbuffer;
pub mod syscall;

pub use arch::{ProcessArgs, ProcessInit, ProcessStartup, ThreadInit};
pub use definitions::*;

/// Page size for Xous memory operations.
/// On AArch64 hardware (BeetOS): 16KB (Apple Silicon).
/// In hosted mode: 4KB (matches host OS expectations).
#[cfg(any(target_os = "none", target_os = "beetos"))]
pub const PAGE_SIZE: usize = beetos::PAGE_SIZE; // 16384

#[cfg(not(any(target_os = "none", target_os = "beetos")))]
pub const PAGE_SIZE: usize = 4096;
pub use drop_deallocate::*;
#[cfg(beetos)]
pub use beetos;
pub use string::*;
pub use stringbuffer::*;
pub use syscall::*;

#[cfg(feature = "processes-as-threads")]
pub use crate::arch::ProcessArgsAsThread;
