# wgpu on BeetOS — roadmap

This document tracks the path from today's software rasterizer to
real GPU-accelerated rendering through the wgpu API, on every BeetOS
target. The end-state is: any app written against `wgpu`'s API runs
unchanged on QEMU virt, Raspberry Pi 5 (VideoCore VII), and Apple M1
(AGX), because every platform exposes a `wgpu-hal` backend that
speaks directly to its native GPU command stream — no Vulkan layer
in between.

## Why wgpu, why bypass Vulkan

wgpu is the Rust port of the WebGPU specification. It gives apps a
single API surface — `Surface`, `Texture`, `RenderPipeline`,
`CommandEncoder`, `RenderPass`, `BindGroup`, `naga`-compiled shaders
in WGSL — that today maps onto Vulkan / Metal / D3D12 / WebGL2 via
the `wgpu-hal` backend split.

For BeetOS the value is two-fold:

1. **Portable apps**: write a renderer against wgpu, deploy on any
   target that exposes a backend. Same store of binaries works on M1
   laptops, RPi5 boards, future hardware.

2. **No layer tax**: shipping Vulkan on BeetOS would mean carrying a
   Vulkan loader, validation layers, the whole MoltenVK-equivalent
   per platform. By writing a `wgpu-hal` backend straight onto our
   own GPU drivers (`asahi-drm`-derived for AGX,
   `v3d`/`vc4`-derived for VC7), we skip a layer of indirection and
   ~500k LoC of imported C.

## Today (M0 → M6 — what's already in the tree)

Two layers, stacked:

- `beetos::gfx` — software 2D rasterizer with a `Surface` type that
  mirrors wgpu's `Surface` conceptually (writable pixel buffer with
  width/height/stride, plus draw primitives). Every widget in
  `beetos::gui` calls into `Surface` only. There is no direct
  framebuffer pointer in widget code, on purpose.
- `beetos::wgpu_compat` — a no_std shim that exposes a tiny subset of
  wgpu's API (`Instance`, `Surface`, `SurfaceTexture`, `TextureView`,
  `Device`, `Queue`, `CommandEncoder`, `RenderPass`, `CommandBuffer`,
  `Color`) backed by `gfx`. **Apps can write rendering code that looks
  identical to a real wgpu renderer today.** The boot screen in
  `qemu_virt::fb::draw_boot_screen` is the first production caller —
  it follows the exact `Instance → create_surface → encoder →
  begin_render_pass → fill_rect/draw_text → submit → present` shape
  a real wgpu app uses.

Migration when real wgpu lands is `use beetos::wgpu_compat` →
`use wgpu`. Same call sites. The widget framework doesn't even know.

```rust
//   today                         post-M11
//   ─────                         ────────
//   surface.fill(...)             wgpu_render_pass.draw(...)
//   surface.blit(x,y,&texture)    wgpu_render_pass.set_bind_group(...)
//   surface.draw_text(...)        wgpu glyph atlas + sampler
```

When the backend swap happens, widget code does NOT change. Only the
`Surface` impl is replaced.

## Step 1 — `std` support (M7)

wgpu the crate depends on `std`: `Arc`, `Mutex`, `parking_lot`,
threading primitives, `tokio`/async-std-style async, file I/O for
shader hot reload, and a real allocator. BeetOS today is
`no_std + alloc`.

What's needed:
- A custom Rust toolchain that compiles `std` for the target
  `aarch64-unknown-beetos`. Stub `std::fs`, `std::net`, `std::thread`
  on top of BeetOS syscalls. CLAUDE.md M7 already plans this.
- A `#[global_allocator]` backed by the BeetOS heap.
- Real thread primitives (we have processes via IPC; can wrap a
  Thread API on top).

Cost: weeks of compiler / runtime engineering.

## Step 2 — GPU driver, per platform (M11a)

This is the hard part. Each GPU needs a Rust kernel driver that:
- Maps the device's MMIO + DMA + interrupt
- Manages command queues, GPU memory, fences
- Talks the native command stream the silicon expects

### Apple M1 (AGX)

Lowest-friction starting point because Asahi's `asahi-drm` driver
is already written in Rust and upstream in mainline Linux. Lina
documented the ISA + firmware protocol. Porting to BeetOS:
- Cherry-pick `asahi-drm` from Linux mainline
- Replace Linux DRM glue with BeetOS-native syscalls / process IPC
- Map AIC IRQs through our `arch/aarch64/irq.rs`
- Map AGX DART (IOMMU) memory through our `MemoryManager`

Estimate: 6 months for a port that can render a triangle. 12 months
for general-purpose rendering.

### Raspberry Pi 5 (VideoCore VII / v3d)

Harder because the Mesa `v3d` driver is C (~30k LoC) and the kernel
side is the Linux `v3d` driver (~5k LoC of C + a userspace
compiler). We'd:
- Port the v3d ioctl protocol surface from Linux to BeetOS IPC
- Reimplement the kernel `v3d` driver in Rust
- Adopt or port the `v3d_compiler` (NIR → VC7 ISA)
- Plumb through PCIe + RP1 (RPi5 puts GPU on the PCIe-attached RP1)

Estimate: 12 months for a triangle, 24 months for general rendering.

### QEMU virt (virtio-gpu)

Easiest. virtio-gpu is a paravirtualized GPU that proxies commands
to the host's actual GPU. The Linux `virtio_gpu` driver is small
(~5k LoC). Porting to BeetOS would give us hardware-accelerated
rendering inside QEMU and a place to test our wgpu-hal backend
plumbing before the real GPU drivers ship.

Estimate: 2-3 months.

## Step 3 — `wgpu-hal` backend on top of each driver (M11b)

`wgpu-hal` is a thin trait surface that backends implement: command
allocation, render pass begin/end, draw, set_pipeline, etc. We'd
write three new backends:

- `wgpu-hal-asahi` → talks to our `asahi-drm`-derived driver
- `wgpu-hal-v3d`   → talks to our v3d driver
- `wgpu-hal-virtio`→ talks to virtio-gpu

The "bypass Vulkan" point: NONE of these go through a Vulkan loader.
They translate wgpu's command stream directly into the device's
native command format.

For the shader path, we'd need `naga` backends that emit the right
ISA:
- AGX assembly (Asahi has docs)
- VC7 binary (Mesa's `nir_to_vir` is the reference, would need a
  Rust port)
- virtio-gpu accepts SPIR-V — easy, `naga` already does this

Estimate: 2-3 months per platform once the underlying driver works.

## Step 4 — Migrate widgets to wgpu (M12)

Once wgpu runs on every target, swap `beetos::gfx::Surface` for a
thin wrapper around `wgpu::Surface`. Widget code shouldn't notice:
- `Surface::fill_rect(rect, color)` → quad with solid color shader
- `Surface::draw_text` → glyph atlas + textured quad batch
- `Surface::blit` → `copy_texture_to_texture`

Performance flips from CPU-bound (today) to GPU-bound (M12), which
is the point. Animation can move from "10 Hz CPU recompose" to
"vsync at the display refresh rate".

## Summary table

| Layer                | Today          | M7        | M11           | M12          |
|----------------------|----------------|-----------|---------------|--------------|
| App                  | Widget tree    | unchanged | unchanged     | unchanged    |
| Widget framework     | beetos::gui    | unchanged | unchanged     | unchanged    |
| Drawing primitive    | beetos::gfx    | unchanged | unchanged     | wgpu wrap    |
| Rasterizer           | CPU            | CPU       | CPU           | GPU          |
| API surface          | gfx::Surface   | same      | same          | wgpu::Surface|
| Backend              | software       | software  | wgpu-hal-xxx  | wgpu-hal-xxx |
| Underlying driver    | none           | none      | asahi/v3d/vio | same         |

## What this commit night delivered

The foundation: every widget in `beetos::gui` draws through a
`Surface` abstraction. When the wgpu work happens, only the
`Surface` impl swaps — calculator, notes, window manager, text
rendering all keep working. The hardest part of any porting effort
(the call sites) is already aligned.
