// SPDX-FileCopyrightText: 2025 BeetOS contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Framebuffer + window-manager seam for BCM2712.
//!
//! The shape mirrors `qemu_virt::fb` so the boot code, IRQ handler,
//! and `beetos::gui` consumers don't care which platform they're on
//! — both expose:
//!
//!   - `is_fb_ready()` / `surface()`
//!   - `with_wm(closure)` / `compose_desktop()` / `tick_recompose_if_due(tick)`
//!   - `populate_demo_desktop()`
//!
//! The big difference is provenance: where `qemu_virt::fb::init`
//! programs ramfb via FW_CFG DMA and uses a fixed reserved RAM
//! region, here we ask the VideoCore firmware via the BCM2835
//! mailbox property interface for a framebuffer it has already
//! allocated. On real RPi5 hardware `start4.elf` has set up HDMI
//! before our kernel even runs, so the firmware can answer with
//! the existing FB without re-negotiating.
//!
//! Not exercised by `cargo xtask qemu-smoke` because QEMU 8.2.2
//! lacks a raspi5 machine; verified against the BCM mailbox
//! protocol docs.

use beetos::gfx::{color, Color, Rect, Surface};
use beetos::gui::{
    CalcState, LifeState, MandelState, NotesState, SnakeState, TextLine, Window, WindowKind,
    WindowManager, MAX_TEXT_LINES,
};
use beetos::phys_to_virt;

use super::mailbox::{self, FbInfo};

// ─────────────────────────────────────────────────────────────────────────────
// FB state
// ─────────────────────────────────────────────────────────────────────────────

static mut FB_INFO: Option<FbInfo> = None;
static mut FB_READY: bool = false;

/// Global window manager — same const-initialised slab as the qemu_virt
/// path, gated through `with_wm` so the unsafe-static touch stays in one
/// place.
static mut WINDOW_MANAGER: WindowManager = WindowManager::new();

#[inline] pub fn is_fb_ready() -> bool { unsafe { FB_READY } }

pub fn with_wm<R>(f: impl FnOnce(&mut WindowManager) -> R) -> R {
    unsafe { f(&mut *core::ptr::addr_of_mut!(WINDOW_MANAGER)) }
}

/// Try to bring up the framebuffer via mailbox.  Returns `true` on
/// success. Idempotent — re-calling just re-queries (firmware
/// answers with the same FB it already has).
pub fn init(width: u32, height: u32) -> bool {
    match mailbox::alloc_framebuffer(width, height) {
        Some(fb) => {
            unsafe {
                FB_INFO = Some(fb);
                FB_READY = true;
            }
            true
        }
        None => false,
    }
}

/// Borrow the live framebuffer as a [`Surface`]. Returns a dummy
/// 0×0 surface if the FB hasn't been initialised — callers should
/// guard with `is_fb_ready()` rather than rely on this.
///
/// # Safety
///
/// Caller guarantees single-threaded access; today the BCM2712 boot
/// path is single-CPU end-to-end.
pub unsafe fn surface() -> Surface {
    if let Some(fb) = (*core::ptr::addr_of_mut!(FB_INFO)).as_ref() {
        // Firmware returns the FB address in the VC's "bus address"
        // space; phys = bus & 0x3FFF_FFFF on every Pi since BCM2835.
        // alloc_framebuffer already masks that bit pattern.
        let base = phys_to_virt(fb.addr) as *mut u32;
        let stride_px = (fb.pitch / 4) as i32;
        Surface::from_raw_parts(base, fb.width as i32, fb.height as i32, stride_px)
    } else {
        // Dummy surface so callers don't have to Option-wrap
        // — any draw call will hit the bounds check and noop.
        Surface::from_raw_parts(core::ptr::null_mut(), 0, 0, 0)
    }
}

/// Re-render the desktop. Reads the BCM Generic Timer tick to drive
/// the clock + animation just like the qemu_virt path.
pub fn compose_desktop() {
    if !is_fb_ready() { return; }
    let tick = super::timer::tick_count();
    let uptime_seconds = tick / 100;
    let frame = tick / 10;
    let mut s = unsafe { surface() };
    with_wm(|wm| wm.compose_animated(&mut s, uptime_seconds, frame));
}

/// 10 Hz recompose driven by the timer IRQ. Skipped entirely when the
/// FB isn't up (UART-only boot).
pub fn tick_recompose_if_due(tick: u64) {
    if !is_fb_ready() { return; }
    if tick % 10 == 0 {
        with_wm(|wm| { wm.animation_step(); });
        compose_desktop();
    }
}

/// Populate the WindowManager with the same set of demo windows the
/// qemu_virt platform ships — System Info / boot log / Game of Life /
/// Snake / Mandelbrot / Calculator / Notes. Re-runnable; the
/// `MAX_WINDOWS` slab is cleared first.
///
/// Window dimensions are smaller than the qemu_virt layout because
/// the mailbox-allocated FB defaults to 1280x720 (HDMI 720p) rather
/// than 1280x800 (qemu virt). Easy to retune later.
pub fn populate_demo_desktop() {
    if !is_fb_ready() { return; }

    with_wm(|wm| {
        for i in 0..beetos::gui::MAX_WINDOWS {
            wm.remove(beetos::gui::WindowId(i as u8));
        }
        wm.set_desktop_bg(color::DESKTOP_BG);

        // 1280x720 layout — three columns roughly 410 / 360 / 290 wide.

        // Window 1 — system info.
        let mut info = [TextLine::EMPTY; MAX_TEXT_LINES];
        info[0] = TextLine::new("BeetOS v0.1.0");
        info[1] = TextLine::new("Platform: Raspberry Pi 5 (BCM2712)");
        info[2] = TextLine::new("Kernel: Xous (cherry-picked KeyOS)");
        info[3] = TextLine::new("Graphics: beetos::gfx + wgpu_compat");
        info[4] = TextLine::new("FB: VideoCore mailbox-allocated");
        info[5] = TextLine::new("");
        info[6] = TextLine::new("UART: BCM PL011, FDT-discovered");
        info[7] = TextLine::new("GIC:  v3 at 0x107FFF9000");
        info[8] = TextLine::new("Mbox: 0x107C013880 (channel 8)");
        let info_win = Window::new(
            Rect::new(20, 40, 400, 220),
            "System Info",
            WindowKind::Text { lines: info, count: 9 },
        );
        let _ = wm.add(info_win);

        // Window 2 — boot log.
        let mut boot = [TextLine::EMPTY; MAX_TEXT_LINES];
        boot[0] = TextLine::new("[ OK ] BCM2712 platform::init");
        boot[1] = TextLine::new("[ OK ] FDT parsed (PL011 / GIC v3)");
        boot[2] = TextLine::new("[ OK ] GIC v3 initialised");
        boot[3] = TextLine::new("[ OK ] Generic Timer ticking");
        boot[4] = TextLine::new("[ OK ] Mailbox FB allocated");
        boot[5] = TextLine::new("[ OK ] MMU enabled");
        boot[6] = TextLine::new("[ OK ] Shell launched (EL0)");
        boot[7] = TextLine::new("[INFO] Desktop ready.");
        let boot_win = Window::new(
            Rect::new(20, 280, 400, 200),
            "boot log",
            WindowKind::Text { lines: boot, count: 8 },
        );
        let _ = wm.add(boot_win);

        // Window 3 — Game of Life.
        let life = Window::new(
            Rect::new(20, 500, 400, 180),
            "Game of Life",
            WindowKind::Life(LifeState::new()),
        );
        let _ = wm.add(life);

        // Window 4 — Snake (centre-top).
        let snake = Window::new(
            Rect::new(440, 40, 360, 320),
            "Snake",
            WindowKind::Snake(SnakeState::new()),
        );
        let snake_id = wm.add(snake).ok().unwrap_or(beetos::gui::WindowId(0));

        // Window 5 — Mandelbrot (centre-bottom).
        let fractal = Window::new(
            Rect::new(440, 380, 360, 300),
            "Mandelbrot",
            WindowKind::Mandelbrot(MandelState::new()),
        );
        let _ = wm.add(fractal);

        // Window 6 — calculator (right column).
        let calc = Window::new(
            Rect::new(820, 40, 290, 340),
            "Calculator",
            WindowKind::Calc(CalcState::from_preview("1234")),
        );
        let _ = wm.add(calc);

        // Window 7 — notes (right column, bottom).
        let notes = Window::new(
            Rect::new(820, 400, 290, 280),
            "Notes",
            WindowKind::Notes(NotesState::with_text(
                "BeetOS on RPi5 hardware:\n- Mailbox FB working\n- WindowManager live\n- Same widgets as QEMU\n- Tab to cycle, Shift+arrows\n  move cursor.",
            )),
        );
        let _ = wm.add(notes);

        wm.focus(snake_id);
    });

    compose_desktop();
}

/// Provided so the existing `crate::platform::Console::write_str`
/// path (which calls `crate::platform::fb_write`) compiles on bcm2712
/// builds — today it's a no-op (text-on-FB needs an FbConsole
/// equivalent to be set up first; left as future work).
#[allow(dead_code)]
pub fn write_str(_s: &str) {}

// Re-export a small unused helper struct so the `Color` type stays in
// scope even when no Color-using helpers ship — keeps follow-up
// commits diff-clean.
#[allow(dead_code)]
const _COLOR_KEEPALIVE: Color = color::BLACK;
