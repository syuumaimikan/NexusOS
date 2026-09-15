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
            face: font::Face::Crisp,
            antialias: false,
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
            face: font::Face::Crisp,
            antialias: false,
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
            core::ptr::write_volatile((self.base + offset) as *mut u32, colour.0);
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
            self.fill(rect, colour);
            return;
        }

        // The middle band and the two side bands are square, so they are filled
        // outright; only the four corner squares need sampling.
        self.fill(
            Rect::new(
                rect.x + radius,
                rect.y,
                rect.width - radius * 2,
                rect.height,
            ),
            colour,
        );
        self.fill(
            Rect::new(rect.x, rect.y + radius, radius, rect.height - radius * 2),
            colour,
        );
        self.fill(
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
    /// Nearest-neighbour, which for a bitmap face is not a compromise: the
    /// glyphs *are* pixels, and doubling them is what the face would look like
    /// on a display with half the resolution. Smoothing them would invent
    /// detail the face does not have.
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
pub const TITLE_BAR: u32 = 14;

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
    const SAMPLES: u32 = 4;
    let limit = (radius * SAMPLES) * (radius * SAMPLES);
    let mut inside = 0;
    for down in 0..SAMPLES {
        for across in 0..SAMPLES {
            // The centre of each sub-pixel, in quarter-pixel units.
            let sample_x = (x - 1) * SAMPLES + across * 2 / 2 + 1;
            let sample_y = (y - 1) * SAMPLES + down * 2 / 2 + 1;
            if sample_x * sample_x + sample_y * sample_y <= limit {
                inside += 1;
            }
        }
    }
    (inside * 255 / (SAMPLES * SAMPLES)) as u8
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
        let edge = corner_coverage(RADIUS, 1, RADIUS);
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
    fn a_pixel_outside_the_radius_is_never_covered() {
        assert_eq!(corner_coverage(9, 1, 8), 0);
        assert_eq!(corner_coverage(1, 9, 8), 0);
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
