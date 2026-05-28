# BeetOS GUI stack

BeetOS ships a small, allocation-free GUI stack that lives in the
`beetos` crate and runs unchanged in three places:

  - `cargo test -p beetos --features fb` — full host-side tests
  - inside the kernel image (`#![no_std]` + no allocator)
  - eventually inside a userspace compositor process

Everything below is layered. Each layer only depends on the one
beneath it, so you can pull just the bits you need (just `gfx` for
"draw on a buffer"; `gui` adds windows; calling sites pick whether
to feed it real keystrokes).

```
┌────────────────────────────────────────────────────────────────┐
│  Apps / demo content                                           │
│    Calculator, Notes, gfx demo, About, …                       │
├────────────────────────────────────────────────────────────────┤
│  beetos::gui                                                   │
│    Window / WindowManager / WidgetGrid / Button / Label /      │
│    CalcState / NotesState / handle_key / compose_animated      │
├────────────────────────────────────────────────────────────────┤
│  beetos::gfx                                                   │
│    Surface / Rect / Color / pixel & line & rect & circle &     │
│    blit & draw_char/draw_text / subsurface                     │
├────────────────────────────────────────────────────────────────┤
│  Platform framebuffer                                          │
│    qemu_virt::fb (ramfb)  •  bcm2712::fb (HDMI, TODO)         │
│    apple_t8103::fb (m1n1 SimpleFB, TODO)                       │
└────────────────────────────────────────────────────────────────┘
```

## beetos::gfx — drawing primitives

`Surface` is a non-owning view of XRGB8888 pixel memory:

```rust
let mut s = unsafe { Surface::from_raw_parts(ptr, w, h, stride) };
s.fill(color::DESKTOP_BG);
s.fill_rect(&Rect::new(10, 10, 100, 50), color::BEET_PURPLE);
s.line(0, 0, 100, 100, color::WHITE);
s.circle_filled(60, 60, 20, color::BRIGHT_GREEN);
s.draw_text(8, 12, "hello, beet!", color::WHITE, color::BLACK);
```

Available primitives, all clipped to the surface:

| Function                  | What it does                                |
|---------------------------|---------------------------------------------|
| `set_pixel / get_pixel`   | Single-pixel R/W                             |
| `fill / fill_rect`        | Solid fill, whole surface or a rect         |
| `rect_outline`            | 1 px-wide rectangle border                  |
| `hline / vline`           | Horizontal / vertical run                   |
| `line(x0,y0,x1,y1,c)`     | Bresenham, any slope                        |
| `circle / circle_filled`  | Bresenham midpoint, outline or filled       |
| `blit(dx, dy, &src)`      | Copy another `Surface` into this one        |
| `subsurface(&Rect)`       | Borrow a writable rectangular sub-view      |
| `draw_char / draw_text`   | 8×16 font (font8x8) with fg/bg              |

`Color` is a `u32` packed `0x00_RR_GG_BB`. The `gfx::color` module
provides 16 ANSI-style entries plus a few brand colors
(`BEET_PURPLE`, `BEET_PINK`, `DESKTOP_BG`, `WINDOW_BG`,
`WINDOW_FRAME`, `TITLE_BAR`, `TITLE_FG`).

### Why "wgpu-shaped"?

The naming is on purpose. When BeetOS gains real GPU drivers (M11 in
`plan.md`: AGX on Apple, VideoCore VII on RPi5), the plan is to back
this same `Surface` API with `wgpu` so the widget layer doesn't need
to change. Today the rasterizer is pure CPU and Rust; same call
sites, eventually different backend.

## beetos::gui — window manager + widgets

### Window + WindowManager

`WindowManager` is a fixed-size slab (`[Option<Window>; MAX_WINDOWS=8]`)
plus a monotonic z counter. No allocator needed — `new()` is `const`,
so the kernel keeps one in BSS as a global static and accesses it via
`platform::qemu_virt::fb::with_wm(|wm| …)`.

```rust
let win = Window::new(
    Rect::new(40, 60, 420, 240),
    "System Info",
    WindowKind::Text { lines, count: 9 },
);
let id = wm.add(win)?;
wm.focus(id);
wm.compose(&mut screen);
```

Decorations are honest: focused windows wear the accent color, the
close X sits in a right-aligned square, the content rectangle excludes
both the title bar (`TITLEBAR_H`) and the frame (`FRAME_W`).

### WindowKind

| Variant                       | Content                                    |
|-------------------------------|--------------------------------------------|
| `Empty`                       | Just a colored client area                 |
| `Text { lines, count }`       | Up to `MAX_TEXT_LINES=16` lines × 79 bytes |
| `Demo`                        | Static gfx primitives showcase             |
| `Widgets(WidgetGrid)`         | Static widget tree (Label/Button/Spacer)   |
| `Calc(CalcState)`             | Interactive integer calculator             |
| `Notes(NotesState)`           | 512-byte text pad, line-wrapped            |

All variants are `Copy` and bounded in size so `Window` stays a flat
POD struct that fits in the static slab.

### Widgets

```rust
let mut grid = WidgetGrid::new(4);
grid.push_span(LeafWidget::Label(LabelW::new("0").align_right()), 4);
grid.push(LeafWidget::Button(ButtonW::new("AC", 1).warning()));
grid.push(LeafWidget::Button(ButtonW::new("/", 4).accent()));
// …
```

`WidgetGrid` is a single-depth container (no `Box<dyn Widget>`,
no `Vec`). Cells are placed row-major with `push` (one column) or
`push_span(widget, n)` (claim N columns). The bundled
`calculator_grid(display)` and `about_grid()` are full examples.

`Label` supports left / centre / right alignment and per-widget
fg/bg colors. `Button` has `Normal / Pressed / Disabled` states and
helpers `.accent()` / `.warning()` for the brand palette.

### Input routing

`WindowManager::handle_key(byte) -> bool` consumes one ASCII byte. It
returns `true` when the focused window's state changed, signalling a
recompose. Two reserved global keys:

  - `\t` (Tab) — cycle focus forward through visible windows
  - `0x19` (Shift-Tab) — cycle focus backward

Everything else is delegated to the focused window's state machine
(`CalcState::press` or `NotesState::press` for now). Windows that
ignore the key return `false` so the kernel can fall through to its
existing IPC input path — the shell keeps working when no GUI window
is focused.

In `arch/aarch64/irq.rs`, the virtio-input dispatcher tries the GUI
first:

```rust
fn dispatch_input_char(c: u8) {
    let consumed = qemu_virt::fb::with_wm(|wm| wm.handle_key(c));
    if consumed { qemu_virt::fb::compose_desktop(); return; }
    // else: route to the focused process via deliver_char_to_sid …
}
```

### Animation

`WindowManager::compose_animated(screen, uptime_seconds, frame)`
adds two flourishes on top of the static layout:

  - HH:MM:SS clock pill on the right of the taskbar, driven by
    `uptime_seconds`
  - Spinner glyph (`|`, `/`, `-`, `\`) just left of the clock,
    advanced per `frame`
  - Bouncing accent circle in the desktop background

The kernel calls `fb::tick_recompose_if_due(tick)` from the timer IRQ.
It recomposes when `tick % 10 == 0` — i.e. ~10 Hz at the platform's
100 Hz timer — which is smooth enough for the spinner and well below
the framebuffer's saturation point.

## Adding a new platform

A new platform exports two things:

1. A `framebuffer surface` — pixel memory + width/height/stride. Today
   `qemu_virt::fb::surface()` returns `unsafe { Surface::from_raw_parts(…) }`
   from the `ramfb` device. For BCM2712 you'd return the HDMI surface
   set up by the firmware; for Apple T8103 the m1n1 SimpleFB region
   you found via FDT.

2. A `populate_demo_desktop` (or equivalent) entry point that the
   platform's `init` calls after the FB is up. That's just a few
   `WindowManager::add` calls plus a final `compose_desktop()`.

Everything else (drawing primitives, widget framework, animation
loop) is platform-agnostic — only the seam in `platform::Console`
and the timer IRQ hook need to know about the platform.

## What's next

- Mouse cursor + virtio-mouse driver (Phase 5 stretch — keyboard
  Tab already works for navigation)
- Multi-process GUI: today the WindowManager lives in the kernel.
  Move it to a userspace compositor when std support lands (M7),
  so apps can register their own windows over IPC.
- Real GPU backend behind the `Surface` trait (M11 onwards). The
  widget code wouldn't change; `gfx` primitives become wgpu draw
  calls.
- Font scaling / HiDPI — the bundled font8x8 already supports
  pixel doubling; right now we render 1:1.
