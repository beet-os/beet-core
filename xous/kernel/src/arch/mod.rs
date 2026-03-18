// SPDX-FileCopyrightText: 2020 Sean Cross <sean@xobs.io>
// SPDX-License-Identifier: Apache-2.0

#[cfg(beetos)]
mod aarch64;
#[cfg(beetos)]
pub use crate::arch::aarch64::*;

#[cfg(any(windows, unix))]
mod hosted;
#[cfg(any(windows, unix))]
pub use hosted::*;
