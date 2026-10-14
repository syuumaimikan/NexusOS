//! Transparent overlay coverage and bounded CPU backdrop blur.
extern crate alloc;
use alloc::vec::Vec;

fn coverage(pixel: u32) -> u32 {
    if pixel == 0 {
        0
    } else if pixel >> 24 == 0 {
        255
    } else {
        pixel >> 24
    }
}

/// Straight-alpha source-over, with additional glyph/edge coverage.
pub fn over(under: u32, colour: u32, amount: u8) -> u32 {
    let source = coverage(colour) * u32::from(amount) / 255;
    let destination = coverage(under) * (255 - source) / 255;
    let alpha = source + destination;
    if alpha == 0 {
        return 0;
    }
    let mut result = alpha << 24;
    for shift in [0, 8, 16] {
        result |= ((((colour >> shift) & 255) * source + ((under >> shift) & 255) * destination)
            / alpha)
            << shift;
    }
    result
}

/// Blur an opaque RGB backdrop with replicated edges. Sliding windows require
/// linear work and one temporary pixel buffer; output is ordinary packed RGB.
/// A bounded radius and pixel count keep malformed sizes out of the allocator.
pub fn blur(pixels: &mut [u32], width: usize, height: usize, radius: usize) -> bool {
    let Some(count) = width.checked_mul(height) else {
        return false;
    };
    if width == 0 || height == 0 || count != pixels.len() || count > 1024 * 1024 {
        return false;
    }
    let radius = radius.min(32);
    if radius == 0 {
        return true;
    }
    let mut horizontal = Vec::new();
    if horizontal.try_reserve_exact(count).is_err() {
        return false;
    }
    horizontal.resize(count, 0u32);
    let taps = (radius * 2 + 1) as u32;
    for row in 0..height {
        let at = row * width;
        let mut sums = [0u32; 3];
        for offset in -(radius as isize)..=radius as isize {
            add(
                &mut sums,
                pixels[at + offset.clamp(0, width as isize - 1) as usize],
            );
        }
        for column in 0..width {
            horizontal[at + column] = packed(sums, taps);
            subtract(&mut sums, pixels[at + column.saturating_sub(radius)]);
            add(&mut sums, pixels[at + (column + radius + 1).min(width - 1)]);
        }
    }
    for column in 0..width {
        let mut sums = [0u32; 3];
        for offset in -(radius as isize)..=radius as isize {
            add(
                &mut sums,
                horizontal[offset.clamp(0, height as isize - 1) as usize * width + column],
            );
        }
        for row in 0..height {
            pixels[row * width + column] = packed(sums, taps);
            subtract(
                &mut sums,
                horizontal[row.saturating_sub(radius) * width + column],
            );
            add(
                &mut sums,
                horizontal[(row + radius + 1).min(height - 1) * width + column],
            );
        }
    }
    true
}

fn add(sums: &mut [u32; 3], pixel: u32) {
    for (channel, shift) in [0, 8, 16].iter().copied().enumerate() {
        sums[channel] += (pixel >> shift) & 255;
    }
}
fn subtract(sums: &mut [u32; 3], pixel: u32) {
    for (channel, shift) in [0, 8, 16].iter().copied().enumerate() {
        sums[channel] -= (pixel >> shift) & 255;
    }
}
fn packed(sums: [u32; 3], taps: u32) -> u32 {
    sums[0] / taps | (sums[1] / taps) << 8 | (sums[2] / taps) << 16
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn transparent_edges_keep_straight_alpha_and_colour() {
        assert_eq!(over(0, 0x80ff_ffff, 128), 0x40ff_ffff);
        assert_eq!(over(0x80ff_ffff, 0x0000_00ff, 255), 0xff00_00ff);
        assert_eq!(over(0, 0, 255), 0);
        assert_eq!(over(0x8099_aaee, 0x0022_3344, 0), 0x8099_aaee);
    }
    #[test]
    fn blur_preserves_uniform_backdrops_and_both_axes() {
        let mut uniform = [0x0099_aabb; 12];
        assert!(blur(&mut uniform, 4, 3, 8));
        assert_eq!(uniform, [0x0099_aabb; 12]);
        let mut point = [0; 9];
        point[4] = 0x00ff_ffff;
        assert!(blur(&mut point, 3, 3, 1));
        assert_eq!(point, [0x001c_1c1c; 9]);
        let mut step = [0, 0, 0xffffff, 0xffffff, 0xffffff];
        assert!(blur(&mut step, 5, 1, 1));
        assert_eq!(step, [0, 0x555555, 0xaaaaaa, 0xffffff, 0xffffff]);
    }
    #[test]
    fn invalid_shape_is_rejected_without_writes() {
        let mut pixels = [0x0012_3456; 4];
        assert!(!blur(&mut pixels, 3, 2, 8));
        assert_eq!(pixels, [0x0012_3456; 4]);
    }
}
