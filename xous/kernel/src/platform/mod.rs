// SPDX-FileCopyrightText: 2022 Foundation Devices, Inc. <hello@foundation.xyz>
// SPDX-License-Identifier: Apache-2.0

#[cfg(beetos)]
pub mod apple_t8103;

pub mod rand;

/// Platform specific initialization.
#[cfg(beetos)]
pub fn init() { self::apple_t8103::init(); }
