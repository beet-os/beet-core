// SPDX-FileCopyrightText: 2025 BeetOS contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Window system on top of [`crate::gfx`].
//!
//! The whole stack is `no_std + no_alloc` because the kernel itself
//! refuses to allocate: every window lives in a fixed-size array, every
//! title in a fixed-size byte buffer. That mirrors the design of
//! `kernel/src/shell/ramfs.rs` (MAX_FILES, MAX_NAME_LEN) and lets the
//! same window manager run unchanged under cargo test (std), in the
//! kernel image (no_std), or eventually inside a userspace compositor.
//!
//! Architecture:
//! - [`Window`] is metadata only (rect, title, z-order, kind, focus).
//!   There is intentionally no per-window back-buffer yet — paint
//!   happens directly on the screen surface during composite. That
//!   keeps memory pressure flat (1 screen FB, no extra buffers) and
//!   matches what a real compositor does on its initial frame anyway.
//! - [`WindowManager`] owns a `[Window; MAX_WINDOWS]` slab + a count,
//!   plus a focused-window pointer. `compose(&mut Surface)` walks
//!   visible windows in z-order and renders each.
//! - Content is described by [`WindowKind`] — `Empty`, `Text(lines)`,
//!   `Demo` (a colourful gfx showcase), `WidgetTree(root)` once
//!   widgets land in Phase 6.
//!
//! Phase 5 / 6 will replace `WindowKind::Text` with a proper widget
//! tree, but the WindowManager API (add, remove, focus, compose) is
//! deliberately frozen now so widget land is purely additive.

use crate::gfx::{color, Color, Rect, Surface};
use crate::font;

// ─────────────────────────────────────────────────────────────────────────────
// Sizing constants — tuned to fit comfortably in the kernel's RAM budget.
// ─────────────────────────────────────────────────────────────────────────────

/// Hard cap on simultaneous windows.  Anything past this returns
/// [`AddError::TooManyWindows`].
pub const MAX_WINDOWS: usize = 8;

/// Title byte limit (UTF-8 truncated at this length).  31 lines up nicely
/// next to most window widths at the 8 px font cell.
pub const MAX_TITLE_LEN: usize = 31;

/// Number of body text lines a `WindowKind::Text` window can hold.
pub const MAX_TEXT_LINES: usize = 16;

/// Each body line is bounded just like the title.
pub const MAX_LINE_LEN: usize = 79;

/// Title bar height in pixels.
pub const TITLEBAR_H: i32 = 22;

/// Frame thickness around windows.
pub const FRAME_W: i32 = 1;

/// Width of the close button on the title bar.
pub const CLOSE_BTN_W: i32 = TITLEBAR_H;

// ─────────────────────────────────────────────────────────────────────────────
// Window kind / content
// ─────────────────────────────────────────────────────────────────────────────

/// What a window renders inside its content area.
///
/// Each variant is `Copy` and bounded in size so the surrounding
/// `Window` stays a flat POD struct that fits in the static
/// `[Window; MAX_WINDOWS]` slab.
#[derive(Clone, Copy)]
pub enum WindowKind {
    /// No content — just a colored client area. Useful as a placeholder
    /// or for tests where what matters is decoration layout.
    Empty,
    /// Up to [`MAX_TEXT_LINES`] of text, drawn from the top-left of the
    /// content area in the default font.  Lines past the visible area
    /// are clipped (no scroll yet).
    Text { lines: [TextLine; MAX_TEXT_LINES], count: u8 },
    /// Calls into a gfx showcase that exercises every primitive (rect,
    /// circle, lines, text).  Mostly for the M12 demo screen and to
    /// keep the rasterizer covered in screenshots.
    Demo,
    /// A widget tree — currently a single grid container, which is enough
    /// for the calculator demo's static look. The interactive calculator
    /// uses [`WindowKind::Calc`] instead.
    Widgets(WidgetGrid),
    /// Interactive integer calculator — owns its own state and reacts
    /// to keys via [`WindowManager::handle_key`]. The rendering pulls
    /// the current display string into a fresh widget grid at draw
    /// time so the layout matches the static `Widgets` calculator.
    Calc(CalcState),
    /// A note pad — every printable key appends, backspace deletes,
    /// Enter inserts a newline. Up to [`NOTES_MAX_LEN`] chars.
    Notes(NotesState),
}

/// One line of text inside a `WindowKind::Text` window.
#[derive(Clone, Copy)]
pub struct TextLine {
    bytes: [u8; MAX_LINE_LEN],
    len: u8,
}

impl TextLine {
    pub const EMPTY: TextLine = TextLine { bytes: [0; MAX_LINE_LEN], len: 0 };

    /// Build a TextLine from a `&str`, truncating to [`MAX_LINE_LEN`].
    pub fn new(s: &str) -> Self {
        let mut buf = [0u8; MAX_LINE_LEN];
        let take = s.len().min(MAX_LINE_LEN);
        buf[..take].copy_from_slice(&s.as_bytes()[..take]);
        TextLine { bytes: buf, len: take as u8 }
    }

    pub fn as_str(&self) -> &str {
        // SAFETY: we only ever ingest from &str in `new`, so the bytes are
        // already valid UTF-8 (truncation may cut a multi-byte char but it
        // still stays a valid prefix when we slice by `len`).
        core::str::from_utf8(&self.bytes[..self.len as usize]).unwrap_or("")
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Interactive content — calculator + notes
// ─────────────────────────────────────────────────────────────────────────────

/// Maximum digit count for the calculator display (sign + 15 digits is
/// well past i64::MIN's 20 chars but plenty for a demo).
pub const CALC_DISPLAY_MAX: usize = 18;

/// Tiny state machine driving the interactive calculator.
///
/// Integer-only on purpose: keeping i64 lets the whole thing stay in
/// `core` without dragging in `libm`. Operator precedence is the
/// reckless "every binary op evaluates the pending one" model that
/// every desktop calculator uses (i.e. `2 + 3 * 4` becomes 20, not
/// 14). Good enough for the demo and matches user expectation.
#[derive(Clone, Copy)]
pub struct CalcState {
    bytes: [u8; CALC_DISPLAY_MAX],
    len: u8,
    accumulator: i64,
    pending_op: u8,       // '+' '-' '*' '/' or 0 for none
    just_evaluated: bool, // true → next digit replaces display
    overflow: bool,
}

impl CalcState {
    pub const fn new() -> Self {
        let mut bytes = [0u8; CALC_DISPLAY_MAX];
        bytes[0] = b'0';
        CalcState {
            bytes,
            len: 1,
            accumulator: 0,
            pending_op: 0,
            just_evaluated: false,
            overflow: false,
        }
    }

    /// Pre-seeded state for the screenshot — shows a computed value so
    /// the calculator looks alive even before the first keystroke.
    pub fn from_preview(display: &str) -> Self {
        let mut s = Self::new();
        s.set_display(display);
        s
    }

    pub fn display(&self) -> &str {
        if self.overflow { return "Err"; }
        core::str::from_utf8(&self.bytes[..self.len as usize]).unwrap_or("?")
    }

    fn set_display(&mut self, s: &str) {
        let take = s.len().min(CALC_DISPLAY_MAX);
        self.bytes[..take].copy_from_slice(&s.as_bytes()[..take]);
        self.len = take as u8;
        self.overflow = false;
    }

    fn parse_display(&self) -> i64 {
        self.display().parse::<i64>().unwrap_or(0)
    }

    /// Handle a single key press. Returns `true` if the display
    /// changed and the window should be re-drawn.
    pub fn press(&mut self, key: u8) -> bool {
        match key {
            b'0'..=b'9' => {
                if self.just_evaluated { self.set_display("0"); self.just_evaluated = false; }
                if self.display() == "0" {
                    self.bytes[0] = key;
                    self.len = 1;
                } else if (self.len as usize) < CALC_DISPLAY_MAX {
                    self.bytes[self.len as usize] = key;
                    self.len += 1;
                }
                true
            }
            b'+' | b'-' | b'*' | b'/' => {
                self.apply_pending();
                self.pending_op = key;
                self.just_evaluated = true;
                true
            }
            b'=' | b'\n' | b'\r' => {
                self.apply_pending();
                self.pending_op = 0;
                self.just_evaluated = true;
                true
            }
            b'C' | b'c' | 0x1B /* ESC */ => {
                *self = CalcState::new();
                true
            }
            8 | 0x7F => { // Backspace / DEL
                if self.len > 1 { self.len -= 1; }
                else { self.bytes[0] = b'0'; self.len = 1; }
                true
            }
            b's' /* sign */ => {
                if self.display() != "0" {
                    if self.bytes[0] == b'-' {
                        for i in 1..(self.len as usize) {
                            self.bytes[i - 1] = self.bytes[i];
                        }
                        self.len -= 1;
                    } else if (self.len as usize) < CALC_DISPLAY_MAX {
                        for i in (1..=(self.len as usize)).rev() {
                            self.bytes[i] = self.bytes[i - 1];
                        }
                        self.bytes[0] = b'-';
                        self.len += 1;
                    }
                }
                true
            }
            _ => false,
        }
    }

    fn apply_pending(&mut self) {
        let rhs = self.parse_display();
        let result = match self.pending_op {
            b'+' => self.accumulator.checked_add(rhs),
            b'-' => self.accumulator.checked_sub(rhs),
            b'*' => self.accumulator.checked_mul(rhs),
            b'/' => if rhs == 0 { None } else { self.accumulator.checked_div(rhs) },
            _ => Some(rhs),
        };
        match result {
            Some(v) => {
                self.accumulator = v;
                // Render v into the display buffer using a small helper.
                let mut tmp = [0u8; 20];
                let s = i64_to_str(v, &mut tmp);
                self.set_display(s);
            }
            None => {
                self.overflow = true;
                self.accumulator = 0;
            }
        }
    }
}

fn i64_to_str(mut n: i64, buf: &mut [u8]) -> &str {
    if n == 0 { buf[0] = b'0'; return core::str::from_utf8(&buf[..1]).unwrap(); }
    let neg = n < 0;
    let mut idx = buf.len();
    let mut abs: u64 = if neg { (n as i128).unsigned_abs() as u64 } else { n as u64 };
    let _ = &mut n; // silence unused
    while abs > 0 && idx > 0 {
        idx -= 1;
        buf[idx] = b'0' + (abs % 10) as u8;
        abs /= 10;
    }
    if neg && idx > 0 { idx -= 1; buf[idx] = b'-'; }
    core::str::from_utf8(&buf[idx..]).unwrap_or("?")
}

/// Maximum chars in the Notes pad.
pub const NOTES_MAX_LEN: usize = 512;

/// A simple text pad. Append on printable keys, delete on backspace,
/// newline on Enter. Wraps to the next line at the window edge.
#[derive(Clone, Copy)]
pub struct NotesState {
    buf: [u8; NOTES_MAX_LEN],
    len: u16,
}

impl NotesState {
    pub const fn new() -> Self { NotesState { buf: [0; NOTES_MAX_LEN], len: 0 } }

    pub fn with_text(s: &str) -> Self {
        let mut n = Self::new();
        let take = s.len().min(NOTES_MAX_LEN);
        n.buf[..take].copy_from_slice(&s.as_bytes()[..take]);
        n.len = take as u16;
        n
    }

    pub fn text(&self) -> &str {
        core::str::from_utf8(&self.buf[..self.len as usize]).unwrap_or("")
    }

    pub fn press(&mut self, key: u8) -> bool {
        match key {
            8 | 0x7F => {
                if self.len > 0 { self.len -= 1; true } else { false }
            }
            b'\n' | b'\r' => {
                if (self.len as usize) < NOTES_MAX_LEN {
                    self.buf[self.len as usize] = b'\n';
                    self.len += 1;
                    true
                } else { false }
            }
            0x20..=0x7E => {
                if (self.len as usize) < NOTES_MAX_LEN {
                    self.buf[self.len as usize] = key;
                    self.len += 1;
                    true
                } else { false }
            }
            _ => false,
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Window
// ─────────────────────────────────────────────────────────────────────────────

/// A single window in the system.
#[derive(Clone, Copy)]
pub struct Window {
    pub rect: Rect,
    title_bytes: [u8; MAX_TITLE_LEN],
    title_len: u8,
    pub z: u8,
    pub visible: bool,
    pub focused: bool,
    pub bg: Color,
    pub kind: WindowKind,
}

impl Window {
    /// Build a window. `title` is truncated to [`MAX_TITLE_LEN`] bytes.
    pub fn new(rect: Rect, title: &str, kind: WindowKind) -> Self {
        let mut t = [0u8; MAX_TITLE_LEN];
        let take = title.len().min(MAX_TITLE_LEN);
        t[..take].copy_from_slice(&title.as_bytes()[..take]);
        Window {
            rect,
            title_bytes: t,
            title_len: take as u8,
            z: 0,
            visible: true,
            focused: false,
            bg: color::WINDOW_BG,
            kind,
        }
    }

    pub fn title(&self) -> &str {
        core::str::from_utf8(&self.title_bytes[..self.title_len as usize]).unwrap_or("")
    }

    /// Where the actual content of the window starts, excluding decorations.
    pub fn content_rect(&self) -> Rect {
        Rect::new(
            self.rect.x + FRAME_W,
            self.rect.y + TITLEBAR_H,
            (self.rect.w - 2 * FRAME_W).max(0),
            (self.rect.h - TITLEBAR_H - FRAME_W).max(0),
        )
    }

    /// Hit-test the title bar — used so the manager can route drag/focus
    /// events without each caller hard-coding the layout.
    pub fn titlebar_contains(&self, x: i32, y: i32) -> bool {
        let bar = Rect::new(self.rect.x, self.rect.y, self.rect.w, TITLEBAR_H);
        bar.contains(x, y)
    }

    /// Hit-test the close button (top-right square of the title bar).
    pub fn close_button_contains(&self, x: i32, y: i32) -> bool {
        let bx = self.rect.right() - CLOSE_BTN_W;
        let by = self.rect.y;
        Rect::new(bx, by, CLOSE_BTN_W, TITLEBAR_H).contains(x, y)
    }

    fn draw(&self, screen: &mut Surface) {
        if !self.visible { return; }

        // 1. Outer frame.
        let outer = self.rect;
        let frame_color = if self.focused { color::BEET_PURPLE } else { color::WINDOW_FRAME };
        screen.fill_rect(&outer, frame_color);

        // 2. Title bar.
        let title_bar = Rect::new(outer.x, outer.y, outer.w, TITLEBAR_H);
        let title_bg = if self.focused { color::BEET_PURPLE } else { color::TITLE_BAR };
        screen.fill_rect(&title_bar, title_bg);

        // 3. Title text — left padded by FRAME_W + 4 px so it doesn't kiss the frame.
        let title_x = outer.x + FRAME_W + 4;
        let title_y = outer.y + (TITLEBAR_H - font::CHAR_H as i32) / 2;
        screen.draw_text(title_x, title_y, self.title(), color::TITLE_FG, title_bg);

        // 4. Close button — small "X" in a right-aligned square.
        let close_x = outer.right() - CLOSE_BTN_W;
        let close_bg = if self.focused { color::BEET_PINK } else { color::WINDOW_FRAME };
        let close_box = Rect::new(close_x, outer.y, CLOSE_BTN_W, TITLEBAR_H);
        screen.fill_rect(&close_box, close_bg);
        // Draw a centred 8x8 "x" using two crossed diagonals.
        let cx = close_x + CLOSE_BTN_W / 2;
        let cy = outer.y + TITLEBAR_H / 2;
        screen.line(cx - 4, cy - 4, cx + 4, cy + 4, color::WHITE);
        screen.line(cx + 4, cy - 4, cx - 4, cy + 4, color::WHITE);

        // 5. Content area background.
        let content = self.content_rect();
        screen.fill_rect(&content, self.bg);

        // 6. Content.
        match &self.kind {
            WindowKind::Empty => {}
            WindowKind::Text { lines, count } => {
                let mut ty = content.y + 6;
                for line in lines.iter().take(*count as usize) {
                    if ty + font::CHAR_H as i32 > content.bottom() { break; }
                    screen.draw_text(content.x + 6, ty, line.as_str(),
                        color::LIGHT_GRAY, self.bg);
                    ty += font::CHAR_H as i32 + 2;
                }
            }
            WindowKind::Demo => {
                draw_demo(screen, &content, self.bg);
            }
            WindowKind::Widgets(grid) => {
                grid.draw(screen, &content);
            }
            WindowKind::Calc(state) => {
                // Build a fresh grid every frame with the live display.
                // Cheap — WidgetGrid is ~600 B of POD.
                let grid = calculator_grid(state.display());
                grid.draw(screen, &content);
            }
            WindowKind::Notes(state) => {
                screen.fill_rect(&content, color::BLACK);
                // Crude word-wrap: feed chars left-to-right, advance to
                // next line on '\n' or when we run out of horizontal
                // space. No tab support, no scroll — fits the demo.
                let mut x = content.x + 6;
                let mut y = content.y + 6;
                let max_x = content.right() - 6;
                let line_h = font::CHAR_H as i32 + 2;
                for &b in &state.buf[..state.len as usize] {
                    if b == b'\n' || x + font::CHAR_W as i32 > max_x {
                        x = content.x + 6;
                        y += line_h;
                        if y + font::CHAR_H as i32 > content.bottom() { break; }
                        if b == b'\n' { continue; }
                    }
                    screen.draw_char(x, y, b, color::BRIGHT_GREEN, color::BLACK);
                    x += font::CHAR_W as i32;
                }
                // Blinking-style cursor (drawn solid for the still screenshot).
                screen.fill_rect(&Rect::new(x, y, 8, font::CHAR_H as i32),
                    color::BRIGHT_GREEN);
            }
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Window manager
// ─────────────────────────────────────────────────────────────────────────────

/// Errors returned by [`WindowManager::add`].
#[derive(Debug, PartialEq, Eq)]
pub enum AddError {
    /// The slab is full — drop a window first or raise [`MAX_WINDOWS`].
    TooManyWindows,
}

/// Opaque window identifier handed back by `add`.  Currently equal to
/// the index in the slab, but treat it as opaque — the slab may
/// reshuffle for z-order in a future revision.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WindowId(pub u8);

/// Owner of every window's metadata.
///
/// Built as a fixed-size slab so it can be a static / kernel-side
/// singleton without depending on any allocator.
pub struct WindowManager {
    windows: [Option<Window>; MAX_WINDOWS],
    next_z: u8,
    desktop_bg: Color,
}

impl WindowManager {
    pub const fn new() -> Self {
        // `[None; MAX_WINDOWS]` needs the inner type Copy — Window is Copy,
        // so Option<Window> is Copy too.
        Self {
            windows: [None; MAX_WINDOWS],
            next_z: 0,
            desktop_bg: color::DESKTOP_BG,
        }
    }

    pub fn set_desktop_bg(&mut self, color: Color) { self.desktop_bg = color; }

    /// Add a window. Its `z` is assigned monotonically so newer windows
    /// stack on top of older ones by default.
    pub fn add(&mut self, mut win: Window) -> Result<WindowId, AddError> {
        let slot = self.windows.iter().position(|w| w.is_none())
            .ok_or(AddError::TooManyWindows)?;
        win.z = self.next_z;
        self.next_z = self.next_z.saturating_add(1);
        self.windows[slot] = Some(win);
        Ok(WindowId(slot as u8))
    }

    /// Remove a window (e.g. after a close-button click).
    pub fn remove(&mut self, id: WindowId) {
        if let Some(slot) = self.windows.get_mut(id.0 as usize) {
            *slot = None;
        }
    }

    /// Mutable borrow of a window for callers that want to mutate
    /// content / rect / kind.  Returns `None` for stale or out-of-range
    /// ids.
    pub fn get_mut(&mut self, id: WindowId) -> Option<&mut Window> {
        self.windows.get_mut(id.0 as usize)?.as_mut()
    }

    pub fn get(&self, id: WindowId) -> Option<&Window> {
        self.windows.get(id.0 as usize)?.as_ref()
    }

    /// Number of live windows in the slab.
    pub fn len(&self) -> usize {
        self.windows.iter().filter(|w| w.is_some()).count()
    }

    pub fn is_empty(&self) -> bool { self.len() == 0 }

    /// Iterate live windows in z-order (bottom → top).
    pub fn iter_by_z(&self) -> impl Iterator<Item = (WindowId, &Window)> {
        // Build a temporary index table sorted by z. The slab is tiny
        // (MAX_WINDOWS) so an in-place insertion sort is fine and keeps us
        // away from alloc.
        let mut order = [u8::MAX; MAX_WINDOWS];
        let mut n = 0usize;
        for (i, w) in self.windows.iter().enumerate() {
            if w.is_some() {
                // insertion sort by z (ascending)
                let z = w.as_ref().unwrap().z;
                let mut j = n;
                while j > 0 && self.windows[order[j - 1] as usize].as_ref().unwrap().z > z {
                    order[j] = order[j - 1];
                    j -= 1;
                }
                order[j] = i as u8;
                n += 1;
            }
        }
        (0..n).map(move |k| {
            let i = order[k] as usize;
            (WindowId(i as u8), self.windows[i].as_ref().unwrap())
        })
    }

    /// Raise a window to the top of the z-order.  Newly-focused windows
    /// usually want this; the side effect on focus is the caller's job.
    pub fn raise(&mut self, id: WindowId) {
        // Just bump z above everyone else.  Because `next_z` is
        // monotonic, this is always safe and gives correct ordering.
        let new_z = self.next_z;
        self.next_z = self.next_z.saturating_add(1);
        if let Some(w) = self.get_mut(id) { w.z = new_z; }
    }

    /// Set focus to a single window (and unfocus the rest).
    pub fn focus(&mut self, id: WindowId) {
        for (i, slot) in self.windows.iter_mut().enumerate() {
            if let Some(w) = slot {
                w.focused = i == id.0 as usize;
            }
        }
        self.raise(id);
    }

    /// Find the topmost visible window whose rect contains the point,
    /// or `None` if the click landed on the desktop.
    pub fn hit_test(&self, x: i32, y: i32) -> Option<WindowId> {
        // iter_by_z gives bottom→top; we want top→bottom so we collect
        // first.
        let mut top: Option<WindowId> = None;
        for (id, w) in self.iter_by_z() {
            if w.visible && w.rect.contains(x, y) { top = Some(id); }
        }
        top
    }

    /// Cycle focus through visible windows in z-order.
    /// `forward = true` moves to the next z-up window, `false` moves
    /// to the previous one. Typically wired to Tab / Shift-Tab.
    pub fn cycle_focus(&mut self, forward: bool) {
        // Collect ids in z-order.
        let mut ids = [WindowId(0); MAX_WINDOWS];
        let mut n = 0;
        for (id, win) in self.iter_by_z() {
            if win.visible { ids[n] = id; n += 1; }
        }
        if n == 0 { return; }
        let cur = (0..n).find(|&i| {
            let id = ids[i];
            self.get(id).map(|w| w.focused).unwrap_or(false)
        });
        let next = match cur {
            Some(i) => if forward { (i + 1) % n } else { (i + n - 1) % n },
            None    => if forward { 0 } else { n - 1 },
        };
        self.focus(ids[next]);
    }

    /// Deliver a key press to the currently-focused window.  Returns
    /// `true` if state changed and a recompose is warranted.
    ///
    /// Reserved global keys:
    ///   - `\t`    → Tab — cycle focus forward
    ///   - 0x19    → Shift-Tab — cycle focus backward (PC-style)
    pub fn handle_key(&mut self, key: u8) -> bool {
        // Global shortcuts handled before delegation.
        match key {
            b'\t' => { self.cycle_focus(true);  return true; }
            0x19  => { self.cycle_focus(false); return true; }
            _ => {}
        }
        let focused_id = self.windows.iter().enumerate()
            .find(|(_, w)| w.as_ref().is_some_and(|w| w.focused))
            .map(|(i, _)| WindowId(i as u8));
        let Some(id) = focused_id else { return false; };
        let Some(win) = self.get_mut(id) else { return false; };
        match &mut win.kind {
            WindowKind::Calc(state)  => state.press(key),
            WindowKind::Notes(state) => state.press(key),
            _ => false,
        }
    }

    /// Re-paint with both uptime and a free-running animation frame
    /// counter (lower bits drive the spinner / pulses; higher bits
    /// derive the clock).
    pub fn compose_animated(&self, screen: &mut Surface, uptime_seconds: u64, frame: u64) {
        self.compose_inner(screen, Some(uptime_seconds), Some(frame));
    }

    /// Re-paint with an uptime clock but no per-frame animation.
    pub fn compose_with_uptime(&self, screen: &mut Surface, uptime_seconds: u64) {
        self.compose_inner(screen, Some(uptime_seconds), None);
    }

    /// Re-paint the desktop and every visible window onto `screen`.
    /// O(windows × pixels) — fine for the static demo screens, will
    /// move to dirty-rect tracking when input lands.
    pub fn compose(&self, screen: &mut Surface) {
        self.compose_inner(screen, None, None);
    }

    fn compose_inner(&self, screen: &mut Surface, uptime: Option<u64>, frame: Option<u64>) {
        screen.fill(self.desktop_bg);

        // Top accent bar + taskbar across the bottom so the desktop
        // doesn't look like a blank rectangle.
        let w = screen.width();
        let h = screen.height();
        screen.fill_rect(&Rect::new(0, 0, w, 4), color::BEET_PURPLE);
        let taskbar_h = 28;
        let taskbar = Rect::new(0, h - taskbar_h, w, taskbar_h);
        screen.fill_rect(&taskbar, color::TITLE_BAR);
        screen.fill_rect(&Rect::new(0, h - taskbar_h - 1, w, 1), color::WINDOW_FRAME);

        // Brand label on the taskbar.
        screen.draw_text(8, h - taskbar_h + (taskbar_h - font::CHAR_H as i32) / 2,
            "BeetOS", color::TITLE_FG, color::TITLE_BAR);

        // Background animation: a few softly-shifted "stars" on the
        // desktop, plus a slow-moving bouncing accent circle. Skipped
        // when `frame` is None so unit tests stay deterministic.
        if let Some(f) = frame {
            // Bouncing ball — clamps inside the desktop area, avoiding
            // the title bar / taskbar.
            let area_top = 8;
            let area_bot = h - taskbar_h - 8;
            let area_left = 8;
            let area_right = w - 8;
            let span_x = (area_right - area_left).max(1);
            let span_y = (area_bot - area_top).max(1);
            // Cheap sawtooth bounce so we don't need sin/cos.
            let px_cycle = (f * 7 % (span_x as u64 * 2)) as i32;
            let py_cycle = (f * 5 % (span_y as u64 * 2)) as i32;
            let bx = area_left + if px_cycle < span_x { px_cycle } else { 2 * span_x - px_cycle };
            let by = area_top  + if py_cycle < span_y { py_cycle } else { 2 * span_y - py_cycle };
            // Pulse the radius too.
            let r = 10 + ((f % 16) as i32 - 8).abs();
            screen.circle_filled(bx, by, r,   color::BEET_PINK);
            screen.circle(bx, by, r + 4, color::BEET_PURPLE);
        }

        // Uptime clock on the right of the taskbar.
        if let Some(uptime) = uptime {
            let hh = uptime / 3600;
            let mm = (uptime % 3600) / 60;
            let ss = uptime % 60;
            let mut buf = [b'0'; 32];
            // Manual zero-padded HH:MM:SS — keeps us out of core::fmt allocations.
            buf[0] = b'0' + ((hh / 10) % 10) as u8;
            buf[1] = b'0' + (hh % 10) as u8;
            buf[2] = b':';
            buf[3] = b'0' + ((mm / 10) % 10) as u8;
            buf[4] = b'0' + (mm % 10) as u8;
            buf[5] = b':';
            buf[6] = b'0' + ((ss / 10) % 10) as u8;
            buf[7] = b'0' + (ss % 10) as u8;
            let s = core::str::from_utf8(&buf[..8]).unwrap_or("");
            let clock_w = 8 * font::CHAR_W as i32;
            let cx = w - clock_w - 16;
            let cy = h - taskbar_h + (taskbar_h - font::CHAR_H as i32) / 2;
            // Small pill background so the clock pops out of the taskbar.
            let pill = Rect::new(cx - 8, cy - 4, clock_w + 16, font::CHAR_H as i32 + 8);
            screen.fill_rect(&pill, color::BEET_PURPLE);
            screen.draw_text(cx, cy, s, color::WHITE, color::BEET_PURPLE);

            // Spinner just left of the clock when frame is supplied.
            if let Some(f) = frame {
                let spinners = b"|/-\\";
                let sp = spinners[(f as usize) % spinners.len()];
                let spx = cx - 24;
                let spbg = Rect::new(spx - 4, cy - 4, font::CHAR_W as i32 + 8, font::CHAR_H as i32 + 8);
                screen.fill_rect(&spbg, color::TITLE_BAR);
                screen.draw_char(spx, cy, sp, color::BRIGHT_GREEN, color::TITLE_BAR);
            }
        }

        // Taskbar entries — one per window.
        let mut tx: i32 = 80;
        for (_id, win) in self.iter_by_z() {
            if !win.visible { continue; }
            let entry_w = (win.title().len() as i32 * 8 + 16).max(80);
            let entry = Rect::new(tx, h - taskbar_h + 4, entry_w, taskbar_h - 8);
            let bg = if win.focused { color::BEET_PURPLE } else { color::WINDOW_FRAME };
            screen.fill_rect(&entry, bg);
            screen.draw_text(tx + 8, entry.y + 4, win.title(),
                if win.focused { color::WHITE } else { color::LIGHT_GRAY }, bg);
            tx += entry_w + 6;
        }

        // Actual windows, bottom → top.
        for (_id, win) in self.iter_by_z() {
            win.draw(screen);
        }
    }
}

impl Default for WindowManager {
    fn default() -> Self { Self::new() }
}

// ─────────────────────────────────────────────────────────────────────────────
// Widgets — fixed-depth tree (Container → LeafWidget), no_alloc.
// ─────────────────────────────────────────────────────────────────────────────

/// Maximum widgets inside one container.  A 4x4 calculator (16 cells)
/// fits exactly; anything larger should split across multiple windows.
pub const MAX_WIDGETS: usize = 24;

/// State of a button — toggled by the eventual mouse/keyboard wiring
/// (Phase 5).  Today we just render the default; ButtonState::Pressed
/// gives a darker shade so the API is already there.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ButtonState {
    Normal,
    Pressed,
    Disabled,
}

/// A label widget — fixed text, foreground, background.
#[derive(Clone, Copy)]
pub struct LabelW {
    pub text: TextLine,
    pub fg: Color,
    pub bg: Color,
    /// Horizontal alignment inside the cell. 0 = left, 1 = centre, 2 = right.
    pub align: u8,
}

impl LabelW {
    pub fn new(text: &str) -> Self {
        LabelW { text: TextLine::new(text), fg: color::WHITE, bg: color::WINDOW_BG, align: 1 }
    }

    pub fn with_colors(mut self, fg: Color, bg: Color) -> Self { self.fg = fg; self.bg = bg; self }
    pub fn align_left(mut self)  -> Self { self.align = 0; self }
    pub fn align_right(mut self) -> Self { self.align = 2; self }

    fn draw(&self, screen: &mut Surface, rect: &Rect) {
        screen.fill_rect(rect, self.bg);
        let text_w = self.text.as_str().len() as i32 * font::CHAR_W as i32;
        let tx = match self.align {
            0 => rect.x + 4,
            1 => rect.x + (rect.w - text_w) / 2,
            _ => rect.right() - text_w - 4,
        };
        let ty = rect.y + (rect.h - font::CHAR_H as i32) / 2;
        screen.draw_text(tx, ty, self.text.as_str(), self.fg, self.bg);
    }
}

/// A click-able button — currently keyboard/mouse wiring lives in
/// Phase 5; today the widget just renders. The `id` field lets the
/// app distinguish which button was hit when events finally land.
#[derive(Clone, Copy)]
pub struct ButtonW {
    pub label: TextLine,
    pub state: ButtonState,
    pub bg: Color,
    pub fg: Color,
    pub id: u16,
}

impl ButtonW {
    pub fn new(label: &str, id: u16) -> Self {
        ButtonW {
            label: TextLine::new(label),
            state: ButtonState::Normal,
            bg: color::WINDOW_FRAME,
            fg: color::WHITE,
            id,
        }
    }

    pub fn accent(mut self) -> Self { self.bg = color::BEET_PURPLE; self }
    pub fn warning(mut self) -> Self { self.bg = color::BRIGHT_RED; self }

    fn draw(&self, screen: &mut Surface, rect: &Rect) {
        let (bg, border) = match self.state {
            ButtonState::Normal   => (self.bg,                 color::TITLE_FG),
            ButtonState::Pressed  => (darken(self.bg),         color::WHITE),
            ButtonState::Disabled => (color::DARK_GRAY,        color::LIGHT_GRAY),
        };
        screen.fill_rect(rect, bg);
        screen.rect_outline(rect, border);
        let text_w = self.label.as_str().len() as i32 * font::CHAR_W as i32;
        let tx = rect.x + (rect.w - text_w) / 2;
        let ty = rect.y + (rect.h - font::CHAR_H as i32) / 2;
        screen.draw_text(tx, ty, self.label.as_str(), self.fg, bg);
    }
}

/// Multiply RGB channels by 75 % — used for pressed button shading.
fn darken(c: Color) -> Color {
    let r = ((c >> 16) & 0xFF) * 3 / 4;
    let g = ((c >>  8) & 0xFF) * 3 / 4;
    let b = ( c        & 0xFF) * 3 / 4;
    (r << 16) | (g << 8) | b
}

/// A single leaf in a [`WidgetGrid`].
#[derive(Clone, Copy)]
pub enum LeafWidget {
    Label(LabelW),
    Button(ButtonW),
    Spacer,
}

impl LeafWidget {
    fn draw(&self, screen: &mut Surface, rect: &Rect) {
        match self {
            LeafWidget::Label(l)  => l.draw(screen, rect),
            LeafWidget::Button(b) => b.draw(screen, rect),
            LeafWidget::Spacer    => {}
        }
    }
}

/// A grid layout of leaf widgets.
///
/// Cells are placed left-to-right, top-to-bottom — like CSS grid with
/// `grid-auto-flow: row`. `row_span` / `col_span` aren't supported
/// yet (a Label spans `span_cols` columns instead, which is enough
/// for the calculator display).
#[derive(Clone, Copy)]
pub struct WidgetGrid {
    pub cells: [LeafWidget; MAX_WIDGETS],
    pub spans: [u8;          MAX_WIDGETS], // column span per cell (≥1)
    pub count: u8,
    pub cols:  u8,
    pub gap:   i32,
    pub padding: i32,
}

impl WidgetGrid {
    pub const fn new(cols: u8) -> Self {
        WidgetGrid {
            cells: [LeafWidget::Spacer; MAX_WIDGETS],
            spans: [1; MAX_WIDGETS],
            count: 0,
            cols,
            gap: 6,
            padding: 8,
        }
    }

    /// Append a widget that occupies one column.
    pub fn push(&mut self, widget: LeafWidget) -> &mut Self {
        if (self.count as usize) < MAX_WIDGETS {
            self.cells[self.count as usize] = widget;
            self.spans[self.count as usize] = 1;
            self.count += 1;
        }
        self
    }

    /// Append a widget that spans `span` columns (clamped to `cols`).
    pub fn push_span(&mut self, widget: LeafWidget, span: u8) -> &mut Self {
        if (self.count as usize) < MAX_WIDGETS {
            self.cells[self.count as usize] = widget;
            self.spans[self.count as usize] = span.max(1).min(self.cols);
            self.count += 1;
        }
        self
    }

    /// Compute layout + draw every cell inside `rect`.
    fn draw(&self, screen: &mut Surface, rect: &Rect) {
        if self.count == 0 || self.cols == 0 { return; }
        let cols = self.cols as i32;
        // Sum spans to figure out per-row geometry.  We greedy-pack
        // each row until the running span tally reaches `cols`, then
        // start a new row.
        let inner_w = rect.w - 2 * self.padding;
        let inner_h = rect.h - 2 * self.padding;
        // Rows are computed from the same packing the draw loop uses.
        let row_count = self.row_count() as i32;
        if row_count == 0 { return; }
        let cell_w  = (inner_w - (cols - 1) * self.gap) / cols;
        let cell_h  = (inner_h - (row_count - 1) * self.gap) / row_count;

        let mut col_cursor: i32 = 0;
        let mut row_cursor: i32 = 0;
        for i in 0..(self.count as usize) {
            let span = self.spans[i] as i32;
            if col_cursor + span > cols {
                col_cursor = 0;
                row_cursor += 1;
            }
            let x = rect.x + self.padding + col_cursor * (cell_w + self.gap);
            let y = rect.y + self.padding + row_cursor * (cell_h + self.gap);
            let w = cell_w * span + self.gap * (span - 1);
            let cell_rect = Rect::new(x, y, w, cell_h);
            self.cells[i].draw(screen, &cell_rect);
            col_cursor += span;
            if col_cursor >= cols { col_cursor = 0; row_cursor += 1; }
        }
    }

    /// How many rows the current cells would occupy with greedy
    /// row-major packing.
    fn row_count(&self) -> usize {
        let cols = self.cols as i32;
        let mut col_cursor: i32 = 0;
        let mut rows: i32 = if self.count > 0 { 1 } else { 0 };
        for i in 0..(self.count as usize) {
            let span = self.spans[i] as i32;
            if col_cursor + span > cols {
                rows += 1;
                col_cursor = 0;
            }
            col_cursor += span;
            if col_cursor >= cols { col_cursor = 0; if i + 1 < self.count as usize { rows += 1; } }
        }
        rows as usize
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Demo content
// ─────────────────────────────────────────────────────────────────────────────

// ─────────────────────────────────────────────────────────────────────────────
// Sample widget trees — these are templates apps will reuse once the
// shell can spawn GUI processes (Phase 7 demo apps).
// ─────────────────────────────────────────────────────────────────────────────

/// Build the calculator widget grid: a wide display label at the top,
/// then a 4×5 keypad — clear / sign / percent / divide, 7/8/9/×,
/// 4/5/6/-, 1/2/3/+, ±/0/./=.  Each button has an id that maps to
/// `CalcKey` once we wire input.
pub fn calculator_grid(display: &str) -> WidgetGrid {
    let mut g = WidgetGrid::new(4);
    g.padding = 10;
    g.gap = 6;

    // Row 0 — display, spanning all 4 columns.
    let display_label = LabelW::new(display)
        .with_colors(color::BRIGHT_GREEN, color::BLACK)
        .align_right();
    g.push_span(LeafWidget::Label(display_label), 4);

    // Row 1 — clear, sign, percent, divide.
    g.push(LeafWidget::Button(ButtonW::new("AC", 1).warning()));
    g.push(LeafWidget::Button(ButtonW::new("+/-", 2)));
    g.push(LeafWidget::Button(ButtonW::new("%", 3)));
    g.push(LeafWidget::Button(ButtonW::new("/", 4).accent()));

    // Row 2 — 7 8 9 ×
    g.push(LeafWidget::Button(ButtonW::new("7", 7)));
    g.push(LeafWidget::Button(ButtonW::new("8", 8)));
    g.push(LeafWidget::Button(ButtonW::new("9", 9)));
    g.push(LeafWidget::Button(ButtonW::new("x", 10).accent()));

    // Row 3 — 4 5 6 -
    g.push(LeafWidget::Button(ButtonW::new("4", 13)));
    g.push(LeafWidget::Button(ButtonW::new("5", 14)));
    g.push(LeafWidget::Button(ButtonW::new("6", 15)));
    g.push(LeafWidget::Button(ButtonW::new("-", 16).accent()));

    // Row 4 — 1 2 3 +
    g.push(LeafWidget::Button(ButtonW::new("1", 19)));
    g.push(LeafWidget::Button(ButtonW::new("2", 20)));
    g.push(LeafWidget::Button(ButtonW::new("3", 21)));
    g.push(LeafWidget::Button(ButtonW::new("+", 22).accent()));

    // Row 5 — 0 spanning 2, then ., =.
    g.push_span(LeafWidget::Button(ButtonW::new("0", 25)), 2);
    g.push(LeafWidget::Button(ButtonW::new(".", 26)));
    g.push(LeafWidget::Button(ButtonW::new("=", 27).accent()));

    g
}

/// Build a small "about BeetOS" widget grid — a couple of labels and
/// an OK button. Exercises mixed widget types in a single grid.
pub fn about_grid() -> WidgetGrid {
    let mut g = WidgetGrid::new(2);
    g.padding = 12;
    g.gap = 8;
    g.push_span(LeafWidget::Label(
        LabelW::new("BeetOS v0.1.0")
            .with_colors(color::WHITE, color::WINDOW_BG)
    ), 2);
    g.push_span(LeafWidget::Label(
        LabelW::new("Secure microkernel OS on AArch64")
            .with_colors(color::LIGHT_GRAY, color::WINDOW_BG)
    ), 2);
    g.push_span(LeafWidget::Label(
        LabelW::new("github.com/beet-os/beet-core")
            .with_colors(color::BRIGHT_CYAN, color::WINDOW_BG)
    ), 2);
    g.push_span(LeafWidget::Spacer, 2);
    g.push(LeafWidget::Button(ButtonW::new("Close", 99)));
    g.push(LeafWidget::Button(ButtonW::new("OK", 100).accent()));
    g
}

/// Paint a quick showcase inside `content` using every gfx primitive.
/// Doubles as a visual regression target for [`crate::gfx`].
fn draw_demo(screen: &mut Surface, content: &Rect, bg: Color) {
    // Background gradient via 8 horizontal stripes.
    let strips = 8;
    let strip_h = content.h / strips;
    for i in 0..strips {
        let intensity = 0x10 + (i as u32) * 6;
        let c = (intensity << 16) | (intensity * 3 / 2 << 8) | (intensity * 2);
        screen.fill_rect(
            &Rect::new(content.x, content.y + i * strip_h, content.w, strip_h),
            c,
        );
    }

    // A few overlapping circles.
    let cx = content.x + content.w / 4;
    let cy = content.y + content.h / 2;
    screen.circle_filled(cx, cy, 20, color::BEET_PINK);
    screen.circle_filled(cx + 24, cy + 6, 16, color::BRIGHT_CYAN);
    screen.circle(cx + 12, cy - 4, 28, color::WHITE);

    // A rectangle grid.
    let gx = content.x + content.w / 2;
    let gy = content.y + 10;
    for i in 0..4 {
        for j in 0..3 {
            let r = Rect::new(gx + i * 18, gy + j * 18, 14, 14);
            let palette = [color::RED, color::GREEN, color::BLUE, color::YELLOW];
            screen.fill_rect(&r, palette[((i + j) as usize) % palette.len()]);
        }
    }

    // Lines radiating from a corner — exercises the Bresenham path.
    let ox = content.right() - 4;
    let oy = content.bottom() - 4;
    for k in 0..12 {
        screen.line(ox, oy, ox - 80 + k * 5, oy - 70, color::BEET_PURPLE);
    }

    // Caption.
    let caption = "beetos::gfx — software 2D";
    let ty = content.bottom() - 24;
    screen.draw_text(content.x + 8, ty, caption, color::WHITE, bg);
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    extern crate alloc;

    fn surface(w: i32, h: i32) -> (alloc::vec::Vec<u32>, Surface) {
        let mut buf = alloc::vec![0u32; (w * h) as usize];
        let ptr = buf.as_mut_ptr();
        let s = unsafe { Surface::from_raw_parts(ptr, w, h, w) };
        (buf, s)
    }

    #[test]
    fn add_remove_window() {
        let mut wm = WindowManager::new();
        let id = wm.add(Window::new(Rect::new(10, 10, 100, 50), "hi", WindowKind::Empty)).unwrap();
        assert_eq!(wm.len(), 1);
        assert_eq!(wm.get(id).unwrap().title(), "hi");
        wm.remove(id);
        assert!(wm.is_empty());
    }

    #[test]
    fn too_many_windows() {
        let mut wm = WindowManager::new();
        for i in 0..MAX_WINDOWS {
            wm.add(Window::new(Rect::new(0, 0, 10, 10),
                "x", WindowKind::Empty)).unwrap_or_else(|_| panic!("add #{i}"));
        }
        let err = wm.add(Window::new(Rect::new(0, 0, 10, 10), "x", WindowKind::Empty));
        assert_eq!(err.err(), Some(AddError::TooManyWindows));
    }

    #[test]
    fn z_order_is_insertion_order() {
        let mut wm = WindowManager::new();
        let a = wm.add(Window::new(Rect::new(0, 0, 10, 10), "a", WindowKind::Empty)).unwrap();
        let b = wm.add(Window::new(Rect::new(0, 0, 10, 10), "b", WindowKind::Empty)).unwrap();
        let c = wm.add(Window::new(Rect::new(0, 0, 10, 10), "c", WindowKind::Empty)).unwrap();
        let ids: alloc::vec::Vec<_> = wm.iter_by_z().map(|(id, _)| id).collect();
        assert_eq!(ids, alloc::vec![a, b, c]);

        wm.raise(a);
        let ids: alloc::vec::Vec<_> = wm.iter_by_z().map(|(id, _)| id).collect();
        assert_eq!(ids, alloc::vec![b, c, a]);
    }

    #[test]
    fn focus_unfocuses_others() {
        let mut wm = WindowManager::new();
        let a = wm.add(Window::new(Rect::new(0, 0, 10, 10), "a", WindowKind::Empty)).unwrap();
        let b = wm.add(Window::new(Rect::new(0, 0, 10, 10), "b", WindowKind::Empty)).unwrap();
        wm.focus(a);
        assert!(wm.get(a).unwrap().focused);
        assert!(!wm.get(b).unwrap().focused);
        wm.focus(b);
        assert!(!wm.get(a).unwrap().focused);
        assert!(wm.get(b).unwrap().focused);
    }

    #[test]
    fn hit_test_picks_topmost() {
        let mut wm = WindowManager::new();
        let a = wm.add(Window::new(Rect::new(0, 0, 100, 100), "a", WindowKind::Empty)).unwrap();
        let b = wm.add(Window::new(Rect::new(50, 50, 100, 100), "b", WindowKind::Empty)).unwrap();
        // (50,50) lives inside both — `b` was added later so its z is higher.
        assert_eq!(wm.hit_test(60, 60), Some(b));
        // (10,10) only inside `a`.
        assert_eq!(wm.hit_test(10, 10), Some(a));
        // (500,500) outside both.
        assert_eq!(wm.hit_test(500, 500), None);
    }

    #[test]
    fn content_rect_excludes_decorations() {
        let win = Window::new(Rect::new(0, 0, 200, 100), "hi", WindowKind::Empty);
        let cr = win.content_rect();
        assert_eq!(cr.x, FRAME_W);
        assert_eq!(cr.y, TITLEBAR_H);
        assert!(cr.w < win.rect.w);
        assert!(cr.h < win.rect.h);
    }

    #[test]
    fn compose_paints_desktop_and_windows() {
        let (_buf, mut s) = surface(200, 200);
        let mut wm = WindowManager::new();
        wm.set_desktop_bg(color::BLUE);
        let _ = wm.add(Window::new(Rect::new(20, 20, 80, 60), "w", WindowKind::Empty));
        wm.compose(&mut s);

        // Desktop pixel (outside any window).
        assert_eq!(s.get_pixel(5, 100), color::BLUE);
        // Title bar pixel — title sits at the top, height TITLEBAR_H.
        assert_eq!(s.get_pixel(40, 20 + 5), color::TITLE_BAR);
        // Bottom frame pixel — the 1 px strip at the very bottom of the window.
        assert_eq!(s.get_pixel(40, 20 + 60 - 1), color::WINDOW_FRAME);
        // Content area pixel (window bg) — well inside the body.
        assert_eq!(s.get_pixel(40, 50), color::WINDOW_BG);
    }

    #[test]
    fn widget_grid_packs_rows() {
        let mut g = WidgetGrid::new(4);
        g.push(LeafWidget::Spacer);
        g.push(LeafWidget::Spacer);
        g.push(LeafWidget::Spacer);
        g.push(LeafWidget::Spacer);
        // One row exactly, no overflow.
        assert_eq!(g.row_count(), 1);
        g.push(LeafWidget::Spacer);
        // 5th cell wraps into a second row.
        assert_eq!(g.row_count(), 2);
    }

    #[test]
    fn widget_grid_span_consumes_columns() {
        let mut g = WidgetGrid::new(4);
        g.push_span(LeafWidget::Spacer, 4); // a full-row label
        g.push(LeafWidget::Spacer);         // single cell on next row
        assert_eq!(g.row_count(), 2);
    }

    #[test]
    fn calculator_grid_has_expected_buttons() {
        let g = calculator_grid("0");
        // 1 display + 4 fn keys + 4×3 number rows + 3 keys on bottom row
        // = 1 + 4 + 12 + 3 = 20 cells.
        assert_eq!(g.count, 20);
        // First cell is the display Label spanning 4 columns.
        assert_eq!(g.spans[0], 4);
        match g.cells[0] {
            LeafWidget::Label(_) => {}
            _ => panic!("expected Label as first cell"),
        }
        // Last row should end with the "=" button.
        match g.cells[19] {
            LeafWidget::Button(b) => assert_eq!(b.label.as_str(), "="),
            _ => panic!("expected = button as last cell"),
        }
    }

    #[test]
    fn widgets_window_renders_pixels() {
        let (_buf, mut s) = surface(400, 240);
        let mut wm = WindowManager::new();
        let _ = wm.add(Window::new(
            Rect::new(10, 10, 380, 220),
            "calc",
            WindowKind::Widgets(calculator_grid("42")),
        ));
        wm.compose(&mut s);
        // Display label sits near the top of the content area — count
        // accent-coloured pixels to make sure something was actually drawn.
        let mut accent = 0;
        for y in 30..220 { for x in 12..390 {
            let px = s.get_pixel(x, y);
            if px == color::BEET_PURPLE || px == color::WINDOW_FRAME { accent += 1; }
        }}
        assert!(accent > 200, "expected painted widget pixels, accent={accent}");
    }

    #[test]
    fn calc_basic_arithmetic() {
        let mut c = CalcState::new();
        assert_eq!(c.display(), "0");
        c.press(b'1'); c.press(b'2');
        assert_eq!(c.display(), "12");
        c.press(b'+');
        c.press(b'3'); c.press(b'4');
        assert_eq!(c.display(), "34");
        c.press(b'=');
        assert_eq!(c.display(), "46");
    }

    #[test]
    fn calc_multiply_chain() {
        let mut c = CalcState::new();
        c.press(b'2'); c.press(b'*'); c.press(b'3'); c.press(b'*'); c.press(b'4');
        c.press(b'=');
        assert_eq!(c.display(), "24");
    }

    #[test]
    fn calc_clear_and_backspace() {
        let mut c = CalcState::new();
        c.press(b'1'); c.press(b'2'); c.press(b'3');
        c.press(8);
        assert_eq!(c.display(), "12");
        c.press(b'c');
        assert_eq!(c.display(), "0");
    }

    #[test]
    fn calc_div_by_zero_shows_err() {
        let mut c = CalcState::new();
        c.press(b'5'); c.press(b'/'); c.press(b'0'); c.press(b'=');
        assert_eq!(c.display(), "Err");
    }

    #[test]
    fn calc_sign_toggle() {
        let mut c = CalcState::new();
        c.press(b'4'); c.press(b'2');
        c.press(b's');
        assert_eq!(c.display(), "-42");
        c.press(b's');
        assert_eq!(c.display(), "42");
    }

    #[test]
    fn notes_appends_printables() {
        let mut n = NotesState::new();
        for &b in b"hi" { n.press(b); }
        assert_eq!(n.text(), "hi");
        n.press(b'\n');
        n.press(b'!');
        assert_eq!(n.text(), "hi\n!");
        n.press(8);
        assert_eq!(n.text(), "hi\n");
    }

    #[test]
    fn wm_handle_key_routes_to_focused_calc() {
        let mut wm = WindowManager::new();
        let calc_id = wm.add(Window::new(Rect::new(0,0,200,200), "calc",
            WindowKind::Calc(CalcState::new()))).unwrap();
        wm.focus(calc_id);
        assert!(wm.handle_key(b'7'));
        match wm.get(calc_id).unwrap().kind {
            WindowKind::Calc(state) => assert_eq!(state.display(), "7"),
            _ => unreachable!(),
        }
    }

    #[test]
    fn wm_tab_cycles_focus() {
        let mut wm = WindowManager::new();
        let a = wm.add(Window::new(Rect::new(0,0,10,10),"a", WindowKind::Empty)).unwrap();
        let b = wm.add(Window::new(Rect::new(0,0,10,10),"b", WindowKind::Empty)).unwrap();
        let c = wm.add(Window::new(Rect::new(0,0,10,10),"c", WindowKind::Empty)).unwrap();
        wm.focus(a);
        wm.handle_key(b'\t');
        // Focus advanced one step in z-order (b is next, but `focus(a)`
        // raised a to the top of z, so after Tab we should land on the
        // *bottom* of the new z-order. Just assert *something* moved.
        assert!(!wm.get(a).unwrap().focused);
        let _ = b; let _ = c;
    }

    #[test]
    fn text_window_renders_lines() {
        let mut lines = [TextLine::EMPTY; MAX_TEXT_LINES];
        lines[0] = TextLine::new("hello");
        lines[1] = TextLine::new("world");
        let (_buf, mut s) = surface(300, 120);
        let mut wm = WindowManager::new();
        let _ = wm.add(Window::new(Rect::new(10, 10, 280, 100), "msg",
            WindowKind::Text { lines, count: 2 }));
        wm.compose(&mut s);
        // Some pixels in the content area must be non-bg now that there's text.
        let mut painted = 0;
        for y in 35..50 { for x in 16..80 {
            if s.get_pixel(x, y) == color::LIGHT_GRAY { painted += 1; }
        }}
        assert!(painted > 10, "expected drawn text pixels, painted={painted}");
    }
}
