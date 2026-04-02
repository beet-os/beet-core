// SPDX-FileCopyrightText: 2025 BeetOS contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! QEMU ramfb framebuffer via the FW_CFG DMA interface.
//!
//! QEMU's `ramfb` device lets the guest choose a physical RAM address,
//! write pixels there, and have QEMU render the region to its display
//! window. The guest configures ramfb by writing a descriptor to the
//! FW_CFG file `etc/ramfb` through the FW_CFG MMIO DMA interface.
//!
//! Boot flow:
//!   1. `init()` — enumerate FW_CFG files, find `etc/ramfb`, write config
//!   2. The framebuffer lives at `FB_PHYS` (reserved, not given to MemoryManager)
//!   3. `write_str(s)` — draw text; call this after `init()` succeeds
//!
//! FW_CFG MMIO address: `0x0902_0000` (fixed for QEMU virt; confirmed via FDT).
//! Framebuffer address: `FB_PHYS` — top of the kernel's 1 GB RAM window.

use core::ptr::{addr_of_mut, read_volatile, write_volatile};

use beetos::{phys_to_virt, virt_to_phys};

use crate::fb_console::FbConsole;

// ─────────────────────────────────────────────────────────────────────────────
// Framebuffer layout
// ─────────────────────────────────────────────────────────────────────────────

/// Physical address of the framebuffer (top 4 MB of QEMU's 1 GB RAM window).
/// Must match the reservation subtracted from `ram_size` in boot.rs.
pub const FB_PHYS: usize = 0x7FC0_0000;

/// Framebuffer width in pixels.
pub const FB_WIDTH: usize = 1280;

/// Framebuffer height in pixels.
pub const FB_HEIGHT: usize = 800;

/// Bytes per row (XRGB8888 = 4 bytes per pixel).
pub const FB_STRIDE_BYTES: usize = FB_WIDTH * 4;

/// Total framebuffer size reserved in physical RAM (multiple of 16 KB page).
pub const FB_SIZE: usize = 4 * 1024 * 1024; // 4 MB


// ─────────────────────────────────────────────────────────────────────────────
// FW_CFG constants
// ─────────────────────────────────────────────────────────────────────────────

/// FW_CFG MMIO base (physical). Fixed for QEMU virt; confirmed via DTB dump.
const FWCFG_PHYS: usize = 0x0902_0000;

/// FW_CFG MMIO register offsets.
const FWCFG_DATA:     usize = 0x00; // u8  — sequential data R/W
const FWCFG_SELECTOR: usize = 0x08; // u16 — key selector (write BE16)
const FWCFG_DMA:      usize = 0x10; // u64 — DMA submit (write BE64)

/// Standard key for the file directory.
const FW_CFG_FILE_DIR: u16 = 0x0019;

/// DMA control: select a new key (upper 16 bits of control = key index).
const DMA_CTL_SELECT: u32 = 0x08;

/// DMA control: write guest memory → FW_CFG file.
const DMA_CTL_WRITE: u32 = 0x10;

/// DRM fourcc for XRGB8888 ("XR24" in little-endian).
const DRM_FORMAT_XRGB8888: u32 = 0x3432_5258;

// ─────────────────────────────────────────────────────────────────────────────
// Wire-format structs
// ─────────────────────────────────────────────────────────────────────────────

/// RamFB configuration written to FW_CFG `etc/ramfb` (all fields big-endian).
///
/// `packed` matches QEMU's `QEMU_PACKED` C struct (28 bytes, no padding).
#[repr(C, packed)]
struct RamFbCfg {
    addr:   u64, // framebuffer physical address
    fourcc: u32, // DRM pixel format code
    flags:  u32, // reserved, must be 0
    width:  u32, // pixels
    height: u32, // pixels
    stride: u32, // bytes per row
}

/// FW_CFG DMA access descriptor (all fields big-endian, 8-byte aligned).
#[repr(C, align(8))]
struct FwCfgDmaAccess {
    control: u32, // flags | (key << 16)
    length:  u32, // byte count for the transfer
    address: u64, // guest physical address of data buffer
}

// Static storage — physical addresses obtained at runtime via `virt_to_phys`.
static mut DMA_ACCESS: FwCfgDmaAccess = FwCfgDmaAccess { control: 0, length: 0, address: 0 };
static mut RAMFB_CFG:  RamFbCfg       = RamFbCfg { addr: 0, fourcc: 0, flags: 0, width: 0, height: 0, stride: 0 };

/// Global console instance (initialised once in `init()`).
static mut FB_CONSOLE: Option<FbConsole> = None;

// ─────────────────────────────────────────────────────────────────────────────
// Low-level FW_CFG access
// ─────────────────────────────────────────────────────────────────────────────

fn fwcfg_va() -> usize {
    phys_to_virt(FWCFG_PHYS)
}

/// Select a FW_CFG key (write BE16 to selector register).
unsafe fn fwcfg_select(key: u16) {
    write_volatile((fwcfg_va() + FWCFG_SELECTOR) as *mut u16, key.to_be());
}

/// Read one byte from the sequential data register.
unsafe fn fwcfg_read_u8() -> u8 {
    read_volatile((fwcfg_va() + FWCFG_DATA) as *const u8)
}

unsafe fn fwcfg_read_be16() -> u16 {
    let hi = fwcfg_read_u8() as u16;
    let lo = fwcfg_read_u8() as u16;
    (hi << 8) | lo
}

unsafe fn fwcfg_read_be32() -> u32 {
    let b0 = fwcfg_read_u8() as u32;
    let b1 = fwcfg_read_u8() as u32;
    let b2 = fwcfg_read_u8() as u32;
    let b3 = fwcfg_read_u8() as u32;
    (b0 << 24) | (b1 << 16) | (b2 << 8) | b3
}

/// Submit a DMA request using two 32-bit writes.
///
/// Writing the high half first sets the pending address; writing the low
/// half triggers the DMA transfer. Both halves are big-endian (the DMA
/// region uses DEVICE_BIG_ENDIAN). DSB ISH ensures prior Normal-memory
/// writes to DMA_ACCESS and RAMFB_CFG are visible before the trigger.
unsafe fn fwcfg_dma_submit() {
    core::arch::asm!("dsb ish", options(nomem, nostack));

    let dma_phys = virt_to_phys(addr_of_mut!(DMA_ACCESS) as usize) as u64;
    let hi: u32 = (dma_phys >> 32) as u32;
    let lo: u32 = (dma_phys & 0xFFFF_FFFF) as u32;

    // Write high 32 bits (sets pending address, no trigger)
    write_volatile((fwcfg_va() + FWCFG_DMA) as *mut u32, hi.to_be());
    // Write low 32 bits (sets remaining address bits and triggers DMA)
    write_volatile((fwcfg_va() + FWCFG_DMA + 4) as *mut u32, lo.to_be());

    core::arch::asm!("dsb ish; isb", options(nomem, nostack));
}

// ─────────────────────────────────────────────────────────────────────────────
// RamFB setup
// ─────────────────────────────────────────────────────────────────────────────

/// Enumerate the FW_CFG file directory and return the key for `etc/ramfb`.
unsafe fn find_ramfb_key() -> Option<u16> {
    fwcfg_select(FW_CFG_FILE_DIR);

    let count = fwcfg_read_be32();

    for _ in 0..count {
        // Each directory entry: u32 size | u16 select | u16 pad | [u8; 56] name
        let _size  = fwcfg_read_be32();
        let select = fwcfg_read_be16();
        let _pad   = fwcfg_read_be16();

        let mut name = [0u8; 56];
        for b in name.iter_mut() {
            *b = fwcfg_read_u8();
        }

        if name.starts_with(b"etc/ramfb\0") {
            return Some(select);
        }
    }

    None
}

/// Write the ramfb configuration to FW_CFG via the DMA interface.
unsafe fn write_ramfb_cfg(key: u16) {
    // Fill RamFbCfg (all big-endian).
    let cfg = addr_of_mut!(RAMFB_CFG);
    write_volatile(addr_of_mut!((*cfg).addr),   (FB_PHYS as u64).to_be());
    write_volatile(addr_of_mut!((*cfg).fourcc), DRM_FORMAT_XRGB8888.to_be());
    write_volatile(addr_of_mut!((*cfg).flags),  0u32.to_be());
    write_volatile(addr_of_mut!((*cfg).width),  (FB_WIDTH as u32).to_be());
    write_volatile(addr_of_mut!((*cfg).height), (FB_HEIGHT as u32).to_be());
    write_volatile(addr_of_mut!((*cfg).stride), (FB_STRIDE_BYTES as u32).to_be());

    let cfg_phys = virt_to_phys(cfg as usize) as u64;
    let dma_len  = core::mem::size_of::<RamFbCfg>() as u32;

    // Fill DMA access descriptor (all big-endian).
    let dma = addr_of_mut!(DMA_ACCESS);
    let ctrl: u32 = ((key as u32) << 16) | DMA_CTL_SELECT | DMA_CTL_WRITE;
    write_volatile(addr_of_mut!((*dma).control), ctrl.to_be());
    write_volatile(addr_of_mut!((*dma).length),  dma_len.to_be());
    write_volatile(addr_of_mut!((*dma).address), cfg_phys.to_be());

    fwcfg_dma_submit();
}

// ─────────────────────────────────────────────────────────────────────────────
// Public API
// ─────────────────────────────────────────────────────────────────────────────

/// Initialise the QEMU ramfb framebuffer and the text console.
///
/// Must be called after MMU is enabled (TTBR1 linear map is active).
/// Returns `true` on success, `false` if `etc/ramfb` was not found in FW_CFG
/// (e.g., QEMU was launched without `-device ramfb`).
pub unsafe fn init() -> bool {
    let key = match find_ramfb_key() {
        Some(k) => k,
        None => return false,
    };

    write_ramfb_cfg(key);

    // The framebuffer is at a reserved physical address in RAM.
    // Access it through the TTBR1 linear map.
    let fb_va = phys_to_virt(FB_PHYS) as *mut u32;

    // Clear to black.
    let pixels = FB_WIDTH * FB_HEIGHT;
    for i in 0..pixels {
        fb_va.add(i).write_volatile(0x0000_0000);
    }

    // stride in pixels (not bytes)
    FB_CONSOLE = Some(FbConsole::new(fb_va, FB_WIDTH, FB_HEIGHT, FB_WIDTH));

    true
}

/// Write a string to the framebuffer console.
///
/// No-op if `init()` has not been called or returned `false`.
pub fn write_str(s: &str) {
    unsafe {
        if let Some(ref mut con) = FB_CONSOLE {
            for b in s.bytes() {
                con.putc(b);
            }
        }
    }
}
