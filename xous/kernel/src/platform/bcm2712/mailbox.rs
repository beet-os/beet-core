// SPDX-FileCopyrightText: 2025 BeetOS contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! BCM2712 (and BCM2835/2837/2711 — protocol is unchanged) mailbox driver.
//!
//! The "mailbox" is the ARM ↔ VideoCore inter-processor bell+queue used
//! to ask the firmware for things the ARM cores can't poke directly —
//! most importantly the framebuffer descriptor the firmware sets up
//! during `start4.elf` boot. We use channel 8 ("property tags"), the
//! universal request-response protocol that BCM has shipped since 2012:
//!
//!   1. Build a u32-aligned "property buffer" in writable RAM:
//!        [total_size, request_code,
//!         tag_id, tag_buf_size, tag_request_code, ...payload...,
//!         tag_id, tag_buf_size, tag_request_code, ...payload...,
//!         END (0)]
//!   2. Write `(buf_phys & ~0xF) | channel` to MBOX_WRITE.
//!   3. Poll MBOX_STATUS until response present, then read MBOX_READ
//!      until you get back the same word you wrote.
//!   4. The firmware overwrites the buffer in place: each tag's
//!      `request_code` slot becomes 0x8000_0000 | response_len.
//!
//! On BCM2712 the mailbox MMIO lives at the same logical offset as on
//! all previous Pi chips (`MBOX_PHYS` below), just at the BCM2712
//! peripheral base (0x107C00_0000) instead of the 32-bit-era
//! 0x3F000000 / 0xFE000000.
//!
//! References:
//!   - https://github.com/raspberrypi/firmware/wiki/Mailbox-property-interface
//!   - Linux `drivers/mailbox/bcm2835-mailbox.c`
//!
//! Status: pure MMIO with no allocator, suitable for early boot. NOT
//! exercised by `cargo xtask qemu-smoke` because QEMU 8.2.2 has no
//! raspi5; written against the documented protocol and the existing
//! Raspberry Pi firmware contract.

use beetos::phys_to_virt;
use core::ptr::{read_volatile, write_volatile};

/// BCM2712 mailbox MMIO base (ARM-side physical address).
/// On older Pi chips this was at 0x3F00B880 / 0xFE00B880; the offset
/// from the peripheral base is the same, only the base moved.
pub const MBOX_PHYS: usize = 0x107C_01_3880;

/// Mailbox register offsets from `MBOX_PHYS`.
const MBOX_READ:   usize = 0x00;
const MBOX_POLL:   usize = 0x10;
const MBOX_SENDER: usize = 0x14;
const MBOX_STATUS: usize = 0x18;
const MBOX_CONFIG: usize = 0x1C;
const MBOX_WRITE:  usize = 0x20;

/// Bits in MBOX_STATUS.
const MBOX_FULL:  u32 = 0x8000_0000;
const MBOX_EMPTY: u32 = 0x4000_0000;

/// "Property tags" channel — the high-level request-response protocol.
const CHANNEL_PROPERTY: u32 = 8;

/// Mark a property tag header's request_code; the firmware overwrites
/// it with `RESPONSE_OK | response_len` (or `RESPONSE_ERR`).
const REQUEST: u32 = 0x0000_0000;

/// Tag IDs we care about.
const TAG_END:                 u32 = 0x0000_0000;
const TAG_GET_FB:              u32 = 0x0004_0001; // allocate FB
const TAG_GET_FB_PITCH:        u32 = 0x0004_0008; // bytes-per-row
const TAG_SET_PHYS_WH:         u32 = 0x0004_8003; // physical w/h
const TAG_SET_VIRT_WH:         u32 = 0x0004_8004; // virtual w/h
const TAG_SET_DEPTH:           u32 = 0x0004_8005; // bits per pixel
const TAG_SET_PIXEL_ORDER:     u32 = 0x0004_8006; // 0=BGR, 1=RGB

/// Pixel order constant for "RGB" — XRGB8888 in BCM speak.
const PIXEL_ORDER_RGB: u32 = 1;

/// 16 KB page-aligned property buffer. Mailbox channel 8 only uses the
/// low 28 bits of the write word for the address, so the buffer's
/// physical address must be 16-byte aligned; we go fully page-aligned
/// to keep the cache invariants clean.
#[repr(C, align(16384))]
struct MboxBuf {
    words: [u32; 64],
}

static mut MBOX_BUF: MboxBuf = MboxBuf { words: [0; 64] };

fn reg(off: usize) -> usize {
    phys_to_virt(MBOX_PHYS) + off
}

unsafe fn rd(off: usize) -> u32 {
    read_volatile(reg(off) as *const u32)
}

unsafe fn wr(off: usize, v: u32) {
    write_volatile(reg(off) as *mut u32, v);
}

/// Submit the property buffer over channel 8 and wait for the firmware
/// to overwrite it with the response.  Returns `Ok(())` on a clean
/// round-trip (firmware mirrored back the same write word and the
/// header's request_code now contains the response success bit).
unsafe fn submit_property() -> Result<(), ()> {
    let buf_pa = beetos::virt_to_phys(core::ptr::addr_of_mut!(MBOX_BUF) as usize) as u32;
    let write_word = (buf_pa & !0xF) | CHANNEL_PROPERTY;

    // Wait for the mailbox to drain.
    while rd(MBOX_STATUS) & MBOX_FULL != 0 {}
    // Ensure prior buffer writes are visible to the VideoCore side.
    core::arch::asm!("dsb sy", options(nomem, nostack));
    wr(MBOX_WRITE, write_word);

    // Wait for the response and confirm it's ours (multiple consumers
    // is fine; we just discard messages on other channels).
    loop {
        while rd(MBOX_STATUS) & MBOX_EMPTY != 0 {}
        let resp = rd(MBOX_READ);
        if resp == write_word { break; }
    }
    core::arch::asm!("dsb sy", options(nomem, nostack));

    // Check the property header's response code: bit 31 means "OK".
    let header_resp = (*core::ptr::addr_of_mut!(MBOX_BUF)).words[1];
    if header_resp & 0x8000_0000 != 0 { Ok(()) } else { Err(()) }
}

/// A successfully-allocated framebuffer as reported by the firmware.
#[derive(Clone, Copy, Debug)]
pub struct FbInfo {
    pub addr:   usize,
    pub size:   usize,
    pub width:  u32,
    pub height: u32,
    pub pitch:  u32, // bytes per row
    pub bpp:    u32, // bits per pixel (32 for XRGB8888)
}

/// Ask the firmware to allocate (or report the already-allocated)
/// framebuffer at the requested resolution. Single chained request
/// that sets phys/virt size, depth, pixel order, and asks for the FB
/// address + pitch all in one mailbox round-trip.
///
/// Returns `None` if the mailbox is unreachable or the firmware
/// refused the request (typical reasons: no display attached, or
/// requested resolution unsupported). Caller can fall back to UART.
pub fn alloc_framebuffer(width: u32, height: u32) -> Option<FbInfo> {
    unsafe {
        let buf = core::ptr::addr_of_mut!(MBOX_BUF);
        let words = &mut (*buf).words;

        // Layout: [total_size, request, ...tags..., END]
        // Indexing it by hand keeps the no_alloc / no_format property and
        // documents the byte offsets the spec describes.
        let mut i = 0;
        macro_rules! push { ($w:expr) => {{ words[i] = $w; i += 1; }} }

        push!(0);                // total size (patched below)
        push!(REQUEST);          // request code

        // Tag: set physical w/h
        push!(TAG_SET_PHYS_WH);
        push!(8);                // tag value buffer size (bytes)
        push!(8);                // request: 8 bytes of payload follow
        push!(width);
        push!(height);

        // Tag: set virtual w/h (same as physical — no virtual scrolling)
        push!(TAG_SET_VIRT_WH);
        push!(8);
        push!(8);
        push!(width);
        push!(height);

        // Tag: set depth (32 bpp)
        push!(TAG_SET_DEPTH);
        push!(4);
        push!(4);
        push!(32);

        // Tag: set pixel order (RGB so the byte layout matches XRGB8888)
        push!(TAG_SET_PIXEL_ORDER);
        push!(4);
        push!(4);
        push!(PIXEL_ORDER_RGB);

        // Tag: allocate FB — request_code passes alignment; firmware
        // replaces buffer with (addr, size)
        push!(TAG_GET_FB);
        push!(8);
        push!(4);                // 4 bytes of request payload: alignment
        push!(4096);             // 4 KB-aligned FB (safe on all chips)
        push!(0);                // response slot for size

        // Tag: get pitch (bytes per row)
        push!(TAG_GET_FB_PITCH);
        push!(4);
        push!(0);                // request payload length zero
        push!(0);                // response slot

        push!(TAG_END);

        // Patch total size (in bytes, includes the size word itself).
        words[0] = (i as u32) * 4;

        submit_property().ok()?;

        // Walk the response tags by reusing our layout positions. The
        // FB allocation tag is at offset 22 (after 5 set-tags of 5
        // words each = 25 words, minus 3 header — recalculate
        // explicitly to be safe).
        // Header is 2 words, then tag layout repeats:
        //   set_phys_wh  : 5 words → ends at index 7
        //   set_virt_wh  : 5 words → ends at 12
        //   set_depth    : 4 words → ends at 16
        //   set_pixel_ord: 4 words → ends at 20
        //   get_fb       : 5 words → starts at 20, payload at [23..=24]
        //   get_pitch    : 4 words → starts at 25, payload at [28]
        let fb_addr = words[23] & 0x3FFF_FFFF; // upper bits are cache hints on some firmware
        let fb_size = words[24];
        let pitch   = words[28];

        if fb_addr == 0 { return None; }

        Some(FbInfo {
            addr:   fb_addr as usize,
            size:   fb_size as usize,
            width,
            height,
            pitch,
            bpp:    32,
        })
    }
}
