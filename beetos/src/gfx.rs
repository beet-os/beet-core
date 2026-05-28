// SPDX-FileCopyrightText: 2025 BeetOS contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Software-rendered 2D graphics primitives.
//!
//! This module exposes a [`Surface`] — a mutable view of pixel memory in
//! XRGB8888 — together with a small library of drawing primitives that
//! operate on it: pixels, lines, rectangles, circles, blits, and (via
//! [`crate::font`]) text. The shape of the API is intentionally close to
//! what `wgpu` calls a "Surface" / "Texture" so that a future hardware
//! backend (virtio-gpu on QEMU, VideoCore VII on RPi5, AGX on M1) can
//! swap in without touching the widget code that draws on top.
//!
//! The whole module is `no_std` and allocation-free: each primitive
//! takes a `&mut Surface` (or borrows a sub-surface) and writes pixels
//! through a raw pointer. Bounds checks happen at the surface boundary,
//! not on every pixel, so the inner loops stay tight enough to keep
//! 1280×800 boot graphics interactive in QEMU.

use crate::font;

// ─────────────────────────────────────────────────────────────────────────────
// Color helpers
// ─────────────────────────────────────────────────────────────────────────────

/// A pixel in XRGB8888 (`0x00_RR_GG_BB`).  The high byte is ignored by
/// QEMU's ramfb but kept zero so the same value can be written to true
/// 32-bpp DRM XR24 surfaces unchanged.
pub type Color = u32;

/// Pack 8-bit R/G/B components into a [`Color`].
#[inline]
pub const fn rgb(r: u8, g: u8, b: u8) -> Color {
    ((r as u32) << 16) | ((g as u32) << 8) | (b as u32)
}

/// Curated 16-color palette — same order as the IBM-CGA / xterm-16
/// set, picked to look reasonable on a dark FB. Indices match the
/// ANSI 30-37 / 90-97 escape codes when we eventually wire up `\x1b[…m`.
pub mod color {
    use super::{rgb, Color};

    pub const BLACK:       Color = rgb(0x00, 0x00, 0x00);
    pub const RED:         Color = rgb(0xCD, 0x00, 0x00);
    pub const GREEN:       Color = rgb(0x00, 0xCD, 0x00);
    pub const YELLOW:      Color = rgb(0xCD, 0xCD, 0x00);
    pub const BLUE:        Color = rgb(0x00, 0x00, 0xCD);
    pub const MAGENTA:     Color = rgb(0xCD, 0x00, 0xCD);
    pub const CYAN:        Color = rgb(0x00, 0xCD, 0xCD);
    pub const LIGHT_GRAY:  Color = rgb(0xCC, 0xCC, 0xCC);

    pub const DARK_GRAY:    Color = rgb(0x40, 0x40, 0x40);
    pub const BRIGHT_RED:   Color = rgb(0xFF, 0x00, 0x00);
    pub const BRIGHT_GREEN: Color = rgb(0x00, 0xFF, 0x00);
    pub const BRIGHT_YELLOW:Color = rgb(0xFF, 0xFF, 0x00);
    pub const BRIGHT_BLUE:  Color = rgb(0x42, 0x85, 0xF4);
    pub const BRIGHT_MAGENTA:Color = rgb(0xFF, 0x00, 0xFF);
    pub const BRIGHT_CYAN:  Color = rgb(0x00, 0xFF, 0xFF);
    pub const WHITE:        Color = rgb(0xFF, 0xFF, 0xFF);

    // Useful UI palette extensions.
    pub const BEET_PURPLE:  Color = rgb(0x6E, 0x29, 0x80); // accent
    pub const BEET_PINK:    Color = rgb(0xC0, 0x39, 0x8B); // accent-light
    pub const DESKTOP_BG:   Color = rgb(0x14, 0x1A, 0x24);
    pub const WINDOW_BG:    Color = rgb(0x1E, 0x26, 0x33);
    pub const WINDOW_FRAME: Color = rgb(0x3A, 0x47, 0x5C);
    pub const TITLE_BAR:    Color = rgb(0x29, 0x33, 0x44);
    pub const TITLE_FG:     Color = rgb(0xE0, 0xE5, 0xEE);
}

// ─────────────────────────────────────────────────────────────────────────────
// Rect
// ─────────────────────────────────────────────────────────────────────────────

/// A non-empty rectangle in pixel coordinates.  Stored as left/top + width/height
/// because that matches how every primitive iterates (left→right, top→bottom).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl Rect {
    pub const fn new(x: i32, y: i32, w: i32, h: i32) -> Self { Self { x, y, w, h } }
    pub const fn from_xywh(x: i32, y: i32, w: i32, h: i32) -> Self { Self { x, y, w, h } }
    pub fn right(&self)  -> i32 { self.x + self.w }
    pub fn bottom(&self) -> i32 { self.y + self.h }
    pub fn is_empty(&self) -> bool { self.w <= 0 || self.h <= 0 }

    /// Intersection of two rects, or an empty rect if disjoint.
    pub fn intersect(&self, other: &Rect) -> Rect {
        let x = self.x.max(other.x);
        let y = self.y.max(other.y);
        let r = self.right().min(other.right());
        let b = self.bottom().min(other.bottom());
        Rect { x, y, w: (r - x).max(0), h: (b - y).max(0) }
    }

    pub fn contains(&self, x: i32, y: i32) -> bool {
        x >= self.x && x < self.right() && y >= self.y && y < self.bottom()
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Surface — a writable rectangular region of pixels.
// ─────────────────────────────────────────────────────────────────────────────

/// A writable rectangular region of XRGB8888 pixels.
///
/// `Surface` does NOT own its memory — it's a thin (pointer, dims, stride)
/// view, like a `&mut [u32]` shaped into a 2D rectangle. Callers are
/// responsible for keeping the backing storage alive and unique for the
/// surface's lifetime; that contract is encoded in [`Surface::from_raw_parts`]
/// being `unsafe`.
///
/// Pixels are written in row-major order. `stride` is the number of pixels
/// (NOT bytes) between successive rows in the underlying memory — for the
/// QEMU ramfb, `stride == width`; for sub-surfaces carved out of a larger
/// buffer it is whatever the parent surface had.
pub struct Surface {
    base:   *mut u32,
    width:  i32,
    height: i32,
    stride: i32, // pixels per row
}

// `Surface` is `!Send + !Sync` by default through the raw pointer; that's
// what we want — the kernel passes ownership explicitly when it hands a
// surface to a user process.

impl Surface {
    /// Wrap a raw framebuffer pointer.
    ///
    /// # Safety
    ///
    /// - `base` must point to at least `stride * height` writable `u32`s.
    /// - The caller must guarantee no other `&mut` access overlaps for the
    ///   surface's lifetime.
    /// - `width`, `height`, `stride` must be non-negative; `width <= stride`.
    #[inline]
    pub unsafe fn from_raw_parts(base: *mut u32, width: i32, height: i32, stride: i32) -> Self {
        debug_assert!(width >= 0 && height >= 0 && stride >= width);
        Self { base, width, height, stride }
    }

    #[inline] pub fn width(&self)  -> i32 { self.width }
    #[inline] pub fn height(&self) -> i32 { self.height }
    #[inline] pub fn stride(&self) -> i32 { self.stride }
    #[inline] pub fn rect(&self)   -> Rect { Rect::new(0, 0, self.width, self.height) }
    #[inline] pub fn base_ptr(&self) -> *mut u32 { self.base }

    /// Set a single pixel.  Out-of-bounds writes are silently ignored.
    #[inline]
    pub fn set_pixel(&mut self, x: i32, y: i32, color: Color) {
        if x < 0 || y < 0 || x >= self.width || y >= self.height { return; }
        // SAFETY: bounds were just checked; the pointer was validated at construction.
        unsafe { self.base.offset((y * self.stride + x) as isize).write(color); }
    }

    /// Read a single pixel.  Returns `0` for out-of-bounds reads.
    #[inline]
    pub fn get_pixel(&self, x: i32, y: i32) -> Color {
        if x < 0 || y < 0 || x >= self.width || y >= self.height { return 0; }
        unsafe { self.base.offset((y * self.stride + x) as isize).read() }
    }

    /// Fill the whole surface with one colour.
    pub fn fill(&mut self, color: Color) {
        let rect = self.rect();
        self.fill_rect(&rect, color);
    }

    /// Fill an axis-aligned rectangle.
    ///
    /// The rectangle is clipped to the surface; passing a fully off-screen
    /// rect is a no-op, not an error.
    pub fn fill_rect(&mut self, rect: &Rect, color: Color) {
        let clipped = rect.intersect(&self.rect());
        if clipped.is_empty() { return; }
        for row in clipped.y..clipped.bottom() {
            // SAFETY: clipped lies inside the surface bounds.
            unsafe {
                let row_start = self.base.offset((row * self.stride + clipped.x) as isize);
                for col in 0..clipped.w {
                    row_start.offset(col as isize).write(color);
                }
            }
        }
    }

    /// Outline a rectangle with a 1px-wide border.
    pub fn rect_outline(&mut self, rect: &Rect, color: Color) {
        if rect.is_empty() { return; }
        self.hline(rect.x, rect.y,                rect.w, color);
        self.hline(rect.x, rect.bottom() - 1,     rect.w, color);
        self.vline(rect.x,            rect.y,     rect.h, color);
        self.vline(rect.right() - 1,  rect.y,     rect.h, color);
    }

    /// Horizontal run of `len` pixels starting at `(x, y)`.
    #[inline]
    pub fn hline(&mut self, x: i32, y: i32, len: i32, color: Color) {
        let rect = Rect::new(x, y, len, 1);
        self.fill_rect(&rect, color);
    }

    /// Vertical run of `len` pixels starting at `(x, y)`.
    #[inline]
    pub fn vline(&mut self, x: i32, y: i32, len: i32, color: Color) {
        let rect = Rect::new(x, y, 1, len);
        self.fill_rect(&rect, color);
    }

    /// Bresenham's line from `(x0, y0)` to `(x1, y1)` — works for any slope,
    /// including horizontal, vertical, and steep lines.
    pub fn line(&mut self, x0: i32, y0: i32, x1: i32, y1: i32, color: Color) {
        let dx =  (x1 - x0).abs();
        let dy = -(y1 - y0).abs();
        let sx = if x0 < x1 { 1 } else { -1 };
        let sy = if y0 < y1 { 1 } else { -1 };
        let mut err = dx + dy;
        let (mut x, mut y) = (x0, y0);
        loop {
            self.set_pixel(x, y, color);
            if x == x1 && y == y1 { break; }
            let e2 = 2 * err;
            if e2 >= dy { err += dy; x += sx; }
            if e2 <= dx { err += dx; y += sy; }
        }
    }

    /// Bresenham midpoint circle outline centred at `(cx, cy)` with radius `r`.
    pub fn circle(&mut self, cx: i32, cy: i32, r: i32, color: Color) {
        if r < 0 { return; }
        let (mut x, mut y) = (r, 0);
        let mut err: i32 = 1 - r;
        while x >= y {
            self.set_pixel(cx + x, cy + y, color);
            self.set_pixel(cx + y, cy + x, color);
            self.set_pixel(cx - y, cy + x, color);
            self.set_pixel(cx - x, cy + y, color);
            self.set_pixel(cx - x, cy - y, color);
            self.set_pixel(cx - y, cy - x, color);
            self.set_pixel(cx + y, cy - x, color);
            self.set_pixel(cx + x, cy - y, color);
            y += 1;
            if err < 0 {
                err += 2 * y + 1;
            } else {
                x -= 1;
                err += 2 * (y - x) + 1;
            }
        }
    }

    /// Filled circle — drawn as a stack of horizontal scanlines.
    pub fn circle_filled(&mut self, cx: i32, cy: i32, r: i32, color: Color) {
        if r < 0 { return; }
        let (mut x, mut y) = (r, 0);
        let mut err: i32 = 1 - r;
        while x >= y {
            self.hline(cx - x, cy + y, 2 * x + 1, color);
            self.hline(cx - x, cy - y, 2 * x + 1, color);
            self.hline(cx - y, cy + x, 2 * y + 1, color);
            self.hline(cx - y, cy - x, 2 * y + 1, color);
            y += 1;
            if err < 0 {
                err += 2 * y + 1;
            } else {
                x -= 1;
                err += 2 * (y - x) + 1;
            }
        }
    }

    /// Draw a single 8×16 ASCII glyph at `(x, y)` in `fg` on `bg`.
    pub fn draw_char(&mut self, x: i32, y: i32, c: u8, fg: Color, bg: Color) {
        // Delegates to the existing 8×16 glyph painter so we don't carry two
        // copies of the font logic. Out-of-bounds glyphs are clipped via
        // set_pixel rather than rejected wholesale, so partially on-screen
        // characters still render cleanly.
        let idx = if c < 128 { c as usize } else { 0x20 };
        let glyph = font8x8::legacy::BASIC_LEGACY[idx];
        for (src_row, &bits) in glyph.iter().enumerate() {
            for col in 0..font::CHAR_W {
                let on = (bits >> col) & 1 != 0;
                let c  = if on { fg } else { bg };
                // 8x8 glyph is doubled vertically to fill 8x16 cell.
                self.set_pixel(x + col as i32, y + (src_row * 2) as i32,     c);
                self.set_pixel(x + col as i32, y + (src_row * 2 + 1) as i32, c);
            }
        }
    }

    /// Draw an ASCII string starting at `(x, y)`.  Advances by 8 px per char.
    /// Newlines move the cursor down 16 px and back to the start column.
    pub fn draw_text(&mut self, x: i32, y: i32, s: &str, fg: Color, bg: Color) {
        let (mut cx, mut cy) = (x, y);
        for b in s.bytes() {
            match b {
                b'\n' => { cx = x; cy += font::CHAR_H as i32; }
                b'\r' => { cx = x; }
                _    => { self.draw_char(cx, cy, b, fg, bg); cx += font::CHAR_W as i32; }
            }
        }
    }

    /// Copy `src` (XRGB pixels) into this surface at `(dst_x, dst_y)`.
    /// Source and destination regions are clipped to their respective bounds.
    /// Used to composite per-window back-buffers onto the screen FB.
    pub fn blit(&mut self, dst_x: i32, dst_y: i32, src: &Surface) {
        // Intersect "src placed at dst_x/dst_y" with our own rect.
        let placed = Rect::new(dst_x, dst_y, src.width, src.height);
        let visible = placed.intersect(&self.rect());
        if visible.is_empty() { return; }
        // sx0 / sy0 = where in the source the visible region starts.
        let sx0 = visible.x - dst_x;
        let sy0 = visible.y - dst_y;
        for row in 0..visible.h {
            unsafe {
                let s_row = src.base.offset(((sy0 + row) * src.stride + sx0) as isize);
                let d_row = self.base.offset(((visible.y + row) * self.stride + visible.x) as isize);
                core::ptr::copy_nonoverlapping(s_row, d_row, visible.w as usize);
            }
        }
    }

    /// Borrow a rectangular sub-region as its own writable [`Surface`].
    ///
    /// The returned surface shares `stride` with `self`, so writing to it
    /// goes directly into the parent's pixel memory — no extra back-buffer.
    /// Useful for "give the widget the area it owns" without each widget
    /// having to know the parent's coordinate system.
    ///
    /// Returns `None` if `rect` doesn't fit fully inside `self`.
    pub fn subsurface(&mut self, rect: &Rect) -> Option<Surface> {
        let clipped = rect.intersect(&self.rect());
        if clipped != *rect || clipped.is_empty() {
            return None;
        }
        // SAFETY: clipped lies fully inside self, so the offset is valid;
        // we hand out a sub-view with the same lifetime semantics as self.
        unsafe {
            let base = self.base.offset((clipped.y * self.stride + clipped.x) as isize);
            Some(Surface { base, width: clipped.w, height: clipped.h, stride: self.stride })
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests — run on host with `cargo test -p beetos`.
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_surface(w: i32, h: i32) -> (alloc::vec::Vec<u32>, Surface) {
        let mut buf = alloc::vec![0u32; (w * h) as usize];
        let ptr = buf.as_mut_ptr();
        let s = unsafe { Surface::from_raw_parts(ptr, w, h, w) };
        (buf, s)
    }

    extern crate alloc;

    #[test]
    fn set_get_roundtrip() {
        let (buf, mut s) = make_surface(4, 4);
        s.set_pixel(1, 2, color::RED);
        assert_eq!(s.get_pixel(1, 2), color::RED);
        // make sure the right cell in the buffer was hit (row-major).
        assert_eq!(buf[2 * 4 + 1], color::RED);
    }

    #[test]
    fn bounds_clip() {
        let (_buf, mut s) = make_surface(2, 2);
        s.set_pixel(-1, 0, color::RED);
        s.set_pixel(0, 5,  color::RED);
        // nothing should have been written
        for y in 0..2 { for x in 0..2 {
            assert_eq!(s.get_pixel(x, y), 0);
        }}
    }

    #[test]
    fn fill_rect_clips() {
        let (_buf, mut s) = make_surface(4, 4);
        s.fill_rect(&Rect::new(-2, -2, 4, 4), color::BLUE);
        assert_eq!(s.get_pixel(0, 0), color::BLUE);
        assert_eq!(s.get_pixel(1, 1), color::BLUE);
        assert_eq!(s.get_pixel(2, 2), 0); // outside the clipped rect
    }

    #[test]
    fn line_endpoints() {
        let (_buf, mut s) = make_surface(8, 8);
        s.line(0, 0, 7, 7, color::WHITE);
        assert_eq!(s.get_pixel(0, 0), color::WHITE);
        assert_eq!(s.get_pixel(7, 7), color::WHITE);
        assert_eq!(s.get_pixel(3, 3), color::WHITE);
        // off the line
        assert_eq!(s.get_pixel(7, 0), 0);
    }

    #[test]
    fn rect_intersect() {
        let a = Rect::new(0, 0, 10, 10);
        let b = Rect::new(5, 5, 10, 10);
        let i = a.intersect(&b);
        assert_eq!(i, Rect::new(5, 5, 5, 5));

        let c = Rect::new(20, 20, 5, 5);
        assert!(a.intersect(&c).is_empty());
    }

    #[test]
    fn circle_symmetry() {
        let (_buf, mut s) = make_surface(11, 11);
        s.circle(5, 5, 4, color::GREEN);
        // 8 octant symmetry — pick two mirrored points.
        assert_eq!(s.get_pixel(9, 5), color::GREEN); // east
        assert_eq!(s.get_pixel(1, 5), color::GREEN); // west
        assert_eq!(s.get_pixel(5, 1), color::GREEN); // north
        assert_eq!(s.get_pixel(5, 9), color::GREEN); // south
    }

    #[test]
    fn blit_copies_pixels() {
        let (_src_buf, mut src) = make_surface(2, 2);
        src.set_pixel(0, 0, color::RED);
        src.set_pixel(1, 0, color::GREEN);
        src.set_pixel(0, 1, color::BLUE);
        src.set_pixel(1, 1, color::WHITE);

        let (_dst_buf, mut dst) = make_surface(4, 4);
        dst.blit(1, 1, &src);

        assert_eq!(dst.get_pixel(1, 1), color::RED);
        assert_eq!(dst.get_pixel(2, 1), color::GREEN);
        assert_eq!(dst.get_pixel(1, 2), color::BLUE);
        assert_eq!(dst.get_pixel(2, 2), color::WHITE);
        // outside the blit target stays zero
        assert_eq!(dst.get_pixel(0, 0), 0);
    }

    #[test]
    fn subsurface_writes_through_parent() {
        let (_buf, mut parent) = make_surface(4, 4);
        {
            let mut sub = parent.subsurface(&Rect::new(1, 1, 2, 2)).unwrap();
            sub.fill(color::YELLOW);
        }
        assert_eq!(parent.get_pixel(1, 1), color::YELLOW);
        assert_eq!(parent.get_pixel(2, 2), color::YELLOW);
        assert_eq!(parent.get_pixel(0, 0), 0);
    }

    #[test]
    fn draw_text_lands_on_pixels() {
        let (_buf, mut s) = make_surface(64, 16);
        s.draw_text(0, 0, "Hi", color::WHITE, color::BLACK);
        // The glyphs are 8×16; bytes at (0,0) should now contain something
        // non-zero — exact bit pattern depends on font8x8 but coverage > 0.
        let mut painted = 0;
        for y in 0..16 { for x in 0..16 {
            if s.get_pixel(x, y) != 0 { painted += 1; }
        }}
        assert!(painted > 5, "expected some 'Hi' pixels, painted={painted}");
    }
}
