#![cfg_attr(any(target_os = "none", beetos), no_std)]

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
pub use drop_deallocate::*;
#[cfg(beetos)]
pub use beetos;
pub use string::*;
pub use stringbuffer::*;
pub use syscall::*;

#[cfg(feature = "processes-as-threads")]
pub use crate::arch::ProcessArgsAsThread;
