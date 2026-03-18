// SPDX-FileCopyrightText: 2024 BeetOS contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Apple AIC interrupt number definitions for userspace.
//!
//! These correspond to Apple Interrupt Controller (AIC) hardware IRQ numbers.
//! The actual numbers will be determined from the device tree in M2.

/// Apple AIC interrupt numbers.
///
/// These are placeholders — actual IRQ numbers come from the AIC device tree
/// node in M2. Each IRQ type corresponds to a hardware interrupt source
/// on the Apple M1 (T8103) SoC.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum IrqNumber {
    /// ARM Generic Timer (physical timer, EL1)
    Timer = 1,
    /// Apple SPI controller 0 (keyboard)
    Spi0 = 2,
    /// Apple SPI controller 1
    Spi1 = 3,
    /// Apple UART 0 (serial console)
    Uart0 = 4,
    /// Apple NVMe/ANS controller
    Nvme = 5,
    /// Apple DART (IOMMU)
    Dart = 6,
    /// USB xHCI controller
    Usb = 7,
    /// Wi-Fi (BCM4378)
    Wifi = 8,
}

impl TryFrom<usize> for IrqNumber {
    type Error = crate::Error;

    fn try_from(value: usize) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(IrqNumber::Timer),
            2 => Ok(IrqNumber::Spi0),
            3 => Ok(IrqNumber::Spi1),
            4 => Ok(IrqNumber::Uart0),
            5 => Ok(IrqNumber::Nvme),
            6 => Ok(IrqNumber::Dart),
            7 => Ok(IrqNumber::Usb),
            8 => Ok(IrqNumber::Wifi),
            _ => Err(crate::Error::InvalidLimit),
        }
    }
}
