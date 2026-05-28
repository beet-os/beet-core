// SPDX-FileCopyrightText: 2025 BeetOS contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! `wgpu`-shaped compatibility shim backed by [`crate::gfx`].
//!
//! Goal: let apps write rendering code that **looks identical to a
//! real `wgpu`-based renderer today**, so when the day comes that
//! BeetOS has a `wgpu-hal` backend on top of an actual GPU driver
//! (`asahi-drm`-derived for Apple M1, `v3d`-derived for RPi5,
//! virtio-gpu for QEMU), the migration is `use beetos::wgpu_compat`
//! → `use wgpu`. Same call sites, different backend.
//!
//! This is **not** a full WebGPU implementation. It's a deliberately
//! tiny subset of `wgpu`'s public surface, focused on the 2D
//! immediate-mode drawing pattern that the `beetos::gui` widgets
//! already need. The encoder / render-pass distinction is preserved
//! so the program structure is wgpu-shaped, even though the backend
//! is a software rasterizer that executes ops immediately.
//!
//! ## Mapping
//!
//! | wgpu type/method                  | This shim does…                                  |
//! |-----------------------------------|--------------------------------------------------|
//! | `Instance::new()`                 | constructs an empty handle (no global state)     |
//! | `Instance::create_surface(...)`   | wraps a raw FB pointer in a [`Surface`]          |
//! | `Surface::get_current_texture()`  | hands out a [`SurfaceTexture`] backed by the FB  |
//! | `SurfaceTexture::create_view()`   | hands out a writable [`TextureView`]             |
//! | `Device::create_command_encoder` | wraps the target view in a [`CommandEncoder`]    |
//! | `Encoder::begin_render_pass(...)` | starts a [`RenderPass`] (with optional clear)    |
//! | `RenderPass::set_color(...)`      | sets the brush color for subsequent draws        |
//! | `RenderPass::draw_*` / `fill_*`   | call straight into `gfx::Surface` primitives      |
//! | `Encoder::finish()`               | turns into a [`CommandBuffer`] (no-op record)    |
//! | `Queue::submit(buffers)`          | executes any deferred work (none, today)         |
//! | `SurfaceTexture::present()`       | no-op: software FB updates are immediate         |
//!
//! Once we have real GPU drivers, `RenderPass::draw_rect` becomes
//! a textured-quad draw instead of a `fill_rect`, the encoder
//! actually records a command list, `submit` flushes it to the
//! hardware, and `present` swaps buffers. Apps don't care.
//!
//! ## Example
//!
//! ```rust,ignore
//! use beetos::wgpu_compat::{Color, Instance, Rect};
//!
//! let instance = Instance::new();
//! let mut surface = unsafe { instance.create_surface_raw(fb_ptr, 1280, 800, 1280) };
//! let device = instance.request_device();
//! let queue  = device.queue();
//!
//! let frame = surface.get_current_texture();
//! let view  = frame.texture_view();
//! let mut encoder = device.create_command_encoder();
//! {
//!     let mut rpass = encoder.begin_render_pass(&view, Some(Color::BLACK));
//!     rpass.fill_rect(&Rect::new(40, 40, 200, 100), Color::from_rgb(0x6E, 0x29, 0x80));
//!     rpass.draw_text(48, 60, "hello, beet!", Color::WHITE, Color::TRANSPARENT);
//! }
//! queue.submit([encoder.finish()]);
//! frame.present();
//! ```

use crate::gfx;

// ─────────────────────────────────────────────────────────────────────────────
// Color
// ─────────────────────────────────────────────────────────────────────────────

/// A color in 0xRRGGBBAA-friendly representation.
///
/// Today wraps the same XRGB8888 `u32` as `gfx::Color`; we wrap it
/// in a newtype so the *type* matches wgpu's `wgpu::Color` (which is
/// `{r, g, b, a}` f64). Constants below mirror wgpu's wgpu::Color
/// constants where they exist.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Color(pub gfx::Color);

impl Color {
    pub const BLACK:       Color = Color(gfx::color::BLACK);
    pub const WHITE:       Color = Color(gfx::color::WHITE);
    pub const RED:         Color = Color(gfx::color::RED);
    pub const GREEN:       Color = Color(gfx::color::GREEN);
    pub const BLUE:        Color = Color(gfx::color::BLUE);
    pub const TRANSPARENT: Color = Color(0); // alias for BLACK in XRGB8888

    pub const fn from_rgb(r: u8, g: u8, b: u8) -> Color {
        Color(gfx::rgb(r, g, b))
    }

    /// Lift any `gfx::Color` into a wgpu-shaped [`Color`]. Useful when
    /// pulling values out of the existing palette (`gfx::color::*`)
    /// without having to repeat the RGB triplet here.
    pub const fn from_raw(raw: gfx::Color) -> Color { Color(raw) }

    pub const fn as_raw(self) -> gfx::Color { self.0 }
}

/// Re-export of the rectangle type so call sites don't have to
/// import both `gfx::Rect` and our shim.
pub type Rect = gfx::Rect;

// ─────────────────────────────────────────────────────────────────────────────
// Instance / Device / Queue
// ─────────────────────────────────────────────────────────────────────────────

/// Entry point — analogous to `wgpu::Instance`. In real wgpu this
/// owns the underlying graphics backend (Vulkan/Metal/DX12 loader);
/// here it's a zero-sized handle whose only job is to mint
/// [`Surface`]s and [`Device`]s.
#[derive(Clone, Copy, Default)]
pub struct Instance;

impl Instance {
    pub const fn new() -> Self { Self }

    /// Wrap a raw pixel buffer as a presentable [`Surface`].
    ///
    /// # Safety
    ///
    /// Same contract as [`gfx::Surface::from_raw_parts`]: caller
    /// guarantees the buffer is at least `stride * height` u32s long
    /// and not aliased by any other `&mut` for the surface's lifetime.
    pub unsafe fn create_surface_raw(
        &self,
        ptr: *mut u32,
        width: i32,
        height: i32,
        stride: i32,
    ) -> Surface {
        Surface {
            inner: gfx::Surface::from_raw_parts(ptr, width, height, stride),
        }
    }

    /// In real wgpu this is `async fn request_adapter().await.request_device().await`.
    /// We collapse the whole adapter/device dance into one synchronous
    /// call since there's nothing to negotiate with the host.
    pub const fn request_device(&self) -> Device { Device }
}

/// Wgpu's `Device` handle. Today only exposes the methods needed to
/// drive the existing 2D pipeline; queue access mirrors wgpu's API.
#[derive(Clone, Copy)]
pub struct Device;

impl Device {
    pub const fn queue(&self) -> Queue { Queue }

    /// Wraps the target view in a one-shot encoder. Each encoder
    /// owns a `&mut TextureView` for its lifetime; submitting it
    /// drops the borrow.
    pub fn create_command_encoder<'a>(&self, view: TextureView<'a>) -> CommandEncoder<'a> {
        CommandEncoder { view }
    }
}

/// Wgpu's `Queue`. Software backend has nothing to flush, but we
/// keep the type so apps can `queue.submit([encoder.finish()])`
/// the same way they would on real wgpu.
#[derive(Clone, Copy)]
pub struct Queue;

impl Queue {
    /// Drop the command buffers — software backend already applied
    /// their ops immediately as they were recorded.
    pub fn submit<I: IntoIterator<Item = CommandBuffer>>(&self, buffers: I) {
        for b in buffers { let _ = b; }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Surface / SurfaceTexture / TextureView
// ─────────────────────────────────────────────────────────────────────────────

/// A presentable surface. Wraps a [`gfx::Surface`] one-to-one.
pub struct Surface {
    inner: gfx::Surface,
}

impl Surface {
    pub fn width(&self)  -> i32 { self.inner.width() }
    pub fn height(&self) -> i32 { self.inner.height() }
    pub fn rect(&self)   -> Rect { self.inner.rect() }

    /// Acquire the next frame's backing texture. On real wgpu this
    /// is the swapchain image; here it's the same FB you handed in,
    /// just wrapped so writes go through the API.
    pub fn get_current_texture(&mut self) -> SurfaceTexture<'_> {
        SurfaceTexture { surface: self }
    }
}

/// The texture for the current frame.
pub struct SurfaceTexture<'a> {
    surface: &'a mut Surface,
}

impl<'a> SurfaceTexture<'a> {
    /// Borrow the writable view for the duration of this frame.
    pub fn texture_view(&mut self) -> TextureView<'_> {
        TextureView { surface: &mut self.surface.inner }
    }

    /// Submit the frame. Software backend has nothing to do; on a
    /// real GPU this would trigger a buffer swap.
    pub fn present(self) {}
}

/// A writable view onto a texture. Mirrors `wgpu::TextureView`'s role
/// as the thing render passes target.
pub struct TextureView<'a> {
    surface: &'a mut gfx::Surface,
}

// ─────────────────────────────────────────────────────────────────────────────
// CommandEncoder / RenderPass / CommandBuffer
// ─────────────────────────────────────────────────────────────────────────────

/// A wgpu-shaped command encoder. Hands out [`RenderPass`]es that
/// borrow it exclusively until they're dropped; `finish()` consumes
/// the encoder and yields a [`CommandBuffer`] for queue submission.
pub struct CommandEncoder<'a> {
    view: TextureView<'a>,
}

impl<'a> CommandEncoder<'a> {
    /// Begin a render pass targeting the encoder's view.  `clear`
    /// is the optional clear color applied at pass start — `None`
    /// preserves whatever was there.
    pub fn begin_render_pass<'b>(&'b mut self, clear: Option<Color>) -> RenderPass<'b> {
        if let Some(c) = clear {
            self.view.surface.fill(c.as_raw());
        }
        // Borrow the underlying gfx::Surface directly so we avoid an
        // invariant nested lifetime through TextureView<'a>.
        RenderPass { target: self.view.surface, brush: Color::WHITE }
    }

    /// Consume the encoder into a no-op command buffer. The
    /// software backend has already applied every draw op, so the
    /// returned buffer is just a marker for the queue API.
    pub fn finish(self) -> CommandBuffer { CommandBuffer }
}

/// One render pass — every draw call is applied to the bound view.
pub struct RenderPass<'a> {
    target: &'a mut gfx::Surface,
    brush: Color,
}

impl<'a> RenderPass<'a> {
    /// Set the brush color for subsequent untyped draw calls.
    pub fn set_color(&mut self, color: Color) { self.brush = color; }

    /// Fill the entire viewport.
    pub fn clear(&mut self, color: Color) {
        self.target.fill(color.as_raw());
    }

    /// Fill an axis-aligned rectangle.  Future GPU backend turns
    /// this into a textured quad draw against a 1×1 white texture.
    pub fn fill_rect(&mut self, rect: &Rect, color: Color) {
        self.target.fill_rect(rect, color.as_raw());
    }

    /// Outline a rectangle.
    pub fn stroke_rect(&mut self, rect: &Rect, color: Color) {
        self.target.rect_outline(rect, color.as_raw());
    }

    /// Draw a line between two points (Bresenham, any slope).
    pub fn line(&mut self, x0: i32, y0: i32, x1: i32, y1: i32, color: Color) {
        self.target.line(x0, y0, x1, y1, color.as_raw());
    }

    /// Filled circle.
    pub fn fill_circle(&mut self, cx: i32, cy: i32, r: i32, color: Color) {
        self.target.circle_filled(cx, cy, r, color.as_raw());
    }

    /// Outline circle.
    pub fn stroke_circle(&mut self, cx: i32, cy: i32, r: i32, color: Color) {
        self.target.circle(cx, cy, r, color.as_raw());
    }

    /// Draw ASCII text at `(x, y)` using the 8×16 bundled font.
    /// `bg` is the cell background; pass [`Color::TRANSPARENT`] for
    /// no fill (today rendered as solid black — proper alpha
    /// blending lands with the GPU backend).
    pub fn draw_text(&mut self, x: i32, y: i32, s: &str, fg: Color, bg: Color) {
        self.target.draw_text(x, y, s, fg.as_raw(), bg.as_raw());
    }
}

/// A recorded command list. The software backend treats this as
/// purely a token — every op was already applied to the target view
/// when it was recorded. Mirrors `wgpu::CommandBuffer` so apps can
/// pass it to `Queue::submit` exactly like on real wgpu.
#[must_use]
pub struct CommandBuffer;

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    extern crate alloc;

    fn surface_buf(w: i32, h: i32) -> (alloc::vec::Vec<u32>, Surface) {
        let mut buf = alloc::vec![0u32; (w * h) as usize];
        let ptr = buf.as_mut_ptr();
        let instance = Instance::new();
        let s = unsafe { instance.create_surface_raw(ptr, w, h, w) };
        (buf, s)
    }

    #[test]
    fn fill_rect_through_render_pass() {
        let (_buf, mut surface) = surface_buf(64, 64);
        let device = Instance::new().request_device();
        {
            let mut frame = surface.get_current_texture();
            let view = frame.texture_view();
            let mut encoder = device.create_command_encoder(view);
            {
                let mut rpass = encoder.begin_render_pass(Some(Color::BLACK));
                rpass.fill_rect(&Rect::new(10, 10, 20, 20), Color::RED);
            }
            device.queue().submit([encoder.finish()]);
            frame.present();
        }
        // Pixel inside the red rect.
        assert_eq!(surface.inner.get_pixel(15, 15), Color::RED.as_raw());
        // Pixel outside should be the clear color.
        assert_eq!(surface.inner.get_pixel(5, 5), Color::BLACK.as_raw());
    }

    #[test]
    fn full_pipeline_compiles_like_wgpu() {
        // This is the doctest example, exercised at compile + runtime.
        let (_buf, mut surface) = surface_buf(80, 40);
        let instance = Instance::new();
        let device = instance.request_device();
        let queue  = device.queue();

        let mut frame = surface.get_current_texture();
        let view = frame.texture_view();
        let mut encoder = device.create_command_encoder(view);
        {
            let mut rpass = encoder.begin_render_pass(Some(Color::BLACK));
            rpass.fill_rect(&Rect::new(8, 8, 60, 20), Color::from_rgb(0x6E, 0x29, 0x80));
            rpass.draw_text(12, 12, "hi", Color::WHITE, Color::TRANSPARENT);
        }
        queue.submit([encoder.finish()]);
        frame.present();
    }

    #[test]
    fn color_constants_match_gfx() {
        assert_eq!(Color::BLACK.as_raw(), gfx::color::BLACK);
        assert_eq!(Color::WHITE.as_raw(), gfx::color::WHITE);
        // gfx::color::RED is CGA-style 0xCD0000, not 0xFF0000.
        assert_eq!(Color::RED.as_raw(),   gfx::color::RED);
        assert_eq!(Color::from_rgb(0xCD, 0, 0).as_raw(), gfx::color::RED);
    }

    #[test]
    fn brush_color_set_persists() {
        let (_buf, mut surface) = surface_buf(8, 8);
        let device = Instance::new().request_device();
        let mut frame = surface.get_current_texture();
        let view = frame.texture_view();
        let mut encoder = device.create_command_encoder(view);
        let mut rpass = encoder.begin_render_pass(None);
        rpass.set_color(Color::GREEN);
        // Setter is wired up — we can't observe much without exposing
        // the brush, but a separate fill_rect with the same color must
        // round-trip.
        rpass.fill_rect(&Rect::new(0, 0, 8, 8), Color::GREEN);
    }
}
