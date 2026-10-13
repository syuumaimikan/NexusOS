//! Host preview using the same renderer as the guest's client window.
use nexus_ui::{raster3d, Canvas, Colour};
use std::io::Write;

fn main() -> std::io::Result<()> {
    let path = std::env::args().nth(1).expect("usage: cube <preview.ppm>");
    let mut pixels = vec![0u32; 384 * 128];
    for (column, phase) in [0, 2, 4].into_iter().enumerate() {
        // SAFETY: each view is a disjoint 128x128 rectangle in the allocation.
        let mut canvas = unsafe {
            Canvas::strided(
                pixels.as_mut_ptr().add(column * 128) as usize,
                128,
                128,
                384,
            )
        };
        canvas.fill(canvas.bounds(), Colour::rgb(12, 22, 40));
        assert!(raster3d::cube(&mut canvas, phase).unwrap() > 0);
    }
    let mut file = std::fs::File::create(path)?;
    file.write_all(b"P6\n384 128\n255\n")?;
    for pixel in pixels {
        file.write_all(&[(pixel >> 16) as u8, (pixel >> 8) as u8, pixel as u8])?;
    }
    Ok(())
}
