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

use beetos::gfx::{color, Color, Rect, Surface};
use beetos::gui::{
    CalcState, LifeState, MandelState, NotesState, SnakeState, TextLine, Window, WindowKind,
    WindowManager, MAX_TEXT_LINES,
};
use beetos::{phys_to_virt, virt_to_phys};

use crate::fb_console::FbConsole;

// ─────────────────────────────────────────────────────────────────────────────
// Framebuffer layout
// ─────────────────────────────────────────────────────────────────────────────

/// Physical address of the framebuffer (top 4 MB of QEMU's 1 GB RAM window).
/// Must match the reservation subtracted from `ram_size` in boot.rs.
pub const FB_PHYS: usize = 0x7FC0_0000;

/// Framebuffer width in pixels — single source of truth lives in the `beetos` crate.
pub use beetos::FB_WIDTH;
/// Framebuffer height in pixels — single source of truth lives in the `beetos` crate.
pub use beetos::FB_HEIGHT;

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

/// Set to `true` once `init()` finds a working ramfb device. The GUI
/// compose / timer-driven recompose paths read this so terminal-only
/// boots (no `-device ramfb`) don't waste CPU painting into an
/// unmapped framebuffer region.
static mut FB_READY: bool = false;

#[inline]
pub fn is_fb_ready() -> bool { unsafe { FB_READY } }

/// Global window manager.
///
/// `WindowManager::new()` is `const`, so we can hold one as a static and
/// keep the no_alloc invariant.  Access goes through `with_wm` to keep
/// `unsafe { static mut … }` in one place and to surface the
/// "single-threaded during boot, needs a lock for SMP" assumption.
static mut WINDOW_MANAGER: WindowManager = WindowManager::new();

/// Run a closure with mutable access to the global window manager.
///
/// # Safety
///
/// Currently safe in the single-CPU early-boot / panic / IRQ path. The
/// day BeetOS goes SMP this needs a real lock — flagged here so a
/// future audit catches it.
pub fn with_wm<R>(f: impl FnOnce(&mut WindowManager) -> R) -> R {
    unsafe { f(&mut *core::ptr::addr_of_mut!(WINDOW_MANAGER)) }
}

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
    // RamFbCfg is `#[repr(C, packed)]` — its fields are NOT guaranteed
    // to be naturally aligned, so we must use `write_unaligned` rather
    // than `write_volatile` (which UB-checks alignment). The values
    // still cross to QEMU through a regular Normal-memory write
    // followed by the DSB ISH in fwcfg_dma_submit.
    let cfg = addr_of_mut!(RAMFB_CFG);
    core::ptr::write_unaligned(addr_of_mut!((*cfg).addr),   (FB_PHYS as u64).to_be());
    core::ptr::write_unaligned(addr_of_mut!((*cfg).fourcc), DRM_FORMAT_XRGB8888.to_be());
    core::ptr::write_unaligned(addr_of_mut!((*cfg).flags),  0u32.to_be());
    core::ptr::write_unaligned(addr_of_mut!((*cfg).width),  (FB_WIDTH as u32).to_be());
    core::ptr::write_unaligned(addr_of_mut!((*cfg).height), (FB_HEIGHT as u32).to_be());
    core::ptr::write_unaligned(addr_of_mut!((*cfg).stride), (FB_STRIDE_BYTES as u32).to_be());

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
    FB_READY = true;

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

/// Borrow the live framebuffer as a [`Surface`].
///
/// # Safety
///
/// Caller must guarantee no other code writes to the FB concurrently —
/// today the kernel is effectively single-CPU for early init / panic
/// paths so that holds, but this needs revisiting before SMP.
pub unsafe fn surface() -> Surface {
    let base = phys_to_virt(FB_PHYS) as *mut u32;
    Surface::from_raw_parts(base, FB_WIDTH as i32, FB_HEIGHT as i32, FB_WIDTH as i32)
}

/// Paint the BeetOS boot screen — solid background, banner, version,
/// and a row of phase indicator boxes that subsequent init steps can
/// turn from "pending" (frame-only) into "done" (filled) via
/// [`mark_phase_complete`].
///
/// Designed to be called once, right after [`init`] returns true.
pub fn draw_boot_screen() {
    // Run the boot screen through the wgpu_compat shim end-to-end.
    // Same pixel result as a direct gfx::Surface walk — the point is
    // that this is exactly the program structure a real wgpu renderer
    // would have, so when we get a real GPU backend (M11), nothing
    // here needs to change beyond a single `use` line.
    use beetos::wgpu_compat::{Color, Instance};

    // SAFETY: called only from the single-threaded boot path, after `init`
    // mapped the FB. No other writer can race here.
    let instance = Instance::new();
    let mut surface = unsafe {
        instance.create_surface_raw(
            beetos::phys_to_virt(FB_PHYS) as *mut u32,
            FB_WIDTH as i32, FB_HEIGHT as i32, FB_WIDTH as i32,
        )
    };
    let device = instance.request_device();
    let queue = device.queue();

    let mut frame = surface.get_current_texture();
    let view = frame.texture_view();
    let mut encoder = device.create_command_encoder(view);
    {
        let mut rpass = encoder.begin_render_pass(Some(Color::from_raw(color::DESKTOP_BG)));

        // Top accent bar.
        rpass.fill_rect(
            &Rect::new(0, 0, FB_WIDTH as i32, 4),
            Color::from_raw(color::BEET_PURPLE),
        );

        // BeetOS title — drawn twice for fake-bold.
        let title_x = 80;
        let title_y = 80;
        let white  = Color::from_raw(color::WHITE);
        let lgray  = Color::from_raw(color::LIGHT_GRAY);
        let dgray  = Color::from_raw(color::DARK_GRAY);
        let bg     = Color::from_raw(color::DESKTOP_BG);
        rpass.draw_text(title_x,     title_y, "BeetOS", white, bg);
        rpass.draw_text(title_x + 1, title_y, "BeetOS", white, bg);
        rpass.draw_text(title_x, title_y + 24,
            "v0.1.0 - booting on QEMU virt (AArch64)", lgray, bg);

        // Decorative beet shape.
        let beet_cx = FB_WIDTH as i32 - 140;
        let beet_cy = 130;
        rpass.fill_circle(beet_cx, beet_cy, 48, Color::from_raw(color::BEET_PINK));
        rpass.fill_circle(beet_cx, beet_cy, 36, Color::from_raw(color::BEET_PURPLE));
        rpass.line(beet_cx, beet_cy - 48, beet_cx + 18, beet_cy - 80,
            Color::from_raw(color::BRIGHT_GREEN));
        rpass.line(beet_cx, beet_cy - 48, beet_cx + 38, beet_cy - 72,
            Color::from_raw(color::GREEN));

        // Subtitle / hint.
        rpass.draw_text(title_x, title_y + 72,
            "  Kernel: Xous (cherry-picked from KeyOS) + AArch64 port", lgray, bg);
        rpass.draw_text(title_x, title_y + 96,
            "  Graphics: beetos::gfx + wgpu_compat shim", dgray, bg);

        // Phase indicators along the bottom.
        let labels = ["UART", "GIC", "Timer", "FB", "MMU", "ELF", "Shell"];
        let box_w = 80;
        let box_h = 24;
        let gap = 12;
        let total_w = labels.len() as i32 * box_w + (labels.len() as i32 - 1) * gap;
        let start_x = (FB_WIDTH as i32 - total_w) / 2;
        let row_y = FB_HEIGHT as i32 - 80;
        let frame_c = Color::from_raw(color::WINDOW_FRAME);
        for (i, label) in labels.iter().enumerate() {
            let x = start_x + i as i32 * (box_w + gap);
            let r = Rect::new(x, row_y, box_w, box_h);
            rpass.stroke_rect(&r, frame_c);
            let tx = x + (box_w - (label.len() as i32 * 8)) / 2;
            let ty = row_y + (box_h - 16) / 2;
            rpass.draw_text(tx, ty, label, lgray, bg);
        }

        // Bottom accent bar.
        rpass.fill_rect(
            &Rect::new(0, FB_HEIGHT as i32 - 4, FB_WIDTH as i32, 4),
            Color::from_raw(color::BEET_PURPLE),
        );
    }
    queue.submit([encoder.finish()]);
    frame.present();

    // Reset the FbConsole cursor so any subsequent text appears below the
    // banner, not on top of it. We keep the existing FbConsole intact
    // (it's still the path serial->fb mirroring uses) — just nudge its
    // cursor past the boot graphics.
    unsafe {
        if let Some(ref mut con) = FB_CONSOLE {
            // Below the title block, above the phase row.
            con.set_cursor(13, 0);
        }
    }
}

/// Highlight one of the boot-phase indicator boxes drawn by
/// [`draw_boot_screen`]. Boxes are 0-indexed in the order they were laid out
/// (UART=0, GIC=1, Timer=2, FB=3, MMU=4, ELF=5, Shell=6).
pub fn mark_phase_complete(phase_idx: usize, label: &str) {
    let labels_count: i32 = 7;
    let box_w = 80;
    let box_h = 24;
    let gap = 12;
    let total_w = labels_count * box_w + (labels_count - 1) * gap;
    let start_x = (FB_WIDTH as i32 - total_w) / 2;
    let row_y = FB_HEIGHT as i32 - 80;
    let x = start_x + phase_idx as i32 * (box_w + gap);
    let r = Rect::new(x, row_y, box_w, box_h);
    let mut s = unsafe { surface() };
    s.fill_rect(&r, color::BEET_PURPLE);
    let tx = x + (box_w - (label.len() as i32 * 8)) / 2;
    let ty = row_y + (box_h - 16) / 2;
    s.draw_text(tx, ty, label, color::WHITE, color::BEET_PURPLE);
}

/// Populate the global [`WindowManager`] with a demo desktop showcasing
/// the GUI stack — title bars, decorations, taskbar, a few overlapping
/// windows with text and a Demo window that exercises every gfx
/// primitive.  Idempotent: re-calling repopulates the same set so this
/// can be triggered from a shell command later without leaking.
pub fn populate_demo_desktop() {
    // Stop the FbConsole from re-emitting text over the desktop — clamp
    // its cursor into a small region the windows leave free, or just
    // halt scrolling. For the demo we drop the existing console entirely
    // so the screenshot is clean.
    unsafe {
        // SAFETY: only the boot/idle paths touch FB_CONSOLE today.
        let con = &raw mut FB_CONSOLE;
        *con = None;
    }

    with_wm(|wm| {
        // Start fresh so re-runs don't pile up.
        for i in 0..beetos::gui::MAX_WINDOWS {
            wm.remove(beetos::gui::WindowId(i as u8));
        }
        wm.set_desktop_bg(color::DESKTOP_BG);

        // Window 1 — system info (top-left).
        let mut info_lines = [TextLine::EMPTY; MAX_TEXT_LINES];
        info_lines[0] = TextLine::new("BeetOS v0.1.0");
        info_lines[1] = TextLine::new("Platform: QEMU virt (AArch64)");
        info_lines[2] = TextLine::new("Kernel: Xous (cherry-picked KeyOS)");
        info_lines[3] = TextLine::new("Graphics: beetos::gfx software raster");
        info_lines[4] = TextLine::new("Windows: beetos::gui no_alloc MAX=8");
        info_lines[5] = TextLine::new("");
        info_lines[6] = TextLine::new("MMIO addresses: from FDT");
        info_lines[7] = TextLine::new("UART: PL011 at 0x09000000");
        info_lines[8] = TextLine::new("GIC:  v3 at 0x08000000");
        info_lines[9] = TextLine::new("FB:   ramfb 1280x800 XRGB8888");
        let info = Window::new(
            Rect::new(40, 60, 420, 240),
            "System Info",
            WindowKind::Text { lines: info_lines, count: 10 },
        );
        let _ = wm.add(info);

        // Window 2 — boot log (mid-left).
        let mut boot_lines = [TextLine::EMPTY; MAX_TEXT_LINES];
        boot_lines[0] = TextLine::new("[ OK ] platform::init");
        boot_lines[1] = TextLine::new("[ OK ] FDT parsed (UART/GIC from DTB)");
        boot_lines[2] = TextLine::new("[ OK ] GIC v3 initialised");
        boot_lines[3] = TextLine::new("[ OK ] Generic Timer ticking");
        boot_lines[4] = TextLine::new("[ OK ] ramfb 1280x800 mapped");
        boot_lines[5] = TextLine::new("[ OK ] MMU enabled, MemoryManager up");
        boot_lines[6] = TextLine::new("[ OK ] log/procman/fs/shell launched");
        boot_lines[7] = TextLine::new("[ OK ] First preemption switch");
        boot_lines[8] = TextLine::new("[INFO] Desktop ready.");
        let boot = Window::new(
            Rect::new(40, 320, 420, 200),
            "boot log",
            WindowKind::Text { lines: boot_lines, count: 9 },
        );
        let _ = wm.add(boot);

        // Window 3 — Conway's Game of Life (bottom-left).
        let life = Window::new(
            Rect::new(40, 540, 420, 180),
            "Game of Life",
            WindowKind::Life(LifeState::new()),
        );
        let _ = wm.add(life);

        // Window 4 — Snake game (centre-top, the wow-factor demo).
        // Board is 20x15 cells at 16 px each, so we size the content
        // to roughly 340x300 to leave room for score + hint lines.
        let snake = Window::new(
            Rect::new(490, 60, 360, 320),
            "Snake",
            WindowKind::Snake(SnakeState::new()),
        );
        let snake_id = wm.add(snake).ok().unwrap_or(beetos::gui::WindowId(0));

        // Window 5 — Mandelbrot fractal (centre-bottom). Pure CPU
        // rasterizer eating its own dogfood — zooms toward Seahorse
        // Valley once a frame and resets when too tight.
        let fractal = Window::new(
            Rect::new(490, 395, 360, 220),
            "Mandelbrot",
            WindowKind::Mandelbrot(MandelState::new()),
        );
        let _ = wm.add(fractal);

        // Window 5 — interactive calculator (right side, top half).
        // Pre-seed the display with "1234" so it looks alive before
        // the first keystroke arrives over virtio-keyboard.
        let calc = Window::new(
            Rect::new(940, 60, 290, 350),
            "Calculator",
            WindowKind::Calc(CalcState::from_preview("1234")),
        );
        let calc_id = wm.add(calc).ok().unwrap_or(beetos::gui::WindowId(0));

        // Window 6 — interactive notepad below the calculator.
        let notes = Window::new(
            Rect::new(940, 430, 290, 220),
            "Notes",
            WindowKind::Notes(NotesState::with_text(
                "BeetOS notes:\n- Try Tab to switch\n  windows.\n- Calc keys: 0-9 + - * / =\n- This pad accepts text.",
            )),
        );
        let _ = wm.add(notes);

        // Focus the snake so the first arrow keystrokes drive the
        // game — Tab cycles to calc/notes if the user wants those.
        let _ = calc_id;
        wm.focus(snake_id);
    });

    compose_desktop();
}

/// Re-render the live [`WindowManager`] onto the framebuffer.
///
/// Reads the current timer tick count to drive both the taskbar clock
/// and the desktop's animated elements (spinner, bouncing accent).
pub fn compose_desktop() {
    if !is_fb_ready() { return; }
    let tick = super::timer::tick_count();
    // 100 timer ticks per second — see TICK_RATE_HZ in timer.rs.
    let uptime_seconds = tick / 100;
    // Animation frame counter — runs at the recompose rate (~10 Hz).
    let frame = tick / 10;
    let mut s = unsafe { surface() };
    with_wm(|wm| wm.compose_animated(&mut s, uptime_seconds, frame));
}

/// Trigger a periodic recompose from the timer IRQ — drives the
/// taskbar clock, spinner, and background animation without needing
/// explicit input.  Called by `handle_irq` ~10 times a second.
// ─────────────────────────────────────────────────────────────────────────────
// Deferred composition (IRQs request, idle paints)
// ─────────────────────────────────────────────────────────────────────────────
//
// A full software compose of the 1280×800 desktop is far too slow to
// run with IRQs masked (~9 ms release, ~90 ms in a debug build — at
// 10 Hz that masked the CPU for most of its life and lost UART input
// beyond the 16-byte FIFO). IRQ handlers therefore only *request* a
// repaint — cheap flag writes — and the actual painting happens in the
// kernel idle loop with IRQs unmasked, guarded so input arriving
// mid-paint can't mutate the WindowManager while compose walks it.

/// A repaint is wanted (timer cadence, key, or pointer activity).
static mut COMPOSE_REQUESTED: bool = false;
/// A 10 Hz animation boundary passed; step animations on next paint.
static mut ANIM_STEP_DUE: bool = false;
/// The idle-context compose is currently walking the WindowManager.
static mut COMPOSE_IN_PROGRESS: bool = false;

/// Keys that arrived while a compose was walking the WM, drained
/// through the normal routing right after the paint. 16 is plenty:
/// one paint lasts well under two keyboard auto-repeats.
static mut PENDING_KEYS: [u8; 16] = [0; 16];
static mut PENDING_HEAD: usize = 0;
static mut PENDING_TAIL: usize = 0;

/// Ask the idle loop for a repaint. Safe from any IRQ handler.
#[inline]
pub fn request_compose() {
    unsafe { COMPOSE_REQUESTED = true }
}

/// True while the idle-context compose walks the WindowManager —
/// IRQ handlers must not mutate the WM (or repaint) while this holds.
#[inline]
pub fn compose_in_progress() -> bool {
    unsafe { COMPOSE_IN_PROGRESS }
}

/// Park a key that can't take the WM path right now (paint in
/// progress). Returns `false` when the queue is full — the caller
/// drops the key, same outcome as a UART FIFO overrun.
pub fn queue_key_during_compose(c: u8) -> bool {
    unsafe {
        let next = (PENDING_HEAD + 1) % PENDING_KEYS.len();
        if next == PENDING_TAIL {
            return false;
        }
        PENDING_KEYS[PENDING_HEAD] = c;
        PENDING_HEAD = next;
        true
    }
}

fn pop_pending_key() -> Option<u8> {
    unsafe {
        if PENDING_HEAD == PENDING_TAIL {
            return None;
        }
        let c = PENDING_KEYS[PENDING_TAIL];
        PENDING_TAIL = (PENDING_TAIL + 1) % PENDING_KEYS.len();
        Some(c)
    }
}

/// 10 Hz pacing from the timer IRQ: request a paint, never do one.
pub fn tick_recompose_if_due(tick: u64) {
    // Terminal-only boots (no `-device ramfb`) skip the entire GUI
    // pipeline so the timer IRQ stays cheap and the shell isn't
    // competing with a wasted rasterizer.
    if !is_fb_ready() { return; }
    // 10 Hz: smooth enough for the spinner and bouncing accent, well
    // below the 100 Hz timer so we don't melt the (software!) rasterizer.
    if tick % 10 == 0 {
        unsafe {
            ANIM_STEP_DUE = true;
            COMPOSE_REQUESTED = true;
        }
    }
}

/// Idle-loop entry point: paint if a repaint was requested.
///
/// Must be called with IRQs masked. Unmasks them around the heavy
/// compose so input/net/timer stay live during the paint, and
/// re-masks before returning. Returns `true` if it painted — the
/// caller should re-run the scheduler, since an IRQ during the paint
/// may have readied a thread.
pub fn idle_compose_if_requested() -> bool {
    unsafe {
        if !FB_READY || !COMPOSE_REQUESTED {
            return false;
        }
        COMPOSE_REQUESTED = false;
        let step = ANIM_STEP_DUE;
        ANIM_STEP_DUE = false;
        COMPOSE_IN_PROGRESS = true;

        // NB: no `nomem` on these asm blocks — they double as compiler
        // barriers so the flag writes above/below can't drift across
        // the mask boundary.
        core::arch::asm!("msr daifclr, #2", options(nostack));
        if step {
            with_wm(|wm| { wm.animation_step(); });
        }
        compose_desktop();
        core::arch::asm!("msr daifset, #2", options(nostack));

        COMPOSE_IN_PROGRESS = false;
    }

    // Route keys that were parked mid-paint. IRQs are masked again, so
    // this runs in the same conditions as normal IRQ-context delivery
    // (WM first now that the walk is over, IPC fallback otherwise).
    while let Some(c) = pop_pending_key() {
        crate::arch::irq::dispatch_input_char_public(c);
    }
    true
}

/// Convenience: write a colored status line below the boot banner.
/// Intended for future per-driver status updates from outside the
/// platform module; kept ungated so we don't have to thread through
/// extra plumbing the first time it's needed.
#[allow(dead_code)]
pub fn boot_status_line(line: i32, msg: &str, fg: Color) {
    let mut s = unsafe { surface() };
    let y = 220 + line * 18;
    // Clear the line area first so re-writes don't overlap.
    let clear = Rect::new(80, y, FB_WIDTH as i32 - 160, 16);
    s.fill_rect(&clear, color::BLACK);
    s.draw_text(80, y, msg, fg, color::BLACK);
}
