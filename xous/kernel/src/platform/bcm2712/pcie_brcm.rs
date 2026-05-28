// SPDX-FileCopyrightText: 2025 BeetOS contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! BCM2712 PCIe-1 host bridge bring-up — RPi5 specific.
//!
//! The generic enumeration / config-space code lives in
//! [`crate::pcie`] so it has unit tests against a `MockMmio`.
//! Everything here is hardware-specific: the BCM controller's
//! PHY init, ATU window programming, link-training wait, and the
//! peripheral-base addresses pulled from the `brcm,bcm2712-pcie`
//! DTS node.
//!
//! Reference: Linux `drivers/pci/controller/pcie-brcmstb.c`.
//! Status: scaffold + enumeration, no PHY init yet. The PHY
//! sequence needs RPi5 hardware to validate so we keep the link
//! bring-up commented while the enumeration glue is exercised
//! purely on the generic side.

use crate::pcie::{self, Enumeration, RealMmio};

/// PCIe-1 controller MMIO base on BCM2712 (the one the RP1 sits on).
/// Source: RPi5 device tree (`brcm,bcm2712-pcie`).
pub const PCIE1_CTRL_BASE: usize = 0x1000_120000;

/// ECAM window — outbound configuration access region.
pub const PCIE1_ECAM_BASE: usize = 0x1000_400000;

/// 4 MB (1 bus × 256 dev/fn × 4 KB).
pub const PCIE1_ECAM_SIZE: usize = 4 * 1024 * 1024;

/// Known device IDs on the RPi5 PCIe bus.
pub const DID_BCM2712_RC: u16 = 0x2712; // BCM host bridge (type 1)
pub const DID_RP1:        u16 = 0x0001; // RP1 endpoint    (type 0)

/// Bring the host bridge up and walk the bus. Returns the
/// enumeration table on success; the RP1 endpoint (and anything
/// behind it) is the typical contents on real RPi5 hardware.
///
/// Today this is a "soft" init — we don't toggle the PHY because
/// the register sequence requires hardware-validated timing. On
/// real RPi5 the firmware may have already trained the link, so
/// just walking ECAM is a useful first check.
///
/// # Safety
///
/// `ctrl_base_va` / `ecam_base_va` must be Device-mapped MMIO
/// regions of `PCIE1_ECAM_SIZE` bytes each, accessed through TTBR1.
pub unsafe fn init(ctrl_base_va: usize, ecam_base_va: usize) -> Result<Enumeration, &'static str> {
    let _ = ctrl_base_va;
    let mmio = RealMmio::new(ecam_base_va as *mut u32, PCIE1_ECAM_SIZE);
    let enumeration = pcie::enumerate(&mmio, /* max_bus = */ 1);
    if enumeration.count == 0 {
        return Err("PCIe link did not train (or firmware did not bring it up)");
    }
    Ok(enumeration)
}
