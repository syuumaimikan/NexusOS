//! NexusUI: what a program draws with.
//!
//! A program is given a surface — memory of a size it was told — and nothing
//! else. Everything above that is this crate: putting a pixel somewhere, then a
//! rectangle, then a glyph, then a line of text, then a stack of things laid out
//! one under another. None of it is in the kernel and none of it is in the
//! compositor, because none of it is either of their business: what a button
//! looks like is not a fact about the machine.
//!
//! # Where it sits
//!
//! Below a toolkit and above a framebuffer. It knows about pixels, colours,
//! glyphs and rectangles; it does not know about windows, input, focus or
//! events. A program using this draws a frame and hands it over — what happens
//! to the frame is the compositor's, and what the program does between frames is
//! the program's.
//!
//! # The font
//!
//! [`nexus_font`], the same face the kernel draws its panel with. A system with
//! two faces is a system where the same string is two widths depending on who
//! drew it, and the one that would go wrong is the one nobody is looking at.
//!
//! It covers the characters the interface's own translations use, which is why
//! Japanese renders here without this crate knowing anything about Japanese:
//! full-width glyphs advance sixteen pixels instead of eight, and that is the
//! whole of what [`Canvas`] has to understand about scripts.
//!
//! # What it is not
//!
//! No damage tracking, no retained scene, no compositing within a surface. A
//! frame is drawn from nothing every time, which at a few thousand pixels is
//! cheaper than remembering what changed — and remembering is the optimisation
//! to make when there is something to measure, not before.

#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]

extern crate alloc;

pub mod glass;
pub mod raster3d;

use alloc::vec::Vec;

pub use nexus_font as font;

/// A colour, as the framebuffer wants it: eight bits each of blue, green and
/// red in the low three bytes.
///
/// A type rather than a bare `u32` so that a colour cannot be passed where a
/// coordinate goes, which is a mistake that draws something and therefore does
/// not look like a mistake.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Colour(pub u32);

impl Colour {
    /// From the three parts, in the order people say them.
    #[must_use]
    pub const fn rgb(red: u8, green: u8, blue: u8) -> Self {
        Self(((red as u32) << 16) | ((green as u32) << 8) | blue as u32)
    }

    /// Mix towards `other`, with `amount` of 255 being all of it.
    ///
    /// Per channel and in eight bits, because the surfaces here are eight bits
    /// per channel and doing it in more would be arithmetic thrown away at the
    /// last step.
    #[must_use]
    pub const fn blend(self, other: Self, amount: u8) -> Self {
        let amount = amount as u32;
        let rest = 255 - amount;
        let red = (((self.0 >> 16) & 0xFF) * rest + ((other.0 >> 16) & 0xFF) * amount) / 255;
        let green = (((self.0 >> 8) & 0xFF) * rest + ((other.0 >> 8) & 0xFF) * amount) / 255;
        let blue = ((self.0 & 0xFF) * rest + (other.0 & 0xFF) * amount) / 255;
        Self((red << 16) | (green << 8) | blue)
    }
}

/// A rectangle, in the surface's own coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

impl Rect {
    #[must_use]
    pub const fn new(x: u32, y: u32, width: u32, height: u32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    /// The same rectangle with `by` taken off every edge.
    ///
    /// Saturating, so insetting more than a rectangle has leaves an empty one
    /// rather than a very large one — which is what unsigned subtraction does
    /// if it is allowed to, and it draws over the whole surface.
    #[must_use]
    pub const fn inset(self, by: u32) -> Self {
        Self {
            x: self.x + by,
            y: self.y + by,
            width: self.width.saturating_sub(by * 2),
            height: self.height.saturating_sub(by * 2),
        }
    }

    /// Whether a point is inside.
    #[must_use]
    pub const fn contains(self, x: u32, y: u32) -> bool {
        x >= self.x && x < self.x + self.width && y >= self.y && y < self.y + self.height
    }
}

/// Somewhere to draw.
///
/// Holds the address a surface is mapped at and its shape, and nothing else. It
/// does not own the memory and does not know where it came from: a canvas over
/// a compositor surface and a canvas over anything else are the same thing.
pub struct Canvas {
    base: usize,
    width: u32,
    height: u32,
    /// Which face text is drawn in, and whether its edges are softened.
    ///
    /// On the canvas rather than passed to every call, because it is a property
    /// of the surface being drawn on and not of one string: a window with two
    /// labels in different faces would be a window nobody asked for.
    face: font::Face,
    antialias: bool,
    layer: bool,
    /// Pixels per row. Equal to the width for a packed surface, and not equal
    /// for a framebuffer whose scanlines the hardware padded.
    stride: u32,
}

impl Canvas {
    /// A canvas over `width` by `height` pixels at `base`, packed.
    ///
    /// # Safety
    ///
    /// `base` must be mapped and writable for `width * height * 4` bytes, and
    /// nothing else may be writing to it while this is drawn on.
    #[must_use]
    pub const unsafe fn packed(base: usize, width: u32, height: u32) -> Self {
        Self {
            base,
            width,
            height,
            stride: width,
            face: font::Face::Smooth,
            antialias: true,
            layer: false,
        }
    }

    /// A canvas whose rows are `stride` pixels apart.
    ///
    /// # Safety
    ///
    /// As [`packed`](Self::packed), for `stride * height * 4` bytes.
    #[must_use]
    pub const unsafe fn strided(base: usize, width: u32, height: u32, stride: u32) -> Self {
        Self {
            base,
            width,
            height,
            stride,
            face: font::Face::Smooth,
            antialias: true,
            layer: false,
        }
    }

    /// Choose the face and whether its edges are softened.
    ///
    /// Both together, because they are not independent: the crisp face is
    /// stored as full coverage everywhere it has ink, so blending it changes
    /// nothing, and asking for the smooth face without blending gives a
    /// thresholded version of a face designed to be blended, which looks worse
    /// than either. A program passes what the settings file says and lets the
    /// two travel together.
    pub const fn set_text_style(&mut self, face: font::Face, antialias: bool) {
        self.face = face;
        self.antialias = antialias;
    }

    /// Draw a transparent overlay. Zero clears it; RGB colours are opaque and
    /// explicit ARGB colours retain their coverage, including rounded/text edges.
    /// Other canvases retain the ordinary opaque framebuffer pixel format.
    pub const fn set_layer(&mut self, enabled: bool) {
        self.layer = enabled;
    }

    /// What the text style is now.
    #[must_use]
    pub const fn text_style(&self) -> (font::Face, bool) {
        (self.face, self.antialias)
    }

    #[must_use]
    pub const fn width(&self) -> u32 {
        self.width
    }

    #[must_use]
    pub const fn height(&self) -> u32 {
        self.height
    }

    /// The whole of it.
    #[must_use]
    pub const fn bounds(&self) -> Rect {
        Rect::new(0, 0, self.width, self.height)
    }

    /// Put one pixel down, if it is on the canvas.
    ///
    /// Clipped rather than checked by the caller. Every drawing routine below
    /// goes through here, so clipping once is clipping everywhere — and the
    /// alternative is every routine getting its own bounds arithmetic right,
    /// which is the same code written five times and wrong in one of them.
    pub fn set(&mut self, x: u32, y: u32, colour: Colour) {
        if x >= self.width || y >= self.height {
            return;
        }
        let offset = (y as usize * self.stride as usize + x as usize) * 4;
        // SAFETY: the constructor's contract says this range is mapped and
        // writable, and the bounds check above keeps the offset inside it.
        unsafe {
            let pixel = if self.layer && colour.0 != 0 && colour.0 >> 24 == 0 {
                colour.0 | 0xff00_0000
            } else {
                colour.0
            };
            core::ptr::write_volatile((self.base + offset) as *mut u32, pixel);
        }
    }

    /// Read a pixel back, if it is on the canvas.
    ///
    /// Needed because blending is a read: putting half a colour somewhere means
    /// knowing what is already there. Nothing else in this crate reads the
    /// surface, and it is worth saying why that is safe -- the canvas owns the
    /// surface for as long as it exists, and the compositor is not reading it,
    /// which is what the `damaged`/`shown` handshake establishes.
    #[must_use]
    pub fn get(&self, x: u32, y: u32) -> Option<Colour> {
        if x >= self.width || y >= self.height {
            return None;
        }
        let offset = (y as usize * self.stride as usize + x as usize) * 4;
        // SAFETY: as `set`, and reading rather than writing.
        let value = unsafe { core::ptr::read_volatile((self.base + offset) as *const u32) };
        Some(Colour(value))
    }

    /// Put part of a colour down, mixed with whatever is already there.
    ///
    /// `alpha` is how much of the new colour to use, 0 to 255. Fully opaque
    /// skips the read, because the common case should not pay for the rare one.
    pub fn blend(&mut self, x: u32, y: u32, colour: Colour, alpha: u8) {
        if self.layer {
            if let Some(under) = self.get(x, y) {
                self.set(x, y, Colour(glass::over(under.0, colour.0, alpha)));
            }
            return;
        }
        match alpha {
            0 => {}
            255 => self.set(x, y, colour),
            alpha => {
                let Some(under) = self.get(x, y) else {
                    return;
                };
                self.set(x, y, under.blend(colour, alpha));
            }
        }
    }

    /// Fill a rectangle with rounded corners.
    ///
    /// The corners are anti-aliased whatever the text setting says, and it is
    /// worth saying why the two are not the same switch. Text is drawn from a
    /// face that is *designed* hard-edged, so softening it is a matter of taste
    /// and belongs to whoever is reading it. A quarter-circle is not designed
    /// at all: at this size an un-softened one is four or five visible steps,
    /// which is not a style, it is a staircase.
    ///
    /// Coverage is counted rather than computed: each corner pixel is sampled
    /// on a four-by-four grid and the fraction inside the curve is its alpha.
    /// Sixteen comparisons a pixel, on about `radius * radius` pixels a corner,
    /// which is nothing next to filling the rectangle itself -- and it needs no
    /// square root, which this system has no floating point for.
    pub fn fill_rounded(&mut self, rect: Rect, radius: u32, colour: Colour) {
        let radius = radius.min(rect.width / 2).min(rect.height / 2);
        if radius == 0 {
            self.paint_rect(rect, colour);
            return;
        }

        // The middle band and the two side bands are square, so they are filled
        // outright; only the four corner squares need sampling.
        self.paint_rect(
            Rect::new(
                rect.x + radius,
                rect.y,
                rect.width - radius * 2,
                rect.height,
            ),
            colour,
        );
        self.paint_rect(
            Rect::new(rect.x, rect.y + radius, radius, rect.height - radius * 2),
            colour,
        );
        self.paint_rect(
            Rect::new(
                rect.x + rect.width - radius,
                rect.y + radius,
                radius,
                rect.height - radius * 2,
            ),
            colour,
        );

        for (corner_x, corner_y, towards_x, towards_y) in [
            (rect.x, rect.y, 1i32, 1i32),
            (rect.x + rect.width - radius, rect.y, -1, 1),
            (rect.x, rect.y + rect.height - radius, 1, -1),
            (
                rect.x + rect.width - radius,
                rect.y + rect.height - radius,
                -1,
                -1,
            ),
        ] {
            for down in 0..radius {
                for across in 0..radius {
                    // Distance from the centre of the curve, in the corner's
                    // own orientation.
                    let (from_x, from_y) = if towards_x > 0 {
                        (radius - across, radius - down)
                    } else {
                        (across + 1, radius - down)
                    };
                    let (from_x, from_y) = if towards_y > 0 {
                        (from_x, from_y)
                    } else {
                        (from_x, down + 1)
                    };
                    let alpha = corner_coverage(from_x, from_y, radius);
                    self.blend(corner_x + across, corner_y + down, colour, alpha);
                }
            }
        }
    }

    /// Compose the fully covered bands just like the sampled rounded edges.
    fn paint_rect(&mut self, rect: Rect, colour: Colour) {
        if !self.layer {
            self.fill(rect, colour);
            return;
        }
        for row in 0..rect.height {
            for column in 0..rect.width {
                self.blend(rect.x + column, rect.y + row, colour, 255);
            }
        }
    }

    /// A rounded panel with a one-pixel edge: a button, a tab, a card.
    ///
    /// Two rounded fills rather than a fill and an outline, because a square
    /// outline drawn around a rounded fill puts the corners back -- which is
    /// what the first version of this did, and it looked like a rounded button
    /// that somebody had drawn a box around.
    pub fn panel(&mut self, rect: Rect, radius: u32, fill: Colour, edge: Colour) {
        self.fill_rounded(rect, radius, edge);
        self.fill_rounded(rect.inset(1), radius.saturating_sub(1), fill);
    }

    /// Fill a rectangle.
    pub fn fill(&mut self, rect: Rect, colour: Colour) {
        for row in 0..rect.height {
            for column in 0..rect.width {
                self.set(rect.x + column, rect.y + row, colour);
            }
        }
    }

    /// Draw the outline of a rectangle, `thickness` pixels wide, inside it.
    pub fn outline(&mut self, rect: Rect, thickness: u32, colour: Colour) {
        for row in 0..rect.height {
            let edge = row < thickness || row + thickness >= rect.height;
            for column in 0..rect.width {
                if edge || column < thickness || column + thickness >= rect.width {
                    self.set(rect.x + column, rect.y + row, colour);
                }
            }
        }
    }

    /// Fill a rectangle with a vertical ramp between two colours.
    pub fn gradient(&mut self, rect: Rect, top: Colour, bottom: Colour) {
        let height = rect.height.max(1);
        for row in 0..rect.height {
            let amount = (row * 255 / height).min(255) as u8;
            let colour = top.blend(bottom, amount);
            for column in 0..rect.width {
                self.set(rect.x + column, rect.y + row, colour);
            }
        }
    }

    /// Draw one glyph with its top-left corner at `x`, `y`, and say how far to
    /// advance.
    ///
    /// The advance comes back rather than being computed by the caller, because
    /// it depends on the glyph: a full-width character is sixteen pixels and a
    /// half-width one is eight, and a caller that assumed either would lay out
    /// one of the two scripts wrongly.
    pub fn glyph(&mut self, x: u32, y: u32, character: char, colour: Colour) -> u32 {
        let glyph = font::glyph_of(self.face, character);
        for row in 0..font::CELL_HEIGHT {
            for column in 0..glyph.advance {
                let alpha = glyph.alpha(column, row);
                if self.antialias {
                    self.blend(x + column, y + row, colour, alpha);
                } else if alpha >= 128 {
                    // Thresholded rather than blended. Half coverage is the
                    // fairest place to put the edge: below it the pixel is
                    // mostly background and above it mostly ink.
                    self.set(x + column, y + row, colour);
                }
            }
        }
        glyph.advance
    }

    /// Draw a line of text, and say how wide it turned out.
    pub fn text(&mut self, x: u32, y: u32, text: &str, colour: Colour) -> u32 {
        let mut advance = 0;
        for character in text.chars() {
            advance += self.glyph(x + advance, y, character, colour);
        }
        advance
    }

    /// Draw a line of text centred in a rectangle.
    pub fn text_centred(&mut self, rect: Rect, text: &str, colour: Colour) {
        let width = font::measure(text);
        let x = rect.x + rect.width.saturating_sub(width) / 2;
        let y = rect.y + rect.height.saturating_sub(font::CELL_HEIGHT) / 2;
        self.text(x, y, text, colour);
    }

    /// Draw a glyph with every pixel drawn as a `scale` by `scale` block.
    ///
    /// Smooth text resamples coverage at output pixel centres. With smoothing
    /// disabled, pixel faces retain their exact nearest-neighbour blocks.
    ///
    /// There is one face and one size in this system, which was fine while
    /// everything it drew was a label in a small window. A screen two thousand
    /// pixels across needs a heading somebody can read from a normal distance,
    /// and the honest way to get one out of a sixteen-pixel face is to make
    /// each pixel bigger.
    pub fn glyph_scaled(
        &mut self,
        x: u32,
        y: u32,
        character: char,
        colour: Colour,
        scale: u32,
    ) -> u32 {
        let scale = scale.max(1);
        let glyph = font::glyph_of(self.face, character);
        // Filter the coverage mask at output pixel centres. Repeating one
        // alpha value over a whole block made enlarged smooth text stair-step.
        if self.antialias && scale > 1 {
            let width = glyph.advance.saturating_mul(scale);
            let height = font::CELL_HEIGHT.saturating_mul(scale);
            let ink = |column: i64, row: i64| -> u32 {
                if column < 0
                    || column >= glyph.advance as i64
                    || row < 0
                    || row >= font::CELL_HEIGHT as i64
                {
                    0
                } else {
                    glyph.alpha(column as u32, row as u32) as u32
                }
            };
            for down in 0..height.min(self.height.saturating_sub(y)) {
                let sy = ((down as u64 * 256 + 128) / scale as u64) as i64 - 128;
                let row = sy.div_euclid(256);
                let fy = sy.rem_euclid(256) as u32;
                for across in 0..width.min(self.width.saturating_sub(x)) {
                    let sx = ((across as u64 * 256 + 128) / scale as u64) as i64 - 128;
                    let column = sx.div_euclid(256);
                    let fx = sx.rem_euclid(256) as u32;
                    let top = ink(column, row) * (256 - fx) + ink(column + 1, row) * fx;
                    let bottom = ink(column, row + 1) * (256 - fx) + ink(column + 1, row + 1) * fx;
                    let alpha = ((top * (256 - fy) + bottom * fy + 32768) / 65536) as u8;
                    if alpha != 0 {
                        self.blend(x + across, y + down, colour, alpha);
                    }
                }
            }
            return width;
        }
        for row in 0..font::CELL_HEIGHT {
            for column in 0..glyph.advance {
                let alpha = glyph.alpha(column, row);
                if alpha == 0 {
                    continue;
                }
                if !self.antialias && alpha < 128 {
                    continue;
                }
                let left = x + column * scale;
                let top = y + row * scale;
                for down in 0..scale {
                    for across in 0..scale {
                        if self.antialias {
                            self.blend(left + across, top + down, colour, alpha);
                        } else {
                            self.set(left + across, top + down, colour);
                        }
                    }
                }
            }
        }
        glyph.advance * scale
    }

    /// Draw a line of text at a scale, and say how wide it turned out.
    pub fn text_scaled(&mut self, x: u32, y: u32, text: &str, colour: Colour, scale: u32) -> u32 {
        let mut advance = 0;
        for character in text.chars() {
            advance += self.glyph_scaled(x + advance, y, character, colour, scale);
        }
        advance
    }

    /// Draw a line of text centred in a rectangle, at a scale.
    pub fn text_centred_scaled(&mut self, rect: Rect, text: &str, colour: Colour, scale: u32) {
        let scale = scale.max(1);
        let width = font::measure(text) * scale;
        let x = rect.x + rect.width.saturating_sub(width) / 2;
        let y = rect.y + rect.height.saturating_sub(font::CELL_HEIGHT * scale) / 2;
        self.text_scaled(x, y, text, colour, scale);
    }
}

/// How wide a string will be, without drawing it.
#[must_use]
pub fn measure(text: &str) -> u32 {
    font::measure(text)
}

/// And at a scale.
#[must_use]
pub fn measure_scaled(text: &str, scale: u32) -> u32 {
    font::measure(text) * scale.max(1)
}

/// How tall one line is.
pub const LINE_HEIGHT: u32 = font::CELL_HEIGHT;

/// The strip along the top of a window that the compositor draws over.
///
/// A window's surface is the whole of its rectangle, and the compositor copies
/// all of it -- then paints the title bar on top, because a bar a client could
/// draw is a bar a client could leave out. So the top of every client's surface
/// is written twice and the client's version loses.
///
/// A program that wants its first line to be visible starts below this.
///
/// The compositor has its own copy, named `TITLE`, and the two must agree.
/// They are not shared because the compositor would then depend on this crate,
/// which depends on a font -- and the compositor draws no text on purpose. The
/// cost of them drifting apart is fourteen pixels of overlap, which is the
/// trade being made knowingly rather than by accident.
pub const TITLE_BAR: u32 = 32;

/// Sample a resized packed image at output pixel centres. The callback keeps
/// shared/volatile memory access with its owner; no aliased slice is created.
pub fn resample_pixel(
    sw: u32,
    sh: u32,
    dw: u32,
    dh: u32,
    x: u32,
    y: u32,
    mut read: impl FnMut(u32, u32) -> u32,
) -> u32 {
    if [sw, sh, dw, dh]
        .iter()
        .any(|&size| size == 0 || size > 16384)
        || x >= dw
        || y >= dh
    {
        return 0;
    }
    let sx = ((x as u64 * 2 + 1) * sw as u64 * 128 / dw as u64) as i64 - 128;
    let sy = ((y as u64 * 2 + 1) * sh as u64 * 128 / dh as u64) as i64 - 128;
    let column = sx.div_euclid(256);
    let row = sy.div_euclid(256);
    let fx = sx.rem_euclid(256) as u32;
    let fy = sy.rem_euclid(256) as u32;
    let x0 = column.clamp(0, sw as i64 - 1) as u32;
    let x1 = (column + 1).clamp(0, sw as i64 - 1) as u32;
    let y0 = row.clamp(0, sh as i64 - 1) as u32;
    let y1 = (row + 1).clamp(0, sh as i64 - 1) as u32;
    let samples = [read(x0, y0), read(x1, y0), read(x0, y1), read(x1, y1)];
    let mut pixel = 0;
    for shift in [0, 8, 16, 24] {
        let top = ((samples[0] >> shift) & 255) * (256 - fx) + ((samples[1] >> shift) & 255) * fx;
        let bottom =
            ((samples[2] >> shift) & 255) * (256 - fx) + ((samples[3] >> shift) & 255) * fx;
        pixel |= ((top * (256 - fy) + bottom * fy + 32768) / 65536) << shift;
    }
    pixel
}

/// A stack of things laid out one under another.
///
/// The whole of the layout this has. It is not a constraint solver and does not
/// want to be one: a column of rows covers a status panel, a list and a menu,
/// which is everything anything here has needed, and a layout engine that had
/// no users would be a layout engine nobody had checked.
pub struct Column {
    area: Rect,
    /// Where the next row goes.
    cursor: u32,
    /// Pixels between rows.
    spacing: u32,
}

impl Column {
    #[must_use]
    pub const fn new(area: Rect, spacing: u32) -> Self {
        Self {
            area,
            cursor: area.y,
            spacing,
        }
    }

    /// Take `height` pixels off the top, and say where they are.
    ///
    /// Returns an empty rectangle when the column is full, rather than one that
    /// hangs off the bottom: everything drawn goes through [`Canvas::set`],
    /// which clips, so an overflowing row would silently draw a piece of itself
    /// and look like a rendering bug rather than a layout one.
    pub fn row(&mut self, height: u32) -> Rect {
        if self.cursor + height > self.area.y + self.area.height {
            return Rect::new(self.area.x, self.area.y + self.area.height, 0, 0);
        }
        let rect = Rect::new(self.area.x, self.cursor, self.area.width, height);
        self.cursor += height + self.spacing;
        rect
    }

    /// A row one line of text tall.
    pub fn line(&mut self) -> Rect {
        self.row(LINE_HEIGHT)
    }

    /// How much room is left.
    #[must_use]
    pub fn remaining(&self) -> u32 {
        (self.area.y + self.area.height).saturating_sub(self.cursor)
    }
}

/// How much of one corner pixel falls inside the curve, 0 to 255.
///
/// `x` and `y` are the pixel's far corner measured from the centre of the
/// quarter-circle, so the pixel covers `(x-1, y-1)` to `(x, y)`.
fn corner_coverage(x: u32, y: u32, radius: u32) -> u8 {
    // Wholly outside the bounding box of the curve: nothing to sample.
    if x > radius || y > radius {
        return 0;
    }
    const SAMPLES: u32 = 8;
    let scale = SAMPLES * 2;
    let limit = (radius as u64 * scale as u64).pow(2);
    let mut inside = 0;
    for down in 0..SAMPLES {
        for across in 0..SAMPLES {
            // Sixty-four sample centres, represented in sixteenth-pixel units.
            let sample_x = (x - 1) * scale + across * 2 + 1;
            let sample_y = (y - 1) * scale + down * 2 + 1;
            if sample_x as u64 * sample_x as u64 + sample_y as u64 * sample_y as u64 <= limit {
                inside += 1;
            }
        }
    }
    (inside * 255 / (SAMPLES * SAMPLES)) as u8
}

/// Sixty-four-sample coverage for a rounded rectangle, including off-shape pixels.
/// Shared by client panels and compositor decorations for matching edges.
#[must_use]
pub fn rounded_coverage(x: u32, y: u32, width: u32, height: u32, radius: u32) -> u8 {
    if x >= width || y >= height {
        return 0;
    }
    let radius = radius.min(width / 2).min(height / 2).min(4096);
    if radius == 0 || (x >= radius && x < width - radius) || (y >= radius && y < height - radius) {
        return 255;
    }
    let dx = if x < radius {
        radius - x
    } else {
        x - (width - radius) + 1
    };
    let dy = if y < radius {
        radius - y
    } else {
        y - (height - radius) + 1
    };
    corner_coverage(dx, dy, radius)
}

/// Break text into lines that fit a width.
///
/// At spaces where there is one and mid-word where there is not, because a word
/// longer than the line has to go somewhere and dropping it would be worse than
/// splitting it. Returns borrowed slices: the caller already owns the text, and
/// copying it to describe where the breaks are would be copying it to say
/// nothing.
#[must_use]
pub fn wrap(text: &str, width: u32) -> Vec<&str> {
    let mut lines = Vec::new();
    // Byte indices, and every one of them a character boundary. That is not
    // pedantry: a break taken at an arbitrary byte panics the moment the text
    // is Japanese, because slicing into the middle of a multi-byte character is
    // not something Rust will do quietly.
    let mut start = 0usize;
    let mut used = 0u32;
    let mut last_space: Option<usize> = None;

    for (index, character) in text.char_indices() {
        if character == '\n' {
            lines.push(&text[start..index]);
            start = index + 1;
            used = 0;
            last_space = None;
            continue;
        }

        let advance = font::glyph(character).advance;
        if used + advance > width && index > start {
            match last_space {
                // At a space where there is one: the space is eaten, because a
                // line that begins with one is a line that looks indented.
                Some(at) if at > start => {
                    lines.push(&text[start..at]);
                    start = at + 1;
                }
                // And mid-character-boundary where there is not. A word longer
                // than the line has to go somewhere, and dropping it would be
                // worse than splitting it.
                _ => {
                    lines.push(&text[start..index]);
                    start = index;
                }
            }
            used = measure(&text[start..index]) + advance;
            last_space = None;
        } else {
            used += advance;
        }

        if character == ' ' {
            last_space = Some(index);
        }
    }

    if start < text.len() || lines.is_empty() {
        lines.push(&text[start..]);
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layered_rounded_panels_compose_bands_and_edges_consistently() {
        let mut pixels = [0xcaf6f4ffu32; 32 * 32];
        let mut canvas = unsafe { Canvas::packed(pixels.as_mut_ptr() as usize, 32, 32) };
        canvas.set_layer(true);
        canvas.fill_rounded(Rect::new(0, 0, 32, 32), 8, Colour(0x55ffffff));
        assert_eq!(
            pixels[16 * 32 + 16],
            glass::over(0xcaf6f4ff, 0x55ffffff, 255)
        );
        assert_eq!(pixels[7 * 32 + 7], pixels[16 * 32 + 16]);
        assert_eq!(pixels[0], 0xcaf6f4ff);
        assert!(pixels[3 * 32 + 3] >> 24 >= 0xca);
    }

    /// The corner is a quarter-circle, so the pixel at the very tip of the
    /// curve is mostly outside it and the one against the straight edge is
    /// wholly inside. Getting this backwards draws a notch instead of a corner,
    /// which is the mistake this test exists to catch.
    #[test]
    fn corner_coverage_runs_from_none_to_all() {
        const RADIUS: u32 = 8;
        // Far corner, diagonally outside the curve.
        assert_eq!(corner_coverage(RADIUS, RADIUS, RADIUS), 0);
        // Hard against the middle of the shape: wholly inside.
        assert_eq!(corner_coverage(1, 1, RADIUS), 255);
        // On the curve itself, somewhere in between.
        let edge = corner_coverage(RADIUS, 4, RADIUS);
        assert!(edge > 0 && edge < 255, "edge coverage was {edge}");
    }

    #[test]
    fn coverage_never_increases_going_outwards() {
        const RADIUS: u32 = 12;
        for y in 1..=RADIUS {
            let mut last = 255;
            for x in 1..=RADIUS {
                let here = corner_coverage(x, y, RADIUS);
                assert!(
                    here <= last,
                    "coverage rose from {last} to {here} at ({x}, {y})"
                );
                last = here;
            }
        }
    }

    #[test]
    fn rounded_edges_are_symmetric_and_have_partial_coverage() {
        let mut partial = 0;
        for y in 0..24 {
            for x in 0..40 {
                let coverage = rounded_coverage(x, y, 40, 24, 10);
                assert_eq!(coverage, rounded_coverage(39 - x, y, 40, 24, 10));
                assert_eq!(coverage, rounded_coverage(x, 23 - y, 40, 24, 10));
                if coverage > 0 && coverage < 255 {
                    partial += 1;
                }
            }
        }
        assert!(partial > 0);
        assert_eq!(rounded_coverage(20, 12, 40, 24, 10), 255);
        assert_eq!(rounded_coverage(40, 12, 40, 24, 10), 0);
        assert_eq!(rounded_coverage(0, 0, 0, 0, 10), 0);
    }

    #[test]
    fn a_pixel_outside_the_radius_is_never_covered() {
        assert_eq!(corner_coverage(9, 1, 8), 0);
        assert_eq!(corner_coverage(1, 9, 8), 0);
    }

    #[test]
    fn large_corner_math_and_scaled_glyphs_preserve_bounds() {
        assert_eq!(corner_coverage(1, 1, 4096), 255);
        assert_eq!(corner_coverage(4096, 4096, 4096), 0);
        let mut pixels = [0x12345678u32; 32 * 52 + 2];
        let end = pixels.len() - 1;
        pixels[1..end].fill(0);
        let mut canvas = unsafe { Canvas::packed(pixels.as_mut_ptr().add(1) as usize, 32, 52) };
        canvas.set_text_style(font::Face::Smooth, true);
        canvas.glyph_scaled(0, 0, 'A', Colour(0xffffff), 3);
        assert!(
            pixels[1..end]
                .iter()
                .any(|&pixel| pixel > 0 && pixel < 0xffffff)
        );
        assert_eq!(pixels[0], 0x12345678);
        assert_eq!(pixels[end], 0x12345678);
        canvas.glyph_scaled(31, 51, 'A', Colour(0xffffff), 3);
        assert_eq!(pixels[0], 0x12345678);
        assert_eq!(pixels[end], 0x12345678);
    }

    #[test]
    fn image_resampling_preserves_centres_edges_and_refuses_bad_shapes() {
        let pixels = [0xff000000, 0xffff0000, 0xff00ff00, 0xff0000ff];
        for y in 0..2 {
            for x in 0..2 {
                assert_eq!(
                    resample_pixel(2, 2, 2, 2, x, y, |sx, sy| pixels[(sy * 2 + sx) as usize]),
                    pixels[(y * 2 + x) as usize]
                );
            }
        }
        assert_eq!(
            resample_pixel(2, 2, 1, 1, 0, 0, |sx, sy| pixels[(sy * 2 + sx) as usize]),
            0xff404040
        );
        for y in 0..19 {
            for x in 0..11 {
                assert_eq!(
                    resample_pixel(4, 7, 11, 19, x, y, |sx, sy| {
                        assert!(sx < 4 && sy < 7);
                        0xa5137bef
                    }),
                    0xa5137bef
                );
            }
        }
        assert_eq!(
            resample_pixel(0, 2, 2, 2, 0, 0, |_, _| panic!("invalid source read")),
            0
        );
        assert_eq!(
            resample_pixel(2, 2, 2, 2, 2, 0, |_, _| panic!("invalid destination read")),
            0
        );
    }

    /// A quarter-circle of radius r has area pi*r*r/4. Summing the coverage of
    /// every pixel in the corner square should come to about that, which is the
    /// one check that the sampling is measuring the right shape rather than
    /// merely being monotonic.
    #[test]
    fn the_coverage_adds_up_to_a_quarter_circle() {
        const RADIUS: u32 = 16;
        let mut total = 0u32;
        for y in 1..=RADIUS {
            for x in 1..=RADIUS {
                total += u32::from(corner_coverage(x, y, RADIUS));
            }
        }
        let pixels = total / 255;
        // pi * 16 * 16 / 4 = 201.06
        assert!(
            (196..=206).contains(&pixels),
            "the corner covered {pixels} pixels, expected about 201"
        );
    }

    #[test]
    fn a_colour_blends_towards_the_other() {
        let black = Colour::rgb(0, 0, 0);
        let white = Colour::rgb(255, 255, 255);
        assert_eq!(black.blend(white, 0), black);
        assert_eq!(black.blend(white, 255), white);
        let half = black.blend(white, 128);
        assert!(((120..=135).contains(&((half.0 >> 16) & 0xFF))));
    }

    #[test]
    fn a_rectangle_insets_without_wrapping() {
        let small = Rect::new(0, 0, 4, 4);
        let inset = small.inset(10);
        assert_eq!((inset.width, inset.height), (0, 0));
    }
}
