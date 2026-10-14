//! CPU triangle rendering into an ordinary compositor surface.
//!
//! Integer arithmetic also works on NexusOS's soft-float userspace target.
//! This is a small flat-shaded renderer, not a Vulkan or OpenGL implementation.

use crate::{Canvas, Colour};
use alloc::vec::Vec;

/// Projected pixel coordinates and inverse depth (larger is nearer).
#[derive(Clone, Copy, Debug)]
pub struct Vertex {
    pub x: i16,
    pub y: i16,
    pub depth: u16,
}

/// A depth attachment. Clear it before starting each frame.
pub struct DepthBuffer {
    width: u32,
    height: u32,
    values: Vec<u16>,
}

impl DepthBuffer {
    /// Allocation is bounded to one million pixels and is fallible.
    pub fn new(width: u32, height: u32) -> Option<Self> {
        let count = width.checked_mul(height)? as usize;
        if width == 0 || height == 0 || width > 4096 || height > 4096 || count > 1024 * 1024 {
            return None;
        }
        let mut values = Vec::new();
        values.try_reserve_exact(count).ok()?;
        values.resize(count, 0);
        Some(Self {
            width,
            height,
            values,
        })
    }

    pub fn clear(&mut self) {
        self.values.fill(0);
    }

    /// Draw both windings, with pixel-centre sampling and the top-left rule.
    /// Screen clipping happens before rasterization; degenerate faces draw nothing.
    /// Depth zero is the background. All vertices must have positive depth.
    /// Returns the number of pixels which passed the depth test.
    pub fn triangle(&mut self, canvas: &mut Canvas, mut v: [Vertex; 3], colour: Colour) -> usize {
        if canvas.width() != self.width
            || canvas.height() != self.height
            || v.iter().any(|p| p.depth == 0)
        {
            return 0;
        }
        let mut area = edge(v[0], v[1], 2 * i64::from(v[2].x), 2 * i64::from(v[2].y));
        if area == 0 {
            return 0;
        }
        if area < 0 {
            v.swap(1, 2);
            area = -area;
        }
        let left = i32::from(v.iter().map(|p| p.x).min().unwrap()).max(0);
        let right = i32::from(v.iter().map(|p| p.x).max().unwrap()).min(self.width as i32);
        let top = i32::from(v.iter().map(|p| p.y).min().unwrap()).max(0);
        let bottom = i32::from(v.iter().map(|p| p.y).max().unwrap()).min(self.height as i32);
        let edges = [(v[1], v[2]), (v[2], v[0]), (v[0], v[1])];
        let mut drawn = 0;
        for y in top..bottom {
            for x in left..right {
                let weights =
                    edges.map(|(a, b)| edge(a, b, 2 * i64::from(x) + 1, 2 * i64::from(y) + 1));
                if weights
                    .iter()
                    .zip(edges)
                    .any(|(&w, (a, b))| w < 0 || (w == 0 && !top_left(a, b)))
                {
                    continue;
                }
                // i16 coordinates and u16 depth keep these products inside i64.
                let depth = ((weights[0] * i64::from(v[0].depth)
                    + weights[1] * i64::from(v[1].depth)
                    + weights[2] * i64::from(v[2].depth))
                    / area) as u16;
                let index = y as usize * self.width as usize + x as usize;
                if depth > self.values[index] {
                    self.values[index] = depth;
                    canvas.set(x as u32, y as u32, colour);
                    drawn += 1;
                }
            }
        }
        drawn
    }
}

/// Four independently depth-tested samples per pixel. Resolve averages sample
/// colours only after all faces, avoiding dark seams and order-dependent edges.
/// This is CPU MSAA; it does not require Vulkan or a hardware GPU driver.
pub struct MsaaBuffer {
    width: u32,
    height: u32,
    depths: Vec<[u16; 4]>,
    colours: Vec<[u32; 4]>,
}

impl MsaaBuffer {
    /// Capture an opaque canvas as the background for uncovered samples.
    /// Memory is bounded to one million pixels (24 bytes each), and fallible.
    pub fn new(canvas: &Canvas) -> Option<Self> {
        let width = canvas.width();
        let height = canvas.height();
        let count = width.checked_mul(height)? as usize;
        if width == 0 || height == 0 || width > 4096 || height > 4096 || count > 1024 * 1024 {
            return None;
        }
        let mut depths = Vec::new();
        let mut colours = Vec::new();
        depths.try_reserve_exact(count).ok()?;
        colours.try_reserve_exact(count).ok()?;
        depths.resize(count, [0; 4]);
        for y in 0..height {
            for x in 0..width {
                colours.push([canvas.get(x, y)?.0 & 0xffffff; 4]);
            }
        }
        Some(Self {
            width,
            height,
            depths,
            colours,
        })
    }

    /// Positive inverse depth, either winding, and top-left shared-edge rules.
    /// Sample positions are quarter/three-quarter pixel centres on both axes.
    /// Returns samples that passed depth, not a pixel coverage approximation.
    pub fn triangle(&mut self, mut v: [Vertex; 3], colour: Colour) -> usize {
        if v.iter().any(|vertex| vertex.depth == 0) {
            return 0;
        }
        let edge = |a: Vertex, b: Vertex, x: i64, y: i64| {
            (4 * i64::from(b.x) - 4 * i64::from(a.x)) * (y - 4 * i64::from(a.y))
                - (4 * i64::from(b.y) - 4 * i64::from(a.y)) * (x - 4 * i64::from(a.x))
        };
        let mut area = edge(v[0], v[1], 4 * i64::from(v[2].x), 4 * i64::from(v[2].y));
        if area == 0 {
            return 0;
        }
        if area < 0 {
            v.swap(1, 2);
            area = -area;
        }
        let left = i32::from(v.iter().map(|vertex| vertex.x).min().unwrap()).max(0);
        let right =
            i32::from(v.iter().map(|vertex| vertex.x).max().unwrap()).min(self.width as i32);
        let top = i32::from(v.iter().map(|vertex| vertex.y).min().unwrap()).max(0);
        let bottom =
            i32::from(v.iter().map(|vertex| vertex.y).max().unwrap()).min(self.height as i32);
        let edges = [(v[1], v[2]), (v[2], v[0]), (v[0], v[1])];
        let mut drawn = 0;
        for y in top..bottom {
            for x in left..right {
                let index = y as usize * self.width as usize + x as usize;
                for (sample, (sx, sy)) in [(1, 1), (3, 1), (1, 3), (3, 3)].into_iter().enumerate() {
                    let weights = edges
                        .map(|(a, b)| edge(a, b, 4 * i64::from(x) + sx, 4 * i64::from(y) + sy));
                    if weights
                        .iter()
                        .zip(edges)
                        .any(|(&weight, (a, b))| weight < 0 || (weight == 0 && !top_left(a, b)))
                    {
                        continue;
                    }
                    let depth = ((weights[0] * i64::from(v[0].depth)
                        + weights[1] * i64::from(v[1].depth)
                        + weights[2] * i64::from(v[2].depth))
                        / area) as u16;
                    if depth > self.depths[index][sample] {
                        self.depths[index][sample] = depth;
                        self.colours[index][sample] = colour.0 & 0xffffff;
                        drawn += 1;
                    }
                }
            }
        }
        drawn
    }

    /// Present only covered pixels, respecting canvas stride and dimensions.
    pub fn resolve(&self, canvas: &mut Canvas) -> usize {
        if canvas.width() != self.width || canvas.height() != self.height {
            return 0;
        }
        let mut written = 0;
        for y in 0..self.height {
            for x in 0..self.width {
                let index = (y * self.width + x) as usize;
                if self.depths[index].iter().all(|&depth| depth == 0) {
                    continue;
                }
                let mut pixel = 0;
                for shift in [0, 8, 16] {
                    let sum: u32 = self.colours[index]
                        .iter()
                        .map(|colour| (colour >> shift) & 255)
                        .sum();
                    pixel |= (sum / 4) << shift;
                }
                canvas.set(x, y, Colour(pixel));
                written += 1;
            }
        }
        written
    }
}

fn edge(a: Vertex, b: Vertex, x: i64, y: i64) -> i64 {
    (2 * i64::from(b.x) - 2 * i64::from(a.x)) * (y - 2 * i64::from(a.y))
        - (2 * i64::from(b.y) - 2 * i64::from(a.y)) * (x - 2 * i64::from(a.x))
}

fn top_left(a: Vertex, b: Vertex) -> bool {
    b.y < a.y || (b.y == a.y && b.x > a.x)
}

/// Draw a rotating, perspective-projected cube. `phase` wraps every 16 frames.
/// Useful as a surface/renderer integration probe; returns pixels written.
pub fn cube(canvas: &mut Canvas, phase: u32) -> Option<usize> {
    const SIN: [i32; 16] = [
        0, 98, 181, 237, 256, 237, 181, 98, 0, -98, -181, -237, -256, -237, -181, -98,
    ];
    let mut depth = MsaaBuffer::new(canvas)?;
    let s = SIN[(phase % 16) as usize];
    let c = SIN[((phase % 16 + 4) % 16) as usize];
    let centre = [(canvas.width() / 2) as i16, (canvas.height() / 2) as i16];
    let focal = canvas.width().min(canvas.height()) as u16;
    let mut vertices = [Vertex {
        x: 0,
        y: 0,
        depth: 0,
    }; 8];
    for (i, vertex) in vertices.iter_mut().enumerate() {
        let x = if i & 1 == 0 { -64 } else { 64 };
        let y = if i & 2 == 0 { -64 } else { 64 };
        let z = if i & 4 == 0 { -64 } else { 64 };
        let rx = (x * c + z * s) / 256;
        let rz = (z * c - x * s) / 256;
        // A fixed camera tilt makes the top and side faces visible.
        *vertex = project(
            [
                rx as i16,
                ((y * 237 - rz * 98) / 256) as i16,
                (320 + (y * 98 + rz * 237) / 256) as i16,
            ],
            centre,
            focal,
            1,
        )?;
    }
    for (corners, colour) in [
        ([0, 1, 3, 2], Colour::rgb(70, 160, 240)),
        ([4, 6, 7, 5], Colour::rgb(220, 100, 80)),
        ([0, 4, 5, 1], Colour::rgb(70, 110, 170)),
        ([2, 3, 7, 6], Colour::rgb(110, 220, 190)),
        ([0, 2, 6, 4], Colour::rgb(180, 110, 220)),
        ([1, 5, 7, 3], Colour::rgb(240, 185, 70)),
    ] {
        depth.triangle(
            [
                vertices[corners[0]],
                vertices[corners[1]],
                vertices[corners[2]],
            ],
            colour,
        );
        depth.triangle(
            [
                vertices[corners[0]],
                vertices[corners[2]],
                vertices[corners[3]],
            ],
            colour,
        );
    }
    Some(depth.resolve(canvas))
}

/// Perspective projection of camera-space coordinates. Positive Z faces forward.
/// Rejects points behind the near plane or outside the representable screen range.
/// Callers must clip geometry crossing the near plane before projecting it.
pub fn project(point: [i16; 3], centre: [i16; 2], focal: u16, near: u16) -> Option<Vertex> {
    let [x, y, z] = point.map(i64::from);
    if near == 0 || z < i64::from(near) {
        return None;
    }
    Some(Vertex {
        x: i16::try_from(i64::from(centre[0]) + x * i64::from(focal) / z).ok()?,
        y: i16::try_from(i64::from(centre[1]) - y * i64::from(focal) / z).ok()?,
        depth: (i64::from(near) * 65535 / z) as u16,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    const RED: Colour = Colour(0xff0000);
    const BLUE: Colour = Colour(0x0000ff);
    fn vertex(x: i16, y: i16, depth: u16) -> Vertex {
        Vertex { x, y, depth }
    }

    #[test]
    fn msaa_diagonal_resolves_covered_and_background_samples() {
        let mut pixels = [BLUE.0; 64];
        let mut canvas = unsafe { Canvas::packed(pixels.as_mut_ptr() as usize, 8, 8) };
        let mut msaa = MsaaBuffer::new(&canvas).unwrap();
        assert!(msaa.triangle([vertex(0, 0, 10), vertex(4, 0, 10), vertex(0, 4, 10)], RED) > 0);
        assert!(msaa.resolve(&mut canvas) > 0);
        assert_eq!(pixels[1 * 8 + 1], RED.0);
        assert_eq!(pixels[2 * 8 + 1], 0x3f00bf); // One red and three blue samples.
        assert_eq!(pixels[7 * 8 + 7], BLUE.0);
    }

    #[test]
    fn msaa_shared_edges_have_no_gaps_or_double_coverage() {
        let mut pixels = [0u32; 64];
        let mut canvas = unsafe { Canvas::packed(pixels.as_mut_ptr() as usize, 8, 8) };
        let mut msaa = MsaaBuffer::new(&canvas).unwrap();
        let first = msaa.triangle([vertex(1, 1, 10), vertex(7, 1, 10), vertex(7, 7, 10)], RED);
        let second = msaa.triangle([vertex(1, 1, 20), vertex(7, 7, 20), vertex(1, 7, 20)], RED);
        // A higher second depth exposes double-covered samples rather than
        // hiding them with an equal-depth rejection.
        assert_eq!(first + second, 36 * 4);
        assert_eq!(msaa.resolve(&mut canvas), 36);
        assert_eq!(pixels.iter().filter(|&&pixel| pixel == RED.0).count(), 36);
    }

    #[test]
    fn msaa_depth_and_winding_are_order_independent_at_edges() {
        let mut outcomes = Vec::new();
        for reverse in [false, true] {
            let mut pixels = [0u32; 64];
            let mut canvas = unsafe { Canvas::packed(pixels.as_mut_ptr() as usize, 8, 8) };
            let mut msaa = MsaaBuffer::new(&canvas).unwrap();
            let mut faces = [
                (
                    [vertex(-2, -2, 10), vertex(7, 0, 10), vertex(0, 7, 10)],
                    BLUE,
                ),
                ([vertex(0, 0, 30), vertex(4, 0, 30), vertex(0, 4, 30)], RED),
            ];
            if reverse {
                faces.reverse();
            }
            for (mut vertices, colour) in faces {
                if reverse {
                    vertices.swap(0, 1);
                }
                msaa.triangle(vertices, colour);
            }
            msaa.resolve(&mut canvas);
            outcomes.push(pixels);
        }
        assert_eq!(outcomes[0], outcomes[1]);
        assert_eq!(outcomes[0][2 * 8 + 1], 0x3f00bf);
    }

    #[test]
    fn msaa_resolve_respects_strided_surfaces_and_rejects_other_dimensions() {
        let mut pixels = [0x12345678u32; 32];
        let mut canvas = unsafe { Canvas::strided(pixels.as_mut_ptr() as usize, 4, 4, 8) };
        let mut msaa = MsaaBuffer::new(&canvas).unwrap();
        assert_eq!(msaa.triangle([vertex(0, 0, 10); 3], RED), 0);
        msaa.triangle([vertex(0, 0, 10), vertex(4, 0, 10), vertex(0, 4, 10)], RED);
        let mut other = unsafe { Canvas::packed(pixels.as_mut_ptr() as usize, 2, 2) };
        assert_eq!(msaa.resolve(&mut other), 0);
        assert!(msaa.resolve(&mut canvas) > 0);
        for row in 0..4 {
            assert_eq!(&pixels[row * 8 + 4..row * 8 + 8], &[0x12345678; 4]);
        }
    }

    #[test]
    fn shared_edge_covers_square_exactly_once() {
        let mut pixels = [0u32; 64];
        let mut canvas = unsafe { Canvas::packed(pixels.as_mut_ptr() as usize, 8, 8) };
        let mut depth = DepthBuffer::new(8, 8).unwrap();
        let a = depth.triangle(
            &mut canvas,
            [vertex(1, 1, 10), vertex(7, 1, 10), vertex(7, 7, 10)],
            RED,
        );
        depth.clear(); // Detect double coverage rather than hiding it with equal depth.
        let b = depth.triangle(
            &mut canvas,
            [vertex(1, 1, 10), vertex(7, 7, 10), vertex(1, 7, 10)],
            RED,
        );
        assert_eq!(a + b, 36);
        assert_eq!(pixels.iter().filter(|&&p| p == RED.0).count(), 36);
    }

    #[test]
    fn depth_is_order_independent_and_winding_independent() {
        let mut outcomes = Vec::new();
        for reverse in [false, true] {
            let mut pixels = [0u32; 64];
            let mut canvas = unsafe { Canvas::packed(pixels.as_mut_ptr() as usize, 8, 8) };
            let mut depth = DepthBuffer::new(8, 8).unwrap();
            let mut faces = [(10, BLUE), (30, RED)];
            if reverse {
                faces.reverse();
            }
            for (z, colour) in faces {
                let mut vertices = [vertex(-4, -4, z), vertex(12, 0, z), vertex(0, 12, z)];
                if reverse {
                    vertices.swap(0, 1);
                }
                depth.triangle(&mut canvas, vertices, colour);
            }
            assert_eq!(pixels[2 * 8 + 2], RED.0);
            outcomes.push(pixels);
        }
        assert_eq!(outcomes[0], outcomes[1]);
    }

    #[test]
    fn rejects_invalid_buffers_and_degenerate_triangles() {
        assert!(DepthBuffer::new(u32::MAX, 2).is_none());
        assert!(DepthBuffer::new(0, 1).is_none());
        let mut pixels = [0u32; 4];
        let mut canvas = unsafe { Canvas::packed(pixels.as_mut_ptr() as usize, 2, 2) };
        let mut depth = DepthBuffer::new(2, 2).unwrap();
        assert_eq!(depth.triangle(&mut canvas, [vertex(0, 0, 10); 3], RED), 0);
        assert_eq!(
            depth.triangle(
                &mut canvas,
                [
                    vertex(i16::MIN, i16::MIN, 10),
                    vertex(i16::MAX, i16::MIN, 10),
                    vertex(0, i16::MAX, 10)
                ],
                RED
            ),
            4
        );
        assert_eq!(project([1, 1, 0], [0, 0], 10, 1).map(|v| v.depth), None);
        assert_eq!(project([10, 0, 100], [32, 32], 100, 1).unwrap().x, 42);
    }
}
