// SPDX-FileCopyrightText: 2025 BeetOS contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Framebuffer text console.
//!
//! Maintains a cursor and scrolls the screen up by one text row when the
//! bottom is reached. Characters are rendered via `crate::font::draw_char`.

use crate::font;

/// Foreground colour — light gray (0x00RRGGBB).
const FG: u32 = 0x00CC_CCCC;

/// Background colour — black.
const BG: u32 = 0x0000_0000;

/// Pixel scale factor applied to every character cell.
/// 1 = 8×16 px cells (1080p / 1440p), 2 = 16×32 px cells (4K / Retina).
const SCALE: usize = 1;

pub struct FbConsole {
    fb:     *mut u32, // framebuffer base (XRGB8888, one u32 per pixel)
    width:  usize,    // framebuffer width in pixels
    height: usize,    // framebuffer height in pixels
    stride: usize,    // pixels per row (not bytes)
    cols:   usize,    // text columns
    rows:   usize,    // text rows
    col:    usize,    // cursor column
    row:    usize,    // cursor row
}

impl FbConsole {
    /// Create a new console backed by `fb`.
    ///
    /// - `stride` is the number of **pixels** (u32 values) per row.
    pub fn new(fb: *mut u32, width: usize, height: usize, stride: usize) -> Self {
        let cols = width  / (font::CHAR_W * SCALE);
        let rows = height / (font::CHAR_H * SCALE);

        FbConsole { fb, width, height, stride, cols, rows, col: 0, row: 0 }
    }

    /// Write one byte to the console.
    pub fn putc(&mut self, c: u8) {
        match c {
            b'\n' => {
                self.col = 0;
                self.advance_row();
            }
            b'\r' => {
                self.col = 0;
            }
            b'\x08' | b'\x7F' => {
                // Backspace
                if self.col > 0 {
                    self.col -= 1;
                    self.draw(b' ', self.col, self.row);
                }
            }
            c => {
                self.draw(c, self.col, self.row);
                self.col += 1;

                if self.col >= self.cols {
                    self.col = 0;
                    self.advance_row();
                }
            }
        }
    }

    /// Clear the screen and reset the cursor.
    #[allow(dead_code)]
    pub fn clear(&mut self) {
        let total = self.width * self.height;

        for i in 0..total {
            unsafe { self.fb.add(i).write_volatile(BG); }
        }

        self.col = 0;
        self.row = 0;
    }

    fn draw(&self, c: u8, col: usize, row: usize) {
        let x = col * font::CHAR_W * SCALE;
        let y = row * font::CHAR_H * SCALE;
        font::draw_char(self.fb, self.stride, x, y, c, FG, BG, SCALE);
    }

    fn advance_row(&mut self) {
        self.row += 1;

        if self.row >= self.rows {
            self.scroll();
        }
    }

    /// Scroll the display up by one text row.
    fn scroll(&mut self) {
        let row_pixels = font::CHAR_H * SCALE; // pixel rows per text row
        let row_words  = row_pixels * self.stride; // u32 words per text row
        let total      = self.width * self.height;

        unsafe {
            core::ptr::copy(
                self.fb.add(row_words),
                self.fb,
                total - row_words,
            );

            // Clear the last text row.
            for i in (total - row_words)..total {
                self.fb.add(i).write_volatile(BG);
            }
        }

        self.row = self.rows - 1;
    }
}

impl core::fmt::Write for FbConsole {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        for b in s.bytes() {
            self.putc(b);
        }

        Ok(())
    }
}
