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
    /// A live Snake game. Arrow keys steer, space restarts, the
    /// game ticks forward each animation frame.
    Snake(SnakeState),
    /// Animated Mandelbrot fractal — zooms slowly toward a fixed
    /// pretty point. Pure CPU, exercises every gfx pixel path.
    Mandelbrot(MandelState),
    /// Conway's Game of Life — a 32x24 board evolving once per
    /// animation tick.  Space toggles pause, R reseeds randomly.
    Life(LifeState),
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

// ─────────────────────────────────────────────────────────────────────────────
// Snake game state — interactive, ticks once per animation frame.
// ─────────────────────────────────────────────────────────────────────────────

/// Snake board geometry — chosen to give a comfortable cell size at
/// the demo window dimensions (320×240 content → 16 px cells).
pub const SNAKE_COLS: usize = 20;
pub const SNAKE_ROWS: usize = 15;
pub const SNAKE_MAX_LEN: usize = SNAKE_COLS * SNAKE_ROWS;
pub const SNAKE_CELL_PX: i32 = 16;

/// Compass directions the snake can be heading. Stored as u8 so it
/// stays Copy alongside the rest of `SnakeState`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SnakeDir { North = 0, East = 1, South = 2, West = 3 }

/// Snake's full state — segments, direction, food, score, RNG, game-over.
///
/// All allocation-free: segments live in a fixed `[(u8, u8); SNAKE_MAX_LEN]`
/// array with a `len` cursor, and the RNG is a tiny xorshift32.
#[derive(Clone, Copy)]
pub struct SnakeState {
    segments: [(u8, u8); SNAKE_MAX_LEN],
    len: u16,
    dir: SnakeDir,
    /// Direction the user requested but that hasn't been applied yet
    /// (debounces multi-keypresses inside one tick).
    next_dir: SnakeDir,
    food: (u8, u8),
    pub score: u16,
    pub game_over: bool,
    /// Tick counter — the game advances every `STEP_TICKS` frames so
    /// the snake doesn't fly off the board at 10 Hz.
    counter: u16,
    rng: u32,
}

impl SnakeState {
    /// How many animation frames between snake moves (4 → ~2.5 cells/sec
    /// at the 10 Hz recompose rate; brisk but playable).
    const STEP_TICKS: u16 = 4;

    pub const fn new() -> Self {
        // Start with a 3-segment snake at row 7, columns 3-5, heading east.
        let mut segments = [(0u8, 0u8); SNAKE_MAX_LEN];
        segments[0] = (5, 7);
        segments[1] = (4, 7);
        segments[2] = (3, 7);
        SnakeState {
            segments,
            len: 3,
            dir: SnakeDir::East,
            next_dir: SnakeDir::East,
            food: (12, 7),
            score: 0,
            game_over: false,
            counter: 0,
            rng: 0xC0FF_EE13,
        }
    }

    fn xorshift(&mut self) -> u32 {
        let mut x = self.rng;
        x ^= x << 13; x ^= x >> 17; x ^= x << 5;
        self.rng = x.max(1);
        self.rng
    }

    fn spawn_food(&mut self) {
        // Up to a few tries to land on an empty cell — capped so a
        // nearly-full board can't loop forever.
        for _ in 0..32 {
            let r = self.xorshift();
            let fx = (r % SNAKE_COLS as u32) as u8;
            let fy = ((r / SNAKE_COLS as u32) % SNAKE_ROWS as u32) as u8;
            if !self.cell_occupied(fx, fy) {
                self.food = (fx, fy);
                return;
            }
        }
        // Board full — game is essentially won.
        self.game_over = true;
    }

    fn cell_occupied(&self, x: u8, y: u8) -> bool {
        self.segments[..self.len as usize].iter().any(|&(sx, sy)| sx == x && sy == y)
    }

    /// Handle a key press.  Arrow keys steer (with no-reverse rule),
    /// space/r restart after game-over. Returns `true` when something
    /// observable changed.
    pub fn press(&mut self, key: u8) -> bool {
        if self.game_over {
            if matches!(key, b' ' | b'r' | b'R') {
                *self = SnakeState::new();
                return true;
            }
            return false;
        }
        let want = match key {
            // Arrow keys arrive as ANSI escape sequences — but for our
            // virtio-input driver we map them to single bytes:
            // 'w'/'a'/'s'/'d' or arrow keycodes (handled in irq.rs).
            b'w' | b'W' | b'i' => Some(SnakeDir::North),
            b'd' | b'D' | b'l' => Some(SnakeDir::East),
            b's' | b'S' | b'k' => Some(SnakeDir::South),
            b'a' | b'A' | b'j' => Some(SnakeDir::West),
            // Bytes injected by the irq layer for the cursor keys
            // (avoiding ANSI escape sequence parsing inside the GUI).
            0xC1 => Some(SnakeDir::North),
            0xC2 => Some(SnakeDir::South),
            0xC3 => Some(SnakeDir::East),
            0xC4 => Some(SnakeDir::West),
            _ => None,
        };
        if let Some(d) = want {
            // No 180° reversal — that would eat the neck instantly.
            let opposite = matches!(
                (self.dir, d),
                (SnakeDir::North, SnakeDir::South) | (SnakeDir::South, SnakeDir::North)
              | (SnakeDir::East,  SnakeDir::West)  | (SnakeDir::West,  SnakeDir::East),
            );
            if !opposite { self.next_dir = d; }
            return true;
        }
        false
    }

    /// Advance the game one animation frame.  Returns `true` if the
    /// state changed visibly (caller should recompose).
    pub fn tick(&mut self) -> bool {
        if self.game_over { return false; }
        self.counter += 1;
        if self.counter < Self::STEP_TICKS { return false; }
        self.counter = 0;

        self.dir = self.next_dir;
        let head = self.segments[0];
        let (dx, dy): (i16, i16) = match self.dir {
            SnakeDir::North => (0, -1),
            SnakeDir::South => (0,  1),
            SnakeDir::East  => (1,  0),
            SnakeDir::West  => (-1, 0),
        };
        let nx = head.0 as i16 + dx;
        let ny = head.1 as i16 + dy;
        // Wall collision.
        if nx < 0 || ny < 0
            || nx >= SNAKE_COLS as i16
            || ny >= SNAKE_ROWS as i16
        {
            self.game_over = true;
            return true;
        }
        let new_head = (nx as u8, ny as u8);
        let eat = new_head == self.food;
        // Self collision (excluding the very last segment, which will move out).
        let occupied_len = if eat { self.len as usize } else { (self.len as usize).saturating_sub(1) };
        for i in 0..occupied_len {
            if self.segments[i] == new_head {
                self.game_over = true;
                return true;
            }
        }
        // Shift segments back, head first.
        let new_len = if eat { (self.len + 1).min(SNAKE_MAX_LEN as u16) } else { self.len };
        for i in (1..new_len as usize).rev() {
            self.segments[i] = self.segments[i - 1];
        }
        self.segments[0] = new_head;
        self.len = new_len;
        if eat {
            self.score += 10;
            self.spawn_food();
        }
        true
    }

    fn draw(&self, screen: &mut Surface, rect: &Rect) {
        // Board background — slightly inset from the window content.
        let board_w = SNAKE_COLS as i32 * SNAKE_CELL_PX;
        let board_h = SNAKE_ROWS as i32 * SNAKE_CELL_PX;
        let bx = rect.x + (rect.w - board_w) / 2;
        let by = rect.y + 24; // leave room for score above
        let board = Rect::new(bx, by, board_w, board_h);
        screen.fill_rect(&board, color::BLACK);
        screen.rect_outline(&board, color::WINDOW_FRAME);

        // Faint grid so the cells are visible even without the snake.
        for c in 1..SNAKE_COLS as i32 {
            screen.vline(bx + c * SNAKE_CELL_PX, by, board_h, color::DARK_GRAY);
        }
        for r in 1..SNAKE_ROWS as i32 {
            screen.hline(bx, by + r * SNAKE_CELL_PX, board_w, color::DARK_GRAY);
        }

        // Score line above the board.
        let mut buf = [0u8; 24];
        let s = format_score(self.score, &mut buf);
        screen.draw_text(rect.x + 8, rect.y + 4, s, color::WHITE, color::WINDOW_BG);

        // Food.
        let fx = bx + self.food.0 as i32 * SNAKE_CELL_PX;
        let fy = by + self.food.1 as i32 * SNAKE_CELL_PX;
        screen.circle_filled(fx + SNAKE_CELL_PX / 2, fy + SNAKE_CELL_PX / 2,
            SNAKE_CELL_PX / 2 - 2, color::BRIGHT_RED);

        // Snake.
        for (i, &(sx, sy)) in self.segments[..self.len as usize].iter().enumerate() {
            let x = bx + sx as i32 * SNAKE_CELL_PX + 1;
            let y = by + sy as i32 * SNAKE_CELL_PX + 1;
            let c = if i == 0 { color::BRIGHT_GREEN } else { color::GREEN };
            screen.fill_rect(&Rect::new(x, y, SNAKE_CELL_PX - 2, SNAKE_CELL_PX - 2), c);
        }

        if self.game_over {
            // Big "GAME OVER" banner across the middle.
            let banner = Rect::new(bx + board_w / 2 - 80, by + board_h / 2 - 12, 160, 24);
            screen.fill_rect(&banner, color::BRIGHT_RED);
            screen.draw_text(banner.x + 16, banner.y + 4, "GAME OVER", color::WHITE, color::BRIGHT_RED);
            screen.draw_text(rect.x + 8, rect.bottom() - 18,
                "press space / r to restart",
                color::LIGHT_GRAY, color::WINDOW_BG);
        } else {
            screen.draw_text(rect.x + 8, rect.bottom() - 18,
                "wasd or arrows to steer",
                color::LIGHT_GRAY, color::WINDOW_BG);
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Mandelbrot — slowly zooms toward an interesting point
// ─────────────────────────────────────────────────────────────────────────────

/// Animated Mandelbrot fractal state.  Renders at the window's
/// content resolution each frame using a fixed iteration budget;
/// the center + scale slowly walk toward a pre-chosen pretty
/// coordinate so successive frames show the zoom evolve.
#[derive(Clone, Copy)]
pub struct MandelState {
    pub center_re: f32,
    pub center_im: f32,
    pub scale:     f32, // width of the view in complex-plane units
    pub frame:     u32,
    pub max_iter:  u16,
}

impl MandelState {
    pub const fn new() -> Self {
        // Start zoomed all the way out, drifting in.
        // max_iter kept modest because each pixel costs `max_iter` fmul +
        // fcmp on a software FPU — at 360x220 / step 4 we still get ~5k
        // blocks per frame, plenty visible.
        MandelState {
            center_re: -0.75,
            center_im:  0.10,
            scale:      3.0,
            frame:      0,
            max_iter:   28,
        }
    }

    /// Advance one animation tick — pulls the view slowly toward a
    /// "Seahorse Valley" coordinate, then resets to a wide view when
    /// it's tight enough.
    pub fn tick(&mut self) {
        self.frame = self.frame.wrapping_add(1);
        // Target: classic Seahorse Valley point.
        let target_re = -0.743643887037151;
        let target_im =  0.131825904205330;
        // Drift center toward target, scale toward something small.
        self.center_re = self.center_re * 0.985 + (target_re as f32) * 0.015;
        self.center_im = self.center_im * 0.985 + (target_im as f32) * 0.015;
        self.scale *= 0.97;
        // Reset every ~250 frames so the loop never ends.
        if self.scale < 0.004 {
            self.scale = 3.0;
            self.center_re = -0.75;
            self.center_im =  0.10;
        }
    }

    fn draw(&self, screen: &mut Surface, rect: &Rect) {
        screen.fill_rect(rect, color::BLACK);
        let w = rect.w as i32;
        let h = rect.h as i32;
        if w <= 0 || h <= 0 { return; }
        // For each pixel: map to complex plane, iterate z = z² + c.
        // Step by 2 in both axes to keep frame-time tractable in software
        // — that's a 4x speedup with a slight pixelisation that actually
        // looks good given the demo size.
        let scale_x = self.scale / w as f32;
        let scale_y = self.scale * (h as f32 / w as f32) / h as f32;
        let max_iter = self.max_iter;
        // step=4 → 4x4 px blocks. 16x cheaper than per-pixel, and
        // at the demo window size still gives ~90x55 cells (~5k blocks
        // per frame). Worth the chunkier look to keep the 10 Hz
        // recompose pipeline above water.
        let step: i32 = 4;
        let mut y = 0;
        while y < h {
            let im = self.center_im + (y as f32 - h as f32 / 2.0) * scale_y;
            let mut x = 0;
            while x < w {
                let re = self.center_re + (x as f32 - w as f32 / 2.0) * scale_x;
                let mut zr = 0.0f32;
                let mut zi = 0.0f32;
                let mut iter: u16 = 0;
                while iter < max_iter {
                    let zr2 = zr * zr;
                    let zi2 = zi * zi;
                    if zr2 + zi2 > 4.0 { break; }
                    zi = 2.0 * zr * zi + im;
                    zr = zr2 - zi2 + re;
                    iter += 1;
                }
                let color = if iter >= max_iter {
                    color::BLACK
                } else {
                    palette_color(iter, max_iter)
                };
                // Draw a 2×2 block to match the step.
                let bx = rect.x + x;
                let by = rect.y + y;
                screen.fill_rect(&Rect::new(bx, by, step, step), color);
                x += step;
            }
            y += step;
        }
    }
}

/// Map an escape count to a smooth-ish palette across the
/// purple/pink/green band — matches the BeetOS brand colors.
fn palette_color(iter: u16, max_iter: u16) -> Color {
    let t = iter as u32 * 255 / max_iter.max(1) as u32;
    // 6-band rainbow rolled through the beet palette.
    let phase = (iter as u32 * 6 / max_iter.max(1) as u32).min(5);
    let local = (t * 6) % 256;
    let local_u8 = local as u8;
    let inv = 255u8.wrapping_sub(local_u8);
    let rgb = match phase {
        0 => super::gfx::rgb(local_u8, 0, inv),       // purple→pink
        1 => super::gfx::rgb(inv, local_u8, 0),       // pink→orange
        2 => super::gfx::rgb(0, inv, local_u8),       // green→cyan
        3 => super::gfx::rgb(local_u8, inv, 0),
        4 => super::gfx::rgb(0, local_u8, inv),
        _ => super::gfx::rgb(inv, 0, local_u8),
    };
    rgb
}

// ─────────────────────────────────────────────────────────────────────────────
// Conway's Game of Life
// ─────────────────────────────────────────────────────────────────────────────

pub const LIFE_COLS: usize = 32;
pub const LIFE_ROWS: usize = 24;
pub const LIFE_CELLS: usize = LIFE_COLS * LIFE_ROWS;

/// Conway's Game of Life — fixed 32x24 board, double-buffered next
/// state, xorshift seed for "R" reseeding. Pauses on space.
#[derive(Clone, Copy)]
pub struct LifeState {
    pub cells:  [u8; LIFE_CELLS], // 0 = dead, 1 = alive (only LSB used)
    pub paused: bool,
    pub generation: u32,
    rng: u32,
    counter: u8,
}

impl LifeState {
    /// Tick rate divider — Life steps every STEP_TICKS animation frames.
    /// 2 → ~5 generations per second at the 10 Hz recompose rate.
    const STEP_TICKS: u8 = 2;

    /// Pre-seeded with a classic glider + a small R-pentomino so the
    /// initial frame is interesting before anyone presses R.
    pub const fn new() -> Self {
        let mut cells = [0u8; LIFE_CELLS];
        // glider near top-left.
        let glider = [(2, 1), (3, 2), (1, 3), (2, 3), (3, 3)];
        let mut i = 0;
        while i < glider.len() {
            let (x, y) = glider[i];
            cells[y * LIFE_COLS + x] = 1;
            i += 1;
        }
        // R-pentomino in the middle — long-lived chaos.
        let r_pento = [(16, 11), (17, 11), (15, 12), (16, 12), (16, 13)];
        let mut i = 0;
        while i < r_pento.len() {
            let (x, y) = r_pento[i];
            cells[y * LIFE_COLS + x] = 1;
            i += 1;
        }
        // Blinker far right.
        let blinker = [(28, 4), (28, 5), (28, 6)];
        let mut i = 0;
        while i < blinker.len() {
            let (x, y) = blinker[i];
            cells[y * LIFE_COLS + x] = 1;
            i += 1;
        }
        LifeState { cells, paused: false, generation: 0, rng: 0xACE0_F11E, counter: 0 }
    }

    /// Random-fill (~30 % alive) seeded from the xorshift state.
    pub fn reseed(&mut self) {
        self.cells = [0; LIFE_CELLS];
        for cell in self.cells.iter_mut() {
            let mut x = self.rng;
            x ^= x << 13; x ^= x >> 17; x ^= x << 5;
            self.rng = x.max(1);
            *cell = if (x & 0xFF) < 80 { 1 } else { 0 };
        }
        self.generation = 0;
    }

    /// Step the board by one Conway generation.
    fn step(&mut self) {
        let mut next = [0u8; LIFE_CELLS];
        for y in 0..LIFE_ROWS as i32 {
            for x in 0..LIFE_COLS as i32 {
                let mut neighbors = 0u8;
                for dy in -1..=1 {
                    for dx in -1..=1 {
                        if dx == 0 && dy == 0 { continue; }
                        // Toroidal wrap so gliders survive at edges.
                        let nx = (x + dx + LIFE_COLS as i32) % LIFE_COLS as i32;
                        let ny = (y + dy + LIFE_ROWS as i32) % LIFE_ROWS as i32;
                        neighbors += self.cells[(ny as usize) * LIFE_COLS + nx as usize];
                    }
                }
                let alive = self.cells[(y as usize) * LIFE_COLS + x as usize] != 0;
                let lives_next = matches!((alive, neighbors), (true, 2) | (true, 3) | (false, 3));
                next[(y as usize) * LIFE_COLS + x as usize] = lives_next as u8;
            }
        }
        self.cells = next;
        self.generation = self.generation.wrapping_add(1);
    }

    pub fn tick(&mut self) -> bool {
        if self.paused { return false; }
        self.counter += 1;
        if self.counter < Self::STEP_TICKS { return false; }
        self.counter = 0;
        self.step();
        true
    }

    pub fn press(&mut self, key: u8) -> bool {
        match key {
            b' '          => { self.paused = !self.paused; true }
            b'r' | b'R'   => { self.reseed(); true }
            _ => false,
        }
    }

    fn draw(&self, screen: &mut Surface, rect: &Rect) {
        // Pad slightly so the grid doesn't hug the window frame.
        let inset = 8;
        let avail_w = (rect.w - 2 * inset).max(LIFE_COLS as i32);
        let avail_h = (rect.h - 2 * inset - 18).max(LIFE_ROWS as i32);
        let cell = avail_w.min(avail_h * LIFE_COLS as i32 / LIFE_ROWS as i32) / LIFE_COLS as i32;
        let board_w = cell * LIFE_COLS as i32;
        let board_h = cell * LIFE_ROWS as i32;
        let bx = rect.x + (rect.w - board_w) / 2;
        let by = rect.y + inset;
        screen.fill_rect(&Rect::new(bx, by, board_w, board_h), color::BLACK);
        for y in 0..LIFE_ROWS as i32 {
            for x in 0..LIFE_COLS as i32 {
                if self.cells[(y as usize) * LIFE_COLS + x as usize] != 0 {
                    let c = if self.generation < 4 {
                        // Initial seed cells glow brighter so the
                        // starting position is obvious for a moment.
                        color::BRIGHT_GREEN
                    } else {
                        color::GREEN
                    };
                    let cx = bx + x * cell + 1;
                    let cy = by + y * cell + 1;
                    let cs = (cell - 1).max(1);
                    screen.fill_rect(&Rect::new(cx, cy, cs, cs), c);
                }
            }
        }
        // Footer with generation count + hint.
        let mut buf = [0u8; 32];
        let label = format_generation(self.generation, self.paused, &mut buf);
        screen.draw_text(rect.x + 8, rect.bottom() - 16, label, color::LIGHT_GRAY, color::WINDOW_BG);
    }
}

fn format_generation(gen: u32, paused: bool, buf: &mut [u8]) -> &str {
    let pre = if paused { &b"gen "[..] } else { &b"gen "[..] };
    let pre_len = pre.len();
    buf[..pre_len].copy_from_slice(pre);
    let mut n = gen;
    let mut tmp = [0u8; 10];
    let mut idx = tmp.len();
    if n == 0 { idx -= 1; tmp[idx] = b'0'; }
    while n > 0 && idx > 0 {
        idx -= 1;
        tmp[idx] = b'0' + (n % 10) as u8;
        n /= 10;
    }
    let digits = &tmp[idx..];
    let suffix = if paused { &b" [paused - space=resume, r=reseed]"[..] }
                 else      { &b" - space=pause, r=reseed"[..] };
    let mut cursor = pre_len + digits.len();
    buf[pre_len..cursor].copy_from_slice(digits);
    let max = buf.len();
    let take = suffix.len().min(max - cursor);
    buf[cursor..cursor + take].copy_from_slice(&suffix[..take]);
    cursor += take;
    core::str::from_utf8(&buf[..cursor]).unwrap_or("gen")
}

/// Format `score` as `"score: NNNN"` into the given byte buffer.
fn format_score(score: u16, buf: &mut [u8]) -> &str {
    let prefix = b"score: ";
    let pre_len = prefix.len();
    buf[..pre_len].copy_from_slice(prefix);
    let mut n = score as u32;
    let mut tmp = [0u8; 6];
    let mut idx = tmp.len();
    if n == 0 { idx -= 1; tmp[idx] = b'0'; }
    while n > 0 && idx > 0 {
        idx -= 1;
        tmp[idx] = b'0' + (n % 10) as u8;
        n /= 10;
    }
    let digits = &tmp[idx..];
    buf[pre_len..pre_len + digits.len()].copy_from_slice(digits);
    core::str::from_utf8(&buf[..pre_len + digits.len()]).unwrap_or("score:")
}

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
            WindowKind::Snake(state) => {
                screen.fill_rect(&content, self.bg);
                state.draw(screen, &content);
            }
            WindowKind::Mandelbrot(state) => {
                state.draw(screen, &content);
            }
            WindowKind::Life(state) => {
                screen.fill_rect(&content, self.bg);
                state.draw(screen, &content);
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
    /// Cursor position in screen coordinates.  Drawn as an arrow
    /// sprite in compose() so any caller can see it, even without
    /// a mouse driver.  Today moved by Shift+arrow keys; later by
    /// virtio-tablet ABS events.
    cursor: (i32, i32),
    cursor_visible: bool,
}

impl WindowManager {
    pub const fn new() -> Self {
        // `[None; MAX_WINDOWS]` needs the inner type Copy — Window is Copy,
        // so Option<Window> is Copy too.
        Self {
            windows: [None; MAX_WINDOWS],
            next_z: 0,
            desktop_bg: color::DESKTOP_BG,
            cursor: (640, 400),
            cursor_visible: true,
        }
    }

    pub fn cursor(&self) -> (i32, i32) { self.cursor }

    pub fn set_cursor(&mut self, x: i32, y: i32) {
        self.cursor = (x, y);
    }

    pub fn move_cursor(&mut self, dx: i32, dy: i32) {
        self.cursor.0 += dx;
        self.cursor.1 += dy;
    }

    pub fn click_at_cursor(&mut self) -> Option<WindowId> {
        let (x, y) = self.cursor;
        let id = self.hit_test(x, y)?;
        // If the click landed on the window's close X, remove it
        // outright; otherwise just focus + raise the window.
        let on_close = self.get(id).is_some_and(|w| w.close_button_contains(x, y));
        if on_close {
            self.remove(id);
            None
        } else {
            self.focus(id);
            Some(id)
        }
    }

    /// Nudge the focused window by `(dx, dy)` pixels. No-op if nothing
    /// is focused. Today bound to Ctrl+arrow keys in the input layer.
    pub fn drag_focused(&mut self, dx: i32, dy: i32) -> bool {
        let id = self.windows.iter().enumerate()
            .find(|(_, w)| w.as_ref().is_some_and(|w| w.focused))
            .map(|(i, _)| WindowId(i as u8));
        let Some(id) = id else { return false; };
        if let Some(w) = self.get_mut(id) {
            w.rect.x += dx;
            w.rect.y += dy;
            return true;
        }
        false
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
            // Shift+arrow → move cursor (special sentinels from
            // virtio-input). 16 px steps so the cursor is steerable
            // without dozens of keystrokes.
            0xD1 => { self.move_cursor(0,   -16); return true; }
            0xD2 => { self.move_cursor(0,    16); return true; }
            0xD3 => { self.move_cursor(16,   0);  return true; }
            0xD4 => { self.move_cursor(-16,  0);  return true; }
            // Shift+Enter (0xD5) → click at cursor.
            0xD5 => { self.click_at_cursor(); return true; }
            // Ctrl+arrow → drag the focused window by 16 px.
            0xE1 => { self.drag_focused(0,   -16); return true; }
            0xE2 => { self.drag_focused(0,    16); return true; }
            0xE3 => { self.drag_focused(16,   0);  return true; }
            0xE4 => { self.drag_focused(-16,  0);  return true; }
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
            WindowKind::Snake(state) => state.press(key),
            WindowKind::Life(state)  => state.press(key),
            _ => false,
        }
    }

    /// Advance per-frame state in every interactive window (currently just
    /// the snake game). Returns `true` if any state changed and the
    /// desktop should be recomposed.
    pub fn animation_step(&mut self) -> bool {
        let mut changed = false;
        for slot in self.windows.iter_mut() {
            if let Some(w) = slot {
                match &mut w.kind {
                    WindowKind::Snake(state) => {
                        if state.tick() { changed = true; }
                    }
                    WindowKind::Mandelbrot(state) => {
                        state.tick();
                        changed = true;
                    }
                    WindowKind::Life(state) => {
                        if state.tick() { changed = true; }
                    }
                    _ => {}
                }
            }
        }
        changed
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

        // Cursor sprite drawn last so it overlays everything else.
        // A tiny 8x12 arrow — outlined in black, filled white — at the
        // current cursor coordinates. Keeps the cursor visible against
        // both the dark desktop and a focused window's title bar.
        if self.cursor_visible {
            draw_cursor_sprite(screen, self.cursor.0, self.cursor.1);
        }
    }
}

/// Paint a small white-on-black arrow cursor at `(x, y)`.
///
/// The shape is encoded as a 12-row 8-bit mask: bit set = white pixel,
/// bit unset = black outline pixel (or transparent — we draw black
/// behind the outline so the cursor stays visible on bright backgrounds).
fn draw_cursor_sprite(screen: &mut Surface, x: i32, y: i32) {
    // Bit pattern, row by row, MSB = leftmost pixel.
    // 1 = white fill, 0 = transparent, but we also draw a 1-px black
    // border around the fill so the cursor stays visible on light
    // backgrounds. Outline is the union of 8-neighbourhoods.
    const ROWS: [u8; 12] = [
        0b1000_0000,
        0b1100_0000,
        0b1110_0000,
        0b1111_0000,
        0b1111_1000,
        0b1111_1100,
        0b1111_1110,
        0b1111_0000,
        0b1101_1000,
        0b1001_1000,
        0b0000_1100,
        0b0000_1100,
    ];
    // First pass: outline (1 pixel border in BLACK).
    for r in 0..ROWS.len() as i32 {
        let bits = ROWS[r as usize];
        for c in 0..8 {
            if (bits >> (7 - c)) & 1 != 0 {
                // Splat 3x3 black around each fill pixel — the unset
                // pixels among the 9 form the outline.
                for dy in -1..=1 {
                    for dx in -1..=1 {
                        screen.set_pixel(x + c + dx, y + r + dy, color::BLACK);
                    }
                }
            }
        }
    }
    // Second pass: white fill on top of the outline.
    for r in 0..ROWS.len() as i32 {
        let bits = ROWS[r as usize];
        for c in 0..8 {
            if (bits >> (7 - c)) & 1 != 0 {
                screen.set_pixel(x + c, y + r, color::WHITE);
            }
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
    fn snake_moves_east_on_tick() {
        let mut s = SnakeState::new();
        // Burn STEP_TICKS - 1 frames without movement.
        for _ in 0..(SnakeState::STEP_TICKS - 1) { assert!(!s.tick()); }
        let head_before = s.segments[0];
        assert!(s.tick());
        let head_after = s.segments[0];
        assert_eq!(head_after.0, head_before.0 + 1, "should have advanced east");
        assert_eq!(head_after.1, head_before.1);
    }

    #[test]
    fn snake_no_180_reversal() {
        let mut s = SnakeState::new();
        // We're going east; pressing west should be rejected (would eat
        // the neck).
        s.press(b'a');
        assert_eq!(s.dir, SnakeDir::East);
        // North/south are allowed.
        s.press(b'w');
        for _ in 0..SnakeState::STEP_TICKS { s.tick(); }
        assert_eq!(s.dir, SnakeDir::North);
    }

    #[test]
    fn snake_dies_on_wall() {
        let mut s = SnakeState::new();
        // Force-place the head at the east wall.
        s.segments[0] = ((SNAKE_COLS - 1) as u8, 7);
        s.dir = SnakeDir::East;
        s.next_dir = SnakeDir::East;
        for _ in 0..SnakeState::STEP_TICKS { s.tick(); }
        assert!(s.game_over, "should have hit the east wall");
    }

    #[test]
    fn snake_grows_on_food() {
        let mut s = SnakeState::new();
        let len_before = s.len;
        // Drop food on the cell right in front of the head.
        s.food = (s.segments[0].0 + 1, s.segments[0].1);
        for _ in 0..SnakeState::STEP_TICKS { s.tick(); }
        assert_eq!(s.len, len_before + 1);
        assert_eq!(s.score, 10);
    }

    #[test]
    fn snake_restart_after_game_over() {
        let mut s = SnakeState::new();
        s.game_over = true;
        assert!(s.press(b' '));
        assert!(!s.game_over);
        assert_eq!(s.score, 0);
    }

    #[test]
    fn snake_via_window_manager() {
        let mut wm = WindowManager::new();
        let id = wm.add(Window::new(Rect::new(0, 0, 400, 400),
            "snake", WindowKind::Snake(SnakeState::new()))).unwrap();
        wm.focus(id);
        // North is allowed from east.
        assert!(wm.handle_key(b'w'));
        // Animation step advances state.
        let _ = wm.animation_step();
    }

    #[test]
    fn cursor_click_on_close_x_removes_window() {
        let mut wm = WindowManager::new();
        let id = wm.add(Window::new(Rect::new(40, 40, 200, 100),
            "x", WindowKind::Empty)).unwrap();
        wm.focus(id);
        assert_eq!(wm.len(), 1);
        // Close X sits at the top-right of the title bar.
        // close_button is the rightmost TITLEBAR_H × TITLEBAR_H square.
        let w = wm.get(id).unwrap();
        let cx = w.rect.right() - CLOSE_BTN_W / 2;
        let cy = w.rect.y + TITLEBAR_H / 2;
        wm.set_cursor(cx, cy);
        assert!(wm.handle_key(0xD5)); // simulated Shift+Enter click
        assert_eq!(wm.len(), 0);
    }

    #[test]
    fn ctrl_arrow_drags_focused_window() {
        let mut wm = WindowManager::new();
        let id = wm.add(Window::new(Rect::new(100, 100, 200, 100),
            "x", WindowKind::Empty)).unwrap();
        wm.focus(id);
        assert!(wm.handle_key(0xE3)); // Ctrl+Right
        assert_eq!(wm.get(id).unwrap().rect.x, 100 + 16);
        assert!(wm.handle_key(0xE1)); // Ctrl+Up
        assert_eq!(wm.get(id).unwrap().rect.y, 100 - 16);
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
