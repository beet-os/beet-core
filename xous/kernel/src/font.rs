// SPDX-FileCopyrightText: 2025 BeetOS contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! 8×16 bitmap font for the framebuffer console.
//!
//! Source: `font8x8` crate (MIT). Each 8×8 glyph is drawn with every row
//! doubled vertically, giving 8px wide × 16px tall character cells at
//! `scale = 1`. At `scale = 2` (HiDPI) cells are 16px × 32px.
//!
//! Bit layout of each source row byte: bit 7 = leftmost pixel.

/// Character cell width in pixels at scale = 1.
pub const CHAR_W: usize = 8;

/// Character cell height in pixels at scale = 1.
/// The 8-row source glyph is drawn with each row repeated twice → 16 rows.
pub const CHAR_H: usize = 16;

/// Draw one character cell at pixel position `(x, y)` into the framebuffer.
///
/// - `fb`     — pointer to the first pixel (XRGB8888, one `u32` per pixel)
/// - `stride` — pixels per row (framebuffer width, **not** bytes)
/// - `x`, `y` — top-left corner of the cell in pixels
/// - `c`      — character to draw (non-ASCII renders as space)
/// - `fg`/`bg`— colours as `0x00RRGGBB`
/// - `scale`  — pixel scale factor (1 = 8×16 cell, 2 = 16×32 HiDPI cell)
pub fn draw_char(
    fb: *mut u32,
    stride: usize,
    x: usize,
    y: usize,
    c: u8,
    fg: u32,
    bg: u32,
    scale: usize,
) {
    let idx = if c < 128 { c as usize } else { 0x20 };
    let glyph = font8x8::legacy::BASIC_LEGACY[idx];

    for (src_row, &bits) in glyph.iter().enumerate() {
        for col in 0..CHAR_W {
            let pixel = if (bits >> (7 - col)) & 1 != 0 { fg } else { bg };

            // Each source row is painted `2 * scale` pixel rows tall,
            // and each source column is `scale` pixels wide.
            for vr in 0..(2 * scale) {
                for hc in 0..scale {
                    let px = x + col * scale + hc;
                    let py = y + src_row * 2 * scale + vr;

                    unsafe {
                        fb.add(py * stride + px).write_volatile(pixel);
                    }
                }
            }
        }
    }
}
