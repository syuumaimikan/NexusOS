//! Direct access to the firmware-provided linear framebuffer.
//!
//! This is the kernel's only display path until a real GPU driver exists. The
//! Nexus Compositor will eventually own this surface; for now it carries the
//! boot splash and, when things go wrong, the panic screen.
//!
//! The drawing surface exposes a complete primitive set; the boot splash uses
//! only some of it, and the panic screen and compositor use the rest.
#![allow(dead_code)]

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
}

// SAFETY: the framebuffer is plain MMIO with no internal invariants; access is
// serialised by the lock that owns the `Framebuffer`, not by the type itself.
unsafe impl Send for Framebuffer {}

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
                u32::from(color.red()) | u32::from(color.green()) << 8 | u32::from(color.blue()) << 16
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
        // SAFETY: the bounds check above keeps the offset inside the mapped
        // framebuffer, and `bytes_per_pixel` is 4 for every format `new`
        // accepts, so a 32-bit store stays within one pixel.
        unsafe {
            core::ptr::write_volatile(self.base.add(self.offset(x, y)) as *mut u32, encoded);
        }
    }

    /// Fill an axis-aligned rectangle, clipped to the surface.
    pub fn fill_rect(&mut self, x: u32, y: u32, width: u32, height: u32, color: Color) {
        let x_end = (x.saturating_add(width)).min(self.width);
        let y_end = (y.saturating_add(height)).min(self.height);
        if x >= x_end || y >= y_end {
            return;
        }

        let encoded = self.encode(color);
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
