//! Direct access to the firmware-provided linear framebuffer.
//!
//! This is the kernel's only display path until a real GPU driver exists. The
//! Nexus Compositor will eventually own this surface; for now it carries the
//! boot splash and, when things go wrong, the panic screen.
//!
//! The drawing surface exposes a complete primitive set; the boot splash uses
//! only some of it, and the panic screen and compositor use the rest.
#![allow(dead_code)]

pub mod font;

use nexus_abi::{layout, FramebufferInfo, PixelFormat};

/// A 24-bit colour in `0x00RRGGBB` form.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Color(pub u32);

impl Color {
    pub const BLACK: Color = Color(0x0000_0000);
    pub const WHITE: Color = Color(0x00FF_FFFF);
    pub const NEXUS_BLUE: Color = Color(0x0021_8AFF);
    pub const NEXUS_DEEP: Color = Color(0x000B_1220);
    pub const PANIC_RED: Color = Color(0x00B0_1B1B);

    /// Build a colour from separate channels.
    #[must_use]
    pub const fn rgb(red: u8, green: u8, blue: u8) -> Self {
        Self((red as u32) << 16 | (green as u32) << 8 | blue as u32)
    }

    #[must_use]
    pub const fn red(self) -> u8 {
        (self.0 >> 16) as u8
    }

    #[must_use]
    pub const fn green(self) -> u8 {
        (self.0 >> 8) as u8
    }

    #[must_use]
    pub const fn blue(self) -> u8 {
        self.0 as u8
    }

    /// Linearly blend towards `other`; `amount` is 0..=255.
    ///
    /// Blending happens in sRGB space, which is not colourimetrically correct
    /// but is what every other boot-time renderer does and is adequate for
    /// gradients and dimming.
    #[must_use]
    pub const fn blend(self, other: Color, amount: u8) -> Color {
        let inverse = 255 - amount as u32;
        let amount = amount as u32;
        let red = (self.red() as u32 * inverse + other.red() as u32 * amount) / 255;
        let green = (self.green() as u32 * inverse + other.green() as u32 * amount) / 255;
        let blue = (self.blue() as u32 * inverse + other.blue() as u32 * amount) / 255;
        Color(red << 16 | green << 8 | blue)
    }
}

/// A writable linear framebuffer.
pub struct Framebuffer {
    base: *mut u8,
    width: u32,
    height: u32,
    stride: u32,
    bytes_per_pixel: u32,
    format: PixelFormat,
    /// The smallest rectangle covering everything written since it was last
    /// taken, as left, top, right, bottom -- the right and bottom exclusive.
    ///
    /// A bounding box and not a list. Two windows at opposite corners make it
    /// the whole screen, which sends more than changed; a list of rectangles
    /// would send less and is a great deal more arithmetic to get right. The
    /// compositor above this made the same choice for the same reason.
    damage: Option<(u32, u32, u32, u32)>,
}

// SAFETY: the framebuffer is plain MMIO with no internal invariants; access is
// serialised by the lock that owns the `Framebuffer`, not by the type itself.
unsafe impl Send for Framebuffer {}

/// A vertical colour ramp, and the height it is measured against.
///
/// Three values that always travel together, and that are wrong in a way
/// nothing catches when they come apart: the height is the *surface's*, not the
/// region being painted, so that a strip repainted on its own gets the colours
/// a full repaint would have given it. Passed separately, a caller that reached
/// for the region's height instead produced a seam and no error.
#[derive(Debug, Clone, Copy)]
pub struct Gradient {
    /// Colour at the top of the surface.
    pub top: Color,
    /// Colour at the bottom of it.
    pub bottom: Color,
    /// Rows the ramp runs over, which is the whole surface.
    pub surface_height: u32,
}

impl Framebuffer {
    /// Adopt the framebuffer the bootloader described.
    ///
    /// Returns `None` when the firmware provided no usable surface.
    ///
    /// # Safety
    ///
    /// `info` must describe a framebuffer that is mapped writable through the
    /// direct map, and no other `Framebuffer` may exist for the same memory.
    #[must_use]
    pub unsafe fn new(info: &FramebufferInfo) -> Option<Self> {
        if !info.is_valid() || info.format == PixelFormat::Unknown {
            return None;
        }
        Some(Self {
            base: layout::phys_to_virt(info.phys_addr) as *mut u8,
            width: info.width,
            height: info.height,
            stride: info.stride,
            bytes_per_pixel: info.bytes_per_pixel,
            format: info.format,
            // Nothing written yet, which is not the same as an empty rectangle:
            // the first paint has to be sent, and a zero-area rectangle would
            // be skipped.
            damage: None,
        })
    }

    #[must_use]
    pub fn width(&self) -> u32 {
        self.width
    }

    #[must_use]
    pub fn height(&self) -> u32 {
        self.height
    }

    /// Encode `color` in this framebuffer's channel order.
    #[inline]
    fn encode(&self, color: Color) -> u32 {
        match self.format {
            // Byte order R, G, B, X in memory; little-endian words put red low.
            PixelFormat::Rgbx8888 => {
                u32::from(color.red())
                    | u32::from(color.green()) << 8
                    | u32::from(color.blue()) << 16
            }
            // Byte order B, G, R, X, which matches `0x00RRGGBB` directly.
            PixelFormat::Bgrx8888 => color.0,
            // Ruled out by `new`.
            PixelFormat::Unknown => 0,
        }
    }

    /// Byte offset of the pixel at `(x, y)`.
    #[inline]
    fn offset(&self, x: u32, y: u32) -> usize {
        (y as usize * self.stride as usize + x as usize) * self.bytes_per_pixel as usize
    }

    /// Set one pixel. Coordinates outside the surface are ignored.
    #[inline]
    pub fn put_pixel(&mut self, x: u32, y: u32, color: Color) {
        if x >= self.width || y >= self.height {
            return;
        }
        let encoded = self.encode(color);
        self.touched(x, y, x + 1, y + 1);
        // SAFETY: the bounds check above keeps the offset inside the mapped
        // framebuffer, and `bytes_per_pixel` is 4 for every format `new`
        // accepts, so a 32-bit store stays within one pixel.
        unsafe {
            core::ptr::write_volatile(self.base.add(self.offset(x, y)) as *mut u32, encoded);
        }
    }

    /// Note that a rectangle has been written to.
    ///
    /// Called from the two places that write pixels, and only those two:
    /// everything else on this type -- glyphs, text, gradients, `clear` --
    /// reaches the surface through one of them, so the damage is complete
    /// without any of them having to remember to say so. That is the reason it
    /// lives here rather than at the call sites.
    fn touched(&mut self, left: u32, top: u32, right: u32, bottom: u32) {
        self.damage = Some(match self.damage {
            None => (left, top, right, bottom),
            Some((l, t, r, b)) => (l.min(left), t.min(top), r.max(right), b.max(bottom)),
        });
    }

    /// What has been written to since this was last called, and forget it.
    ///
    /// Taken rather than read, so that a caller which sends the rectangle on
    /// cannot send the same pixels twice, and a caller which drops it has
    /// visibly dropped it.
    pub fn take_damage(&mut self) -> Option<(u32, u32, u32, u32)> {
        self.damage.take()
    }

    /// Fill an axis-aligned rectangle, clipped to the surface.
    pub fn fill_rect(&mut self, x: u32, y: u32, width: u32, height: u32, color: Color) {
        let x_end = (x.saturating_add(width)).min(self.width);
        let y_end = (y.saturating_add(height)).min(self.height);
        if x >= x_end || y >= y_end {
            return;
        }

        let encoded = self.encode(color);
        self.touched(x, y, x_end, y_end);
        for row in y..y_end {
            // Compute the row base once rather than per pixel; a full-screen
            // clear does millions of these.
            let row_base = self.offset(0, row);
            for column in x..x_end {
                let offset = row_base + column as usize * self.bytes_per_pixel as usize;
                // SAFETY: `x_end`/`y_end` are clamped to the surface, so every
                // offset lies within the mapped framebuffer.
                unsafe {
                    core::ptr::write_volatile(self.base.add(offset) as *mut u32, encoded);
                }
            }
        }
    }

    /// Fill the whole surface with one colour.
    pub fn clear(&mut self, color: Color) {
        self.fill_rect(0, 0, self.width, self.height, color);
    }

    /// Draw one glyph at `(x, y)`, magnified `scale` times.
    ///
    /// Only set bits are painted, so glyphs compose over whatever is already
    /// there instead of stamping a background box over it. Returns the advance
    /// in pixels, which varies: Latin is half-width and CJK full-width.
    pub fn draw_glyph(&mut self, x: u32, y: u32, character: char, color: Color, scale: u32) -> u32 {
        let glyph = font::glyph(character);
        let scale = scale.max(1);

        for row in 0..font::CELL_HEIGHT {
            for column in 0..glyph.advance {
                // Thresholded rather than blended. The kernel draws on the
                // panel and the boot logo, where reading what it says matters
                // more than how the edges look -- and blending would mean
                // reading the framebuffer back, which is a great deal slower
                // over an uncached mapping than writing to it.
                if glyph.alpha(column, row) < 128 {
                    continue;
                }
                let pixel_x = x + column * scale;
                let pixel_y = y + row * scale;
                if scale == 1 {
                    self.put_pixel(pixel_x, pixel_y, color);
                } else {
                    self.fill_rect(pixel_x, pixel_y, scale, scale, color);
                }
            }
        }

        glyph.advance * scale
    }

    /// Draw `text` starting at `(x, y)`, returning the x coordinate just past
    /// the last glyph.
    ///
    /// Iterates by `char`, so UTF-8 is handled by construction: a Japanese
    /// label and an English one are drawn by exactly the same code.
    ///
    /// There is no wrapping. A caller that cares about the surface edge should
    /// measure with [`Framebuffer::text_width`] first; wrapping is a layout
    /// decision, and this is a drawing primitive.
    pub fn draw_text(&mut self, x: u32, y: u32, text: &str, color: Color, scale: u32) -> u32 {
        let mut cursor = x;
        for character in text.chars() {
            cursor += self.draw_glyph(cursor, y, character, color, scale);
        }
        cursor
    }

    /// Width in pixels that `text` would occupy at `scale`.
    ///
    /// Must be used rather than counting characters: a string of eight kanji is
    /// twice as wide as eight Latin letters, and a layout that assumes
    /// otherwise draws outside whatever region it cleared.
    #[must_use]
    pub fn text_width(text: &str, scale: u32) -> u32 {
        font::measure(text) * scale.max(1)
    }

    /// Height in pixels of one line at `scale`.
    #[must_use]
    pub fn line_height(scale: u32) -> u32 {
        font::CELL_HEIGHT * scale.max(1)
    }

    /// Draw `text` horizontally centred on the surface at `y`.
    pub fn draw_text_centered(&mut self, y: u32, text: &str, color: Color, scale: u32) {
        let width = Self::text_width(text, scale);
        let x = self.width.saturating_sub(width) / 2;
        self.draw_text(x, y, text, color, scale);
    }

    /// Paint a vertical gradient over rows `[start_y, end_y)`.
    ///
    /// The gradient is computed against the *whole surface* height, not the
    /// region, so repainting part of the screen produces exactly the colours
    /// that a full repaint would have put there. A region-relative gradient
    /// would leave a visible seam at the boundary.
    pub fn vertical_gradient_region(&mut self, start_y: u32, end_y: u32, gradient: Gradient) {
        self.vertical_gradient_span(0, self.width, start_y, end_y, gradient);
    }

    /// The same, across part of the width rather than all of it.
    ///
    /// What makes it possible to repaint the background *around* something. The
    /// kernel gives a rectangle of the screen to a process, and a clear that
    /// went from edge to edge would take it back twice a second.
    pub fn vertical_gradient_span(
        &mut self,
        start_x: u32,
        end_x: u32,
        start_y: u32,
        end_y: u32,
        gradient: Gradient,
    ) {
        if end_x <= start_x {
            return;
        }
        let height = gradient.surface_height.max(1);
        let end_y = end_y.min(self.height);
        let end_x = end_x.min(self.width);
        for row in start_y..end_y {
            // The shade depends on the row's place on the *surface*, not in the
            // span, so a strip repainted on its own matches what is beside it.
            let amount = (row as u64 * 255 / height as u64).min(255) as u8;
            let color = gradient.top.blend(gradient.bottom, amount);
            self.fill_rect(start_x, row, end_x - start_x, 1, color);
        }
    }

    /// Paint a vertical gradient from `top` to `bottom` across the surface.
    ///
    /// Used for the boot background, which is the first visual proof that the
    /// kernel is running and that the framebuffer handoff is correct.
    pub fn vertical_gradient(&mut self, top: Color, bottom: Color) {
        let height = self.height.max(1);
        for row in 0..self.height {
            // `row * 255 / (height - 1)` would divide by zero on a 1px surface.
            let amount = (row as u64 * 255 / height as u64) as u8;
            let color = top.blend(bottom, amount);
            self.fill_rect(0, row, self.width, 1, color);
        }
    }
}
