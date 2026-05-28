// SPDX-FileCopyrightText: 2025 BeetOS contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! BCM2712 SDHCI controller glue.
//!
//! The generic SDHCI v3.0 protocol + host driver lives in
//! [`crate::sdhci`] (unit-tested against a MockMmio). This module
//! is the thin BCM-specific layer: the controller's MMIO base
//! address, a [`RealMmio`] wrapper, and a top-level `init` that
//! resets the host and runs the SD card init state machine.
//!
//! BCM2712 ships two SDHCI-compatible blocks:
//!   - `sd_host`  at `0x107D510000` — SD card slot
//!   - `emmc2`    at `0x107D580000` — on-board eMMC (not on RPi5)
//!
//! We target `sd_host` because that's what every consumer RPi5
//! board uses for booting.
//!
//! Reference: Linux `drivers/mmc/host/sdhci-iproc.c` (BCM2711, very
//! close to BCM2712) + the RPi5 device tree
//! `brcm,bcm2712-mmc-host` node.

use core::ptr::{read_volatile, write_volatile};

use crate::sdhci::{
    self, advance, next_command, InitStep, Host, HostError, Mmio, ResponseType, SdCommand,
};

/// SD card slot SDHCI MMIO base (BCM2712 physical address).
pub const SDHCI_PHYS: usize = 0x107D_510000;

/// SDHCI register window is 4 KB on this controller (standard v3.0
/// register file is well under 256 bytes; the rest is vendor
/// reserved space we don't touch).
pub const SDHCI_SIZE: usize = 4096;

/// Real-hardware MMIO accessor — wraps a raw pointer into the BCM
/// SDHCI register region, accessed through TTBR1 high-VA.
pub struct RealMmio {
    base: *mut u8,
}

impl RealMmio {
    /// # Safety
    ///
    /// `base` must be the VA of a Device-mapped MMIO region of
    /// length [`SDHCI_SIZE`] for the lifetime of `Self`.
    pub const unsafe fn new(base_va: usize) -> Self {
        Self { base: base_va as *mut u8 }
    }
}

impl Mmio for RealMmio {
    fn read32(&self, off: usize) -> u32 {
        debug_assert!(off + 4 <= SDHCI_SIZE);
        unsafe { read_volatile(self.base.add(off) as *const u32) }
    }
    fn write32(&self, off: usize, value: u32) {
        debug_assert!(off + 4 <= SDHCI_SIZE);
        unsafe { write_volatile(self.base.add(off) as *mut u32, value) }
    }
}

/// One slot of cached card info discovered during init.
#[derive(Clone, Copy, Debug)]
pub struct CardInfo {
    pub rca: u16,
    pub csd: sdhci::Csd,
    pub blocks: u64,
}

/// Try to bring up the SD card: reset the host, walk the init state
/// machine, and return the negotiated [`CardInfo`] on success.
///
/// Each state machine step issues a real SDHCI command — for ACMD41
/// we send CMD55 first per the SD protocol. CID is captured at
/// AllSendCid time so the caller can log it; the canonical RCA +
/// CSD live in the returned struct.
///
/// On real hardware this is the entry point a future block-device
/// service would call. It's marked `unsafe` because it constructs
/// the MMIO wrapper from a raw VA which must outlive the call.
///
/// # Safety
///
/// `sdhci_va` must map [`SDHCI_SIZE`] bytes of Device-attribute
/// MMIO covering the BCM2712 SD host controller.
pub unsafe fn init(sdhci_va: usize) -> Result<CardInfo, HostError> {
    let mmio = RealMmio::new(sdhci_va);
    let host = Host::new(&mmio);

    host.reset_all()?;

    let mut step = InitStep::GoIdle;
    let mut cached_rca: u16 = 0;

    // Drive the state machine until Done or Error. Each iteration
    // computes the SD command, issues it via the host, and feeds the
    // response back through `advance`. SendOpCond gets a CMD55 prelude
    // every retry (APP_CMD prefix).
    loop {
        match step {
            InitStep::Done { rca, csd, .. } => {
                return Ok(CardInfo {
                    rca,
                    csd,
                    blocks: csd.blocks_512(),
                });
            }
            InitStep::Error(_) => return Err(HostError::BadResponse),
            _ => {}
        }

        // ACMD41 must be preceded by CMD55 with RCA 0 during init.
        if matches!(step, InitStep::SendOpCond { .. }) {
            let _ = host.issue_command(
                SdCommand::new(sdhci::cmd::APP_CMD, 0, ResponseType::R1),
                /* data_present = */ false,
            )?;
        }

        let cmd = match next_command(step) {
            Some(c) => c,
            None    => return Err(HostError::BadResponse),
        };
        let resp = host.issue_command(cmd, /* data_present = */ false)?;

        // The controller packs R6 RCA into RESPONSE0's high word; we
        // also stash it in cached_rca for the eventual block ops.
        if matches!(step, InitStep::SendRelativeAddr) {
            cached_rca = (resp.short() >> 16) as u16;
        }

        step = advance(step, resp.short(), resp.long());
        let _ = cached_rca; // currently unused — wired in a follow-up
                            // commit when block I/O lands.
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The init() function itself is not directly testable here (it
    // touches RealMmio which dereferences a raw pointer). But the
    // constants matter — they're the BCM2712 SDHCI MMIO base + size
    // numbers that have to match the device tree exactly.
    #[test]
    fn sdhci_base_matches_bcm2712_dts() {
        // From upstream RPi5 device tree:
        //   mmc@7d510000 { reg = <0x7d510000 0x300> ... }
        // Same address with the BCM2712 peripheral base prefix
        // (0x1000_000000 in the ARM-side view).
        assert_eq!(SDHCI_PHYS, 0x107D_510000);
        // Window is at least one page so RealMmio bounds check is
        // safe up to 0xFFF.
        assert!(SDHCI_SIZE >= 4096);
    }
}
