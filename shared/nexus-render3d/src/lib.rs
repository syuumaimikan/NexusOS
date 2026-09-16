#![no_std]
//! Triangles, a depth buffer, and no graphics hardware whatsoever.
//!
//! # What this is, and what it is not
//!
//! This is **3D**: a transform, a projection, triangles filled one pixel at a
//! time, and a depth buffer so that what is behind stays behind. It is the
//! whole of what "drawing in three dimensions" means at the bottom, and it is
//! real -- the picture it makes is a picture of a solid.
//!
//! It is **not Vulkan**, and it is not accelerated by anything. Vulkan is an
//! interface to a piece of hardware with its own instruction set, a shader
//! compiler, a memory manager and a driver stack, and none of those is here or
//! within reach. Every pixel below is computed by the processor. Saying that
//! plainly at the top of the file is worth more than a fast library with a
//! borrowed name.
//!
//! # Why the arithmetic is integer
//!
//! The target this runs on is built with `-sse` and `+soft-float`, so every
//! floating-point operation is a call into a software implementation. A
//! rasteriser does a handful of those *per pixel*, which makes the difference
//! between a still picture and a moving one.
//!
//! So: fixed point, sixteen bits of fraction, in an `i32`, with `i64` for the
//! products. That is not a compromise forced by the platform -- edge functions
//! were done in integers on machines that had floating point, because an
//! integer edge function is exact, and exactness is what stops two triangles
//! that share an edge from either overlapping on it or leaving a gap.

extern crate alloc;

#[cfg(test)]
mod tests;

use alloc::vec;
use alloc::vec::Vec;

/// Bits of fraction in a fixed-point number.
pub const FRACTION: u32 = 16;

/// One, in fixed point.
pub const ONE: i32 = 1 << FRACTION;

/// A fixed-point number: an `i32` holding sixteen bits of fraction.
pub type Fixed = i32;

/// Turn a whole number into a fixed-point one.
#[must_use]
pub const fn whole(value: i32) -> Fixed {
    value << FRACTION
}

/// Multiply two fixed-point numbers.
///
/// Through `i64`, because two numbers with sixteen bits of fraction each have
/// thirty-two between them and the product of two values near one another
/// overflows an `i32` long before the result does.
#[must_use]
pub const fn multiply(a: Fixed, b: Fixed) -> Fixed {
    (((a as i64) * (b as i64)) >> FRACTION) as i32
}

/// Divide one fixed-point number by another, or zero when asked to divide by
/// nothing.
///
/// Zero rather than a panic. This is called once per vertex on data that came
/// from a program, and a renderer that stopped the machine because a point
/// landed exactly on the eye would be a renderer nobody could hand untrusted
/// geometry to.
#[must_use]
pub const fn divide(a: Fixed, b: Fixed) -> Fixed {
    if b == 0 {
        return 0;
    }
    (((a as i64) << FRACTION) / (b as i64)) as i32
}

/// A point or a direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Point {
    pub x: Fixed,
    pub y: Fixed,
    pub z: Fixed,
}

impl Point {
    #[must_use]
    pub const fn new(x: Fixed, y: Fixed, z: Fixed) -> Self {
        Self { x, y, z }
    }

    /// From three whole numbers, which is how geometry is usually written down.
    #[must_use]
    pub const fn at(x: i32, y: i32, z: i32) -> Self {
        Self::new(whole(x), whole(y), whole(z))
    }
}

/// A transform: rotation, scale and translation together.
///
/// Three rows of four, not four of four. The fourth row of a transform matrix
/// is `0 0 0 1` in everything this will ever be asked to do, and carrying it
/// would be twelve multiplications a vertex spent proving it is still there.
/// Perspective happens at projection, where the divide is, rather than here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Transform {
    pub rows: [[Fixed; 4]; 3],
}

impl Transform {
    /// The transform that changes nothing.
    #[must_use]
    pub const fn identity() -> Self {
        Self {
            rows: [[ONE, 0, 0, 0], [0, ONE, 0, 0], [0, 0, ONE, 0]],
        }
    }

    /// Move by `by`.
    #[must_use]
    pub const fn translation(by: Point) -> Self {
        let mut out = Self::identity();
        out.rows[0][3] = by.x;
        out.rows[1][3] = by.y;
        out.rows[2][3] = by.z;
        out
    }

    /// Turn about the vertical axis, by an angle given as a sine and a cosine.
    ///
    /// Taken rather than computed, because a sine table belongs to whoever is
    /// animating and not to the renderer -- and because a renderer that owned
    /// one would have to decide how many entries it had, which is a decision
    /// about smoothness that the caller is better placed to make.
    #[must_use]
    pub const fn turn_y(sine: Fixed, cosine: Fixed) -> Self {
        let mut out = Self::identity();
        out.rows[0][0] = cosine;
        out.rows[0][2] = sine;
        out.rows[2][0] = -sine;
        out.rows[2][2] = cosine;
        out
    }

    /// Turn about the horizontal axis.
    #[must_use]
    pub const fn turn_x(sine: Fixed, cosine: Fixed) -> Self {
        let mut out = Self::identity();
        out.rows[1][1] = cosine;
        out.rows[1][2] = -sine;
        out.rows[2][1] = sine;
        out.rows[2][2] = cosine;
        out
    }

    /// This transform, then `then`.
    #[must_use]
    pub fn then(self, then: Self) -> Self {
        let mut rows = [[0i32; 4]; 3];
        for (row, out) in rows.iter_mut().enumerate() {
            for (column, cell) in out.iter_mut().enumerate() {
                let mut sum = 0i64;
                for index in 0..3 {
                    sum += (then.rows[row][index] as i64) * (self.rows[index][column] as i64);
                }
                *cell = (sum >> FRACTION) as i32;
                // The fourth column also picks up `then`'s own translation,
                // which is the part a three-row matrix has to be told about
                // rather than getting from the missing row.
                if column == 3 {
                    *cell = cell.wrapping_add(then.rows[row][3]);
                }
            }
        }
        Self { rows }
    }

    /// Apply this transform to a point.
    #[must_use]
    pub fn apply(&self, point: Point) -> Point {
        let one = |row: &[Fixed; 4]| -> Fixed {
            let sum = (row[0] as i64) * (point.x as i64)
                + (row[1] as i64) * (point.y as i64)
                + (row[2] as i64) * (point.z as i64);
            ((sum >> FRACTION) as i32).wrapping_add(row[3])
        };
        Point::new(one(&self.rows[0]), one(&self.rows[1]), one(&self.rows[2]))
    }
}

/// A colour, as the framebuffer wants it.
pub type Colour = u32;

/// Where a triangle landed on the screen, in whole pixels, with a depth.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Vertex {
    pub x: i32,
    pub y: i32,
    /// Distance from the eye. Larger is further away.
    pub depth: Fixed,
}

/// Somewhere to draw, and what is already in front.
///
/// The colour buffer belongs to the caller -- it is a window's surface, or a
/// framebuffer -- and is borrowed for the length of a frame. The depth buffer
/// belongs to this, because nothing outside has any use for it.
pub struct Canvas<'a> {
    pixels: &'a mut [Colour],
    depth: Vec<Fixed>,
    width: usize,
    height: usize,
}

impl<'a> Canvas<'a> {
    /// Wrap a buffer of `width * height` pixels.
    ///
    /// # Errors
    ///
    /// `None` when the buffer is not exactly that size. Not a clamp: a caller
    /// that got the size wrong has a bug, and drawing a slightly wrong picture
    /// would hide it.
    #[must_use]
    pub fn new(pixels: &'a mut [Colour], width: usize, height: usize) -> Option<Self> {
        if width == 0 || height == 0 || pixels.len() != width * height {
            return None;
        }
        Some(Self {
            pixels,
            depth: vec![Fixed::MAX; width * height],
            width,
            height,
        })
    }

    #[must_use]
    pub const fn width(&self) -> usize {
        self.width
    }

    #[must_use]
    pub const fn height(&self) -> usize {
        self.height
    }

    /// Fill with one colour and forget every depth.
    ///
    /// Both together, always. A frame drawn on top of the last frame's depths
    /// shows this frame's triangles disappearing behind the previous frame's,
    /// which looks like the geometry is wrong and is not.
    pub fn clear(&mut self, colour: Colour) {
        for pixel in self.pixels.iter_mut() {
            *pixel = colour;
        }
        self.depth.fill(Fixed::MAX);
    }

    /// What is at a pixel, for a test to ask.
    #[must_use]
    pub fn pixel(&self, x: usize, y: usize) -> Option<Colour> {
        (x < self.width && y < self.height).then(|| self.pixels[y * self.width + x])
    }

    /// How far away the thing at a pixel is.
    #[must_use]
    pub fn depth_at(&self, x: usize, y: usize) -> Option<Fixed> {
        (x < self.width && y < self.height).then(|| self.depth[y * self.width + x])
    }

    /// Fill one triangle, keeping whatever is nearer.
    ///
    /// Returns how many pixels it actually wrote, which is what a test can
    /// check and what a program can use to know whether anything was visible.
    ///
    /// Triangles facing away are dropped. A solid's far side is drawn by the
    /// same call as its near side, and without this every surface would be
    /// painted twice -- once correctly and once by the face behind it, which
    /// the depth buffer would then have to sort out at a cost of the whole
    /// interior.
    pub fn triangle(&mut self, corners: [Vertex; 3], colour: Colour) -> usize {
        let [a, b, c] = corners;

        // Twice the signed area. Its sign is which way the triangle faces, and
        // its magnitude is what the barycentric weights are divided by.
        let area = edge(a, b, c.x, c.y);
        if area <= 0 {
            return 0;
        }

        // The rectangle it could possibly touch, clipped to the canvas. Without
        // the clip a triangle mostly off-screen would be walked in full.
        let left = a.x.min(b.x).min(c.x).max(0);
        let right = a.x.max(b.x).max(c.x).min(self.width as i32 - 1);
        let top = a.y.min(b.y).min(c.y).max(0);
        let bottom = a.y.max(b.y).max(c.y).min(self.height as i32 - 1);
        if left > right || top > bottom {
            return 0;
        }

        let mut written = 0usize;
        for y in top..=bottom {
            for x in left..=right {
                // Three edge functions. A point is inside when it is on the
                // same side of all three, and "the same side" is a sign test
                // rather than a comparison against a tolerance -- which is why
                // this is exact and why two triangles sharing an edge meet on
                // it without a seam.
                let weight_a = edge(b, c, x, y);
                let weight_b = edge(c, a, x, y);
                let weight_c = edge(a, b, x, y);
                if weight_a < 0 || weight_b < 0 || weight_c < 0 {
                    continue;
                }

                // Depth, interpolated across the triangle by those same
                // weights. Computed in `i64` and divided once: the weights are
                // areas and can be large.
                let depth = (weight_a * i64::from(a.depth)
                    + weight_b * i64::from(b.depth)
                    + weight_c * i64::from(c.depth))
                    / area;

                let at = y as usize * self.width + x as usize;
                if depth >= i64::from(self.depth[at]) {
                    continue;
                }
                self.depth[at] = depth as i32;
                self.pixels[at] = colour;
                written += 1;
            }
        }
        written
    }
}

/// Twice the signed area of the triangle `from`, `to`, `(x, y)`.
///
/// Positive when the point is to one side, negative to the other, zero on the
/// line. Everything the rasteriser decides is decided by the sign of this.
fn edge(from: Vertex, to: Vertex, x: i32, y: i32) -> i64 {
    let ax = (to.x - from.x) as i64;
    let ay = (to.y - from.y) as i64;
    let bx = (x - from.x) as i64;
    let by = (y - from.y) as i64;
    ax * by - ay * bx
}

/// Turn a point in front of the eye into a pixel.
///
/// The one divide in the whole pipeline, and what makes the picture look like a
/// picture rather than a plan: things further away are divided by more, so they
/// are smaller.
///
/// `None` for anything at or behind the eye. Clipping against the near plane
/// properly means cutting triangles in half and making new ones; refusing the
/// vertex is the honest small version, and its cost is stated where it is felt
/// -- a triangle with one corner behind the eye is dropped entirely rather than
/// drawn wrong.
#[must_use]
pub fn project(point: Point, width: usize, height: usize, scale: Fixed) -> Option<Vertex> {
    if point.z <= 0 {
        return None;
    }
    let half_width = (width as i32) / 2;
    let half_height = (height as i32) / 2;
    let x = divide(multiply(point.x, scale), point.z);
    let y = divide(multiply(point.y, scale), point.z);
    Some(Vertex {
        x: half_width + (x >> FRACTION),
        // Down the screen is up in the world, which is the one place this
        // renderer disagrees with the framebuffer underneath it.
        y: half_height - (y >> FRACTION),
        depth: point.z,
    })
}
