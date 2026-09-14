//! Pictures, decoded into pixels this system can draw.
//!
//! PNG and BMP, into the one format [`nexus_ui::Canvas`] wants: eight bits each
//! of blue, green and red in the low three bytes of a `u32`, one per pixel, in
//! rows from the top.
//!
//! # Why these two
//!
//! PNG because it is what pictures are, and because it is decodable: the format
//! is a container, a filter, and DEFLATE, all of which are written down and
//! none of which is a patent. BMP because it is four lines of work once the
//! rest exists and because it is what a screenshot is.
//!
//! JPEG is here too, in [`jpeg`]: baseline sequential, greyscale and YCbCr,
//! with 4:4:4, 4:2:2 and 4:2:0 chroma and restart markers. Progressive coding
//! is refused by name rather than guessed at.
//!
//! # Alpha
//!
//! Flattened onto a background the caller chooses, because the surface a
//! program draws into has no alpha channel: the compositor composites windows,
//! not pixels within one. A picture with transparency drawn on a dark window
//! should have a dark background behind it, and the caller is the only one that
//! knows what its window looks like.
//!
//! # Bounds
//!
//! Every input here came from somewhere else. Dimensions are checked before
//! anything is allocated, the product of them is computed with `checked_mul`,
//! and the caller says how many pixels it is willing to hold. A decoder that
//! believed a header saying four billion by four billion would be a decoder
//! that a picture could stop the machine with.

#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]

extern crate alloc;

pub mod avi;
pub mod jpeg;

use alloc::vec::Vec;

/// The most pixels a picture may have before this refuses it.
///
/// Sixty-four megapixels is four times a large photograph and a quarter of a
/// gibibyte once decoded, which is the point at which refusing is kinder than
/// trying.
pub const MOST_PIXELS: usize = 64 * 1024 * 1024;

/// Why a picture would not decode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trouble {
    /// Nothing here recognises the first few bytes.
    NotAPicture,
    /// The file ends in the middle of something.
    Truncated,
    /// A header says something impossible.
    Malformed,
    /// A valid file this decoder does not handle, and which one.
    Unsupported(&'static str),
    /// Larger than the caller allowed, or than [`MOST_PIXELS`].
    TooLarge,
    /// The compressed part would not decompress.
    Compressed(nexus_inflate::Trouble),
    /// A checksum in the file does not match its own data.
    Corrupt,
}

impl core::fmt::Display for Trouble {
    fn fmt(&self, out: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::NotAPicture => out.write_str("this is not a picture this system reads"),
            Self::Truncated => out.write_str("the file ends in the middle of the picture"),
            Self::Malformed => out.write_str("the picture's header says something impossible"),
            Self::Unsupported(what) => {
                write!(out, "this picture uses {what}, which is not read yet")
            }
            Self::TooLarge => out.write_str("the picture is larger than this program will hold"),
            Self::Compressed(why) => write!(out, "{why}"),
            Self::Corrupt => out.write_str("the picture does not match its own checksum"),
        }
    }
}

/// A decoded picture: pixels, in rows from the top.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Picture {
    pub width: u32,
    pub height: u32,
    /// One `u32` per pixel, `0x00RRGGBB`.
    pub pixels: Vec<u32>,
}

impl Picture {
    /// The pixel at a position, if it is on the picture.
    #[must_use]
    pub fn at(&self, x: u32, y: u32) -> Option<u32> {
        if x >= self.width || y >= self.height {
            return None;
        }
        self.pixels
            .get((y as usize) * (self.width as usize) + x as usize)
            .copied()
    }
}

/// What kind of file this is, by its first bytes.
///
/// By content and not by the name. A file called `.png` that is a bitmap is a
/// file somebody renamed, and the bytes are the only thing that knows.
#[must_use]
pub fn kind(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(&PNG_MAGIC) {
        return Some("png");
    }
    if bytes.starts_with(b"BM") {
        return Some("bmp");
    }
    if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return Some("jpeg");
    }
    // A recording rather than a picture. Named here because this is the one
    // place that says what a file is, and a caller listing a directory wants
    // one answer per file -- not "a picture, or else ask the video reader".
    // `decode` still refuses it: it is not a picture and cannot be made into
    // one, and the caller that wants it goes to `avi`.
    if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"AVI " {
        return Some("avi");
    }
    None
}

/// Decode whatever this is, flattening any transparency onto `behind`.
pub fn decode(bytes: &[u8], behind: u32, most_pixels: usize) -> Result<Picture, Trouble> {
    match kind(bytes) {
        Some("png") => png(bytes, behind, most_pixels),
        Some("bmp") => bmp(bytes, most_pixels),
        Some("jpeg") => jpeg::decode(bytes, most_pixels),
        _ => Err(Trouble::NotAPicture),
    }
}

/// How many pixels this many rows and columns come to, if that is a number this
/// program will hold.
pub(crate) fn pixel_count(width: u32, height: u32, most: usize) -> Result<usize, Trouble> {
    if width == 0 || height == 0 {
        return Err(Trouble::Malformed);
    }
    let count = (width as usize)
        .checked_mul(height as usize)
        .ok_or(Trouble::TooLarge)?;
    if count > most.min(MOST_PIXELS) {
        return Err(Trouble::TooLarge);
    }
    Ok(count)
}

/// Mix a colour onto a background by its alpha.
const fn over(red: u8, green: u8, blue: u8, alpha: u8, behind: u32) -> u32 {
    if alpha == 255 {
        return (red as u32) << 16 | (green as u32) << 8 | blue as u32;
    }
    let inverse = 255 - alpha as u32;
    let alpha = alpha as u32;
    let out_red = (red as u32 * alpha + ((behind >> 16) & 0xFF) * inverse) / 255;
    let out_green = (green as u32 * alpha + ((behind >> 8) & 0xFF) * inverse) / 255;
    let out_blue = (blue as u32 * alpha + (behind & 0xFF) * inverse) / 255;
    out_red << 16 | out_green << 8 | out_blue
}

// -- BMP ---------------------------------------------------------------------

/// Decode a Windows bitmap.
///
/// Uncompressed 24- and 32-bit only, which is what every screenshot and every
/// `File > Save As` produces. Run-length encoded and palettised bitmaps exist
/// and are refused by name rather than guessed at.
pub fn bmp(bytes: &[u8], most_pixels: usize) -> Result<Picture, Trouble> {
    // File header is 14 bytes, then a DIB header whose first field is its own
    // size. Only the 40-byte BITMAPINFOHEADER and its longer successors are
    // read; they agree about everything this needs.
    if bytes.len() < 54 {
        return Err(Trouble::Truncated);
    }
    let read32 = |at: usize| -> u32 {
        u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
    };
    let read16 = |at: usize| -> u16 { u16::from_le_bytes([bytes[at], bytes[at + 1]]) };

    let data_at = read32(10) as usize;
    let header_size = read32(14);
    if header_size < 40 {
        return Err(Trouble::Unsupported("an old bitmap header"));
    }
    let width = read32(18) as i32;
    let raw_height = read32(22) as i32;
    let planes = read16(26);
    let depth = read16(28);
    let compression = read32(30);

    if planes != 1 {
        return Err(Trouble::Malformed);
    }
    if compression != 0 && compression != 3 {
        return Err(Trouble::Unsupported("a compressed bitmap"));
    }
    if depth != 24 && depth != 32 {
        return Err(Trouble::Unsupported("a bitmap with a palette"));
    }
    if width <= 0 {
        return Err(Trouble::Malformed);
    }

    // A negative height means the rows are stored top to bottom. Positive --
    // which is almost every file -- means bottom to top, which is the one thing
    // about this format that catches everybody once.
    let upside_down = raw_height > 0;
    let height = raw_height.unsigned_abs();
    let width = width as u32;

    let count = pixel_count(width, height, most_pixels)?;
    let sample = (depth / 8) as usize;
    // Rows are padded to a multiple of four bytes.
    let stride = ((width as usize * sample) + 3) & !3;
    let needed = stride
        .checked_mul(height as usize)
        .ok_or(Trouble::TooLarge)?;
    let data = bytes
        .get(data_at..data_at + needed)
        .ok_or(Trouble::Truncated)?;

    let mut pixels = alloc::vec![0u32; count];
    for row in 0..height as usize {
        let source = if upside_down {
            height as usize - 1 - row
        } else {
            row
        };
        let line = &data[source * stride..source * stride + width as usize * sample];
        for column in 0..width as usize {
            let at = column * sample;
            // Stored blue, green, red -- and the fourth byte is alpha only in
            // some files and padding in others, so it is ignored. A bitmap that
            // meant it would have said so with a mask header this does not read.
            let blue = line[at];
            let green = line[at + 1];
            let red = line[at + 2];
            pixels[row * width as usize + column] =
                (red as u32) << 16 | (green as u32) << 8 | blue as u32;
        }
    }

    Ok(Picture {
        width,
        height,
        pixels,
    })
}

// -- PNG ---------------------------------------------------------------------

const PNG_MAGIC: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];

/// Decode a PNG.
///
/// Eight- and sixteen-bit greyscale, truecolour, and both with alpha, plus
/// palettised images. Interlaced images are refused by name: Adam7 is a second
/// pass over everything here for a format almost nothing writes any more.
pub fn png(bytes: &[u8], behind: u32, most_pixels: usize) -> Result<Picture, Trouble> {
    if !bytes.starts_with(&PNG_MAGIC) {
        return Err(Trouble::NotAPicture);
    }

    let mut at = PNG_MAGIC.len();
    let mut header: Option<Header> = None;
    let mut palette: Vec<[u8; 3]> = Vec::new();
    let mut transparency: Vec<u8> = Vec::new();
    let mut compressed: Vec<u8> = Vec::new();
    let mut ended = false;

    while at + 8 <= bytes.len() {
        let length =
            u32::from_be_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]]) as usize;
        let kind = bytes.get(at + 4..at + 8).ok_or(Trouble::Truncated)?;
        let body = bytes
            .get(at + 8..at + 8 + length)
            .ok_or(Trouble::Truncated)?;
        // The four CRC bytes have to be *there*, even though they are not
        // checked. A file that stops between a chunk's body and its checksum
        // stopped in the middle of a chunk, and the last four bytes of a PNG
        // are exactly that case -- which is how a file cut four bytes short
        // decoded happily until this line existed.
        bytes
            .get(at + 8 + length..at + 12 + length)
            .ok_or(Trouble::Truncated)?;
        // The CRC itself is not checked: the zlib stream
        // inside carries an Adler-32 over everything that matters, and it is
        // verified. A per-chunk CRC would catch a corrupt palette, which is
        // worth having and is not worth a second checksum implementation yet.
        at += 12 + length;

        match kind {
            b"IHDR" => header = Some(Header::read(body)?),
            b"PLTE" => {
                if body.len() % 3 != 0 {
                    return Err(Trouble::Malformed);
                }
                palette = body.as_chunks::<3>().0.to_vec();
            }
            b"tRNS" => transparency = body.to_vec(),
            b"IDAT" => compressed.extend_from_slice(body),
            b"IEND" => {
                ended = true;
                break;
            }
            _ => {}
        }
    }

    // A file without its end marker is a file that was cut short, even when
    // every pixel happens to have arrived: the alternative is a half-downloaded
    // picture that decodes and looks fine, which is the worst of the three
    // possible behaviours. Other decoders are lenient here. This one is not.
    if !ended {
        return Err(Trouble::Truncated);
    }

    let header = header.ok_or(Trouble::Malformed)?;
    if header.interlaced {
        return Err(Trouble::Unsupported("interlacing"));
    }
    let count = pixel_count(header.width, header.height, most_pixels)?;

    let samples = header.samples()?;
    let bits_per_pixel = samples * header.depth as usize;
    let stride = (header.width as usize * bits_per_pixel).div_ceil(8);
    // Every row carries one extra byte saying how it was filtered.
    let expected = (stride + 1)
        .checked_mul(header.height as usize)
        .ok_or(Trouble::TooLarge)?;

    let raw = nexus_inflate::zlib(&compressed, expected).map_err(Trouble::Compressed)?;
    if raw.len() < expected {
        return Err(Trouble::Truncated);
    }

    let unfiltered = unfilter(&raw, stride, header.height as usize, bits_per_pixel)?;
    let pixels = to_pixels(
        &unfiltered,
        &header,
        stride,
        &palette,
        &transparency,
        behind,
        count,
    )?;

    Ok(Picture {
        width: header.width,
        height: header.height,
        pixels,
    })
}

/// What `IHDR` says.
struct Header {
    width: u32,
    height: u32,
    depth: u8,
    colour: u8,
    interlaced: bool,
}

impl Header {
    fn read(body: &[u8]) -> Result<Self, Trouble> {
        if body.len() < 13 {
            return Err(Trouble::Malformed);
        }
        let width = u32::from_be_bytes([body[0], body[1], body[2], body[3]]);
        let height = u32::from_be_bytes([body[4], body[5], body[6], body[7]]);
        let depth = body[8];
        let colour = body[9];
        if body[10] != 0 || body[11] != 0 {
            return Err(Trouble::Unsupported(
                "a compression or filter method that is not zero",
            ));
        }
        if !matches!(depth, 1 | 2 | 4 | 8 | 16) {
            return Err(Trouble::Malformed);
        }
        Ok(Self {
            width,
            height,
            depth,
            colour,
            interlaced: body[12] != 0,
        })
    }

    /// How many samples each pixel has.
    const fn samples(&self) -> Result<usize, Trouble> {
        Ok(match self.colour {
            0 => 1, // grey
            2 => 3, // red, green, blue
            3 => 1, // an index into the palette
            4 => 2, // grey and alpha
            6 => 4, // colour and alpha
            _ => return Err(Trouble::Malformed),
        })
    }
}

/// Undo the per-row filters PNG applies before compressing.
///
/// Each row names one of five filters and is decoded against the row above it
/// and the pixel to its left. This is where a PNG decoder is usually wrong, and
/// the reason is that "the pixel to the left" is *bytes per pixel* to the left,
/// rounded up to one for images narrower than a byte.
fn unfilter(
    raw: &[u8],
    stride: usize,
    height: usize,
    bits_per_pixel: usize,
) -> Result<Vec<u8>, Trouble> {
    let step = (bits_per_pixel / 8).max(1);
    let mut out = alloc::vec![0u8; stride * height];

    for row in 0..height {
        let filter = raw[row * (stride + 1)];
        let source = &raw[row * (stride + 1) + 1..row * (stride + 1) + 1 + stride];
        let (done, doing) = out.split_at_mut(row * stride);
        let above = if row == 0 {
            None
        } else {
            Some(&done[(row - 1) * stride..row * stride])
        };
        let line = &mut doing[..stride];

        for at in 0..stride {
            let left = if at >= step { line[at - step] } else { 0 };
            let up = above.map_or(0, |above| above[at]);
            let up_left = match above {
                Some(above) if at >= step => above[at - step],
                _ => 0,
            };
            let value = source[at];
            line[at] = match filter {
                0 => value,
                1 => value.wrapping_add(left),
                2 => value.wrapping_add(up),
                3 => value.wrapping_add(((left as u16 + up as u16) / 2) as u8),
                4 => value.wrapping_add(paeth(left, up, up_left)),
                _ => return Err(Trouble::Malformed),
            };
        }
    }

    Ok(out)
}

/// The Paeth predictor: whichever of the three neighbours is closest to their
/// linear combination.
const fn paeth(left: u8, up: u8, up_left: u8) -> u8 {
    let estimate = left as i16 + up as i16 - up_left as i16;
    let from_left = (estimate - left as i16).abs();
    let from_up = (estimate - up as i16).abs();
    let from_corner = (estimate - up_left as i16).abs();
    if from_left <= from_up && from_left <= from_corner {
        left
    } else if from_up <= from_corner {
        up
    } else {
        up_left
    }
}

/// Turn unfiltered rows into pixels.
fn to_pixels(
    rows: &[u8],
    header: &Header,
    stride: usize,
    palette: &[[u8; 3]],
    transparency: &[u8],
    behind: u32,
    count: usize,
) -> Result<Vec<u32>, Trouble> {
    let mut pixels = alloc::vec![0u32; count];
    let width = header.width as usize;
    let depth = header.depth as usize;

    for row in 0..header.height as usize {
        let line = &rows[row * stride..(row + 1) * stride];
        let mut reader = Samples::new(line, depth);
        for column in 0..width {
            let pixel = match header.colour {
                0 => {
                    let grey = reader.scaled()?;
                    over(grey, grey, grey, 255, behind)
                }
                2 => {
                    let red = reader.scaled()?;
                    let green = reader.scaled()?;
                    let blue = reader.scaled()?;
                    over(red, green, blue, 255, behind)
                }
                3 => {
                    let index = reader.raw()? as usize;
                    let entry = palette.get(index).ok_or(Trouble::Malformed)?;
                    // `tRNS` in a palettised image is one alpha per entry, and
                    // entries past its end are opaque.
                    let alpha = transparency.get(index).copied().unwrap_or(255);
                    over(entry[0], entry[1], entry[2], alpha, behind)
                }
                4 => {
                    let grey = reader.scaled()?;
                    let alpha = reader.scaled()?;
                    over(grey, grey, grey, alpha, behind)
                }
                6 => {
                    let red = reader.scaled()?;
                    let green = reader.scaled()?;
                    let blue = reader.scaled()?;
                    let alpha = reader.scaled()?;
                    over(red, green, blue, alpha, behind)
                }
                _ => return Err(Trouble::Malformed),
            };
            pixels[row * width + column] = pixel;
        }
    }

    Ok(pixels)
}

/// Reads samples of one, two, four, eight or sixteen bits out of a row.
struct Samples<'a> {
    bytes: &'a [u8],
    depth: usize,
    /// Bits consumed.
    at: usize,
}

impl<'a> Samples<'a> {
    const fn new(bytes: &'a [u8], depth: usize) -> Self {
        Self {
            bytes,
            depth,
            at: 0,
        }
    }

    /// The next sample as it is stored.
    fn raw(&mut self) -> Result<u16, Trouble> {
        let depth = self.depth;
        if depth == 16 {
            let byte = self.at / 8;
            let pair = self.bytes.get(byte..byte + 2).ok_or(Trouble::Truncated)?;
            self.at += 16;
            return Ok(u16::from_be_bytes([pair[0], pair[1]]));
        }
        let byte = *self.bytes.get(self.at / 8).ok_or(Trouble::Truncated)?;
        let offset = self.at % 8;
        // Most significant bits first, which is how PNG packs sub-byte samples.
        let shift = 8 - depth - offset;
        self.at += depth;
        Ok(u16::from((byte >> shift) & ((1u16 << depth) - 1) as u8))
    }

    /// The next sample, scaled to eight bits.
    ///
    /// A two-bit sample runs 0..=3 and has to become 0..=255, and the scaling
    /// that gets white right is `value * 255 / maximum` rather than a shift: a
    /// shift leaves the brightest value at 252 and a picture that should be
    /// white slightly grey.
    fn scaled(&mut self) -> Result<u8, Trouble> {
        let value = self.raw()?;
        Ok(match self.depth {
            16 => (value >> 8) as u8,
            8 => value as u8,
            depth => {
                let maximum = (1u32 << depth) - 1;
                ((u32::from(value) * 255 + maximum / 2) / maximum) as u8
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Decode one of the fixtures and say how far it is from what ffmpeg made
    /// of the same file.
    ///
    /// The fixtures are the only tests here that were not written by this
    /// project. `fixtures/tiny*.jpg` came out of ffmpeg's MJPEG encoder and
    /// `fixtures/tiny*.rgb` out of its decoder, which makes this the one test
    /// that can catch a decoder that is *consistently* wrong -- the failure a
    /// decoder checked against its own encoder cannot see, because both halves
    /// share the mistake.
    ///
    /// Returns (mean absolute error, worst absolute error) over every channel
    /// of every pixel.
    fn against_ffmpeg(jpeg: &[u8], reference: &[u8], width: usize, height: usize) -> (u32, u32) {
        let picture = decode(jpeg, 0x000000, MOST_PIXELS).expect("the fixture should decode");
        assert_eq!(picture.width as usize, width);
        assert_eq!(picture.height as usize, height);
        assert_eq!(reference.len(), width * height * 3);

        let mut total = 0u64;
        let mut worst = 0u32;
        for (index, pixel) in picture.pixels.iter().enumerate() {
            let mine = [
                ((pixel >> 16) & 0xFF) as i32,
                ((pixel >> 8) & 0xFF) as i32,
                (pixel & 0xFF) as i32,
            ];
            for channel in 0..3 {
                let theirs = i32::from(reference[index * 3 + channel]);
                let off = (mine[channel] - theirs).unsigned_abs();
                total += u64::from(off);
                worst = worst.max(off);
            }
        }
        ((total / (width * height * 3) as u64) as u32, worst)
    }

    #[test]
    fn a_444_jpeg_from_ffmpeg_decodes_to_what_ffmpeg_decodes_it_to() {
        // No chroma subsampling, so there is no upsampling rule to disagree
        // about: every difference here is the inverse transform or the colour
        // conversion, and both should be within a level or two of anyone's.
        let (mean, worst) = against_ffmpeg(
            include_bytes!("../fixtures/tiny444.jpg"),
            include_bytes!("../fixtures/tiny444.rgb"),
            64,
            48,
        );
        assert!(mean <= 1, "mean error {mean} against ffmpeg is too large");
        assert!(
            worst <= 8,
            "worst error {worst} against ffmpeg is too large"
        );
    }

    #[test]
    fn a_422_jpeg_from_ffmpeg_decodes_close_to_what_ffmpeg_decodes_it_to() {
        // Chroma is half width here. This decoder repeats the nearest sample
        // where ffmpeg may interpolate, which was expected to show up as large
        // differences either side of a sharp colour edge -- and does not, on
        // this content, at four levels out of two hundred and fifty-five. The
        // bound is deliberately not loosened to allow for a difference that
        // was not measured.
        let (mean, worst) = against_ffmpeg(
            include_bytes!("../fixtures/tiny422.jpg"),
            include_bytes!("../fixtures/tiny422.rgb"),
            64,
            48,
        );
        assert!(mean <= 1, "mean error {mean} against ffmpeg is too large");
        assert!(
            worst <= 8,
            "worst error {worst} against ffmpeg is too large"
        );
    }

    #[test]
    fn a_420_jpeg_from_ffmpeg_decodes_close_to_what_ffmpeg_decodes_it_to() {
        // Half width and half height. This is what a camera and every
        // Motion-JPEG stream produces, so it is the one that has to work.
        let (mean, worst) = against_ffmpeg(
            include_bytes!("../fixtures/tiny.jpg"),
            include_bytes!("../fixtures/tiny.rgb"),
            64,
            48,
        );
        assert!(mean <= 1, "mean error {mean} against ffmpeg is too large");
        assert!(
            worst <= 8,
            "worst error {worst} against ffmpeg is too large"
        );
    }

    /// A four-by-two bitmap, written by hand: red, green, blue, white on the
    /// top row and black, grey, red, red on the bottom. Bottom-up, as bitmaps
    /// are, so the file has the black row first.
    fn a_bitmap() -> Vec<u8> {
        let mut file = Vec::new();
        // Four pixels of three bytes: twelve, already a multiple of four, so
        // there is no row padding to write.
        let stride = 4 * 3;
        let data_at = 54u32;
        let size = data_at + (stride * 2) as u32;
        file.extend_from_slice(b"BM");
        file.extend_from_slice(&size.to_le_bytes());
        file.extend_from_slice(&0u32.to_le_bytes());
        file.extend_from_slice(&data_at.to_le_bytes());
        // BITMAPINFOHEADER
        file.extend_from_slice(&40u32.to_le_bytes());
        file.extend_from_slice(&4u32.to_le_bytes());
        file.extend_from_slice(&2u32.to_le_bytes());
        file.extend_from_slice(&1u16.to_le_bytes());
        file.extend_from_slice(&24u16.to_le_bytes());
        file.extend_from_slice(&0u32.to_le_bytes());
        file.extend_from_slice(&0u32.to_le_bytes());
        for _ in 0..4 {
            file.extend_from_slice(&0u32.to_le_bytes());
        }
        // Bottom row first: black, grey, red, red -- stored blue, green, red.
        for bgr in [[0, 0, 0], [128, 128, 128], [0, 0, 255], [0, 0, 255]] {
            file.extend_from_slice(&bgr);
        }
        // Then the top row: red, green, blue, white.
        for bgr in [[0, 0, 255], [0, 255, 0], [255, 0, 0], [255, 255, 255]] {
            file.extend_from_slice(&bgr);
        }
        file
    }

    #[test]
    fn a_bitmap_decodes_the_right_way_up() {
        let picture = bmp(&a_bitmap(), 1024).unwrap();
        assert_eq!((picture.width, picture.height), (4, 2));
        // The top row of the picture is the last row of the file.
        assert_eq!(picture.at(0, 0), Some(0xFF_0000));
        assert_eq!(picture.at(1, 0), Some(0x00_FF00));
        assert_eq!(picture.at(2, 0), Some(0x00_00FF));
        assert_eq!(picture.at(3, 0), Some(0xFF_FFFF));
        assert_eq!(picture.at(0, 1), Some(0x00_0000));
        assert_eq!(picture.at(1, 1), Some(0x80_8080));
        assert_eq!(picture.at(4, 0), None);
    }

    #[test]
    fn a_bitmap_this_does_not_read_is_named_rather_than_guessed() {
        let mut palettised = a_bitmap();
        palettised[28] = 8; // eight bits, which means a palette
        assert!(matches!(
            bmp(&palettised, 1024),
            Err(Trouble::Unsupported(_))
        ));

        let mut compressed = a_bitmap();
        compressed[30] = 1; // RLE8
        assert!(matches!(
            bmp(&compressed, 1024),
            Err(Trouble::Unsupported(_))
        ));
    }

    #[test]
    fn a_truncated_bitmap_says_so() {
        let whole = a_bitmap();
        assert_eq!(
            bmp(&whole[..whole.len() - 4], 1024),
            Err(Trouble::Truncated)
        );
        assert_eq!(bmp(&whole[..10], 1024), Err(Trouble::Truncated));
    }

    #[test]
    fn the_cap_is_a_cap() {
        assert_eq!(bmp(&a_bitmap(), 4), Err(Trouble::TooLarge));
    }

    #[test]
    fn the_kind_is_read_from_the_bytes() {
        assert_eq!(kind(&a_bitmap()), Some("bmp"));
        assert_eq!(kind(&PNG_MAGIC), Some("png"));
        assert_eq!(kind(&[0xFF, 0xD8, 0xFF, 0xE0]), Some("jpeg"));
        assert_eq!(kind(include_bytes!("../fixtures/clip.avi")), Some("avi"));
        assert_eq!(kind(b"not a picture"), None);
        assert_eq!(kind(b""), None);
        // RIFF is a family, not a format: a sound file starts the same way and
        // is not a recording this plays.
        assert_eq!(kind(b"RIFF    WAVEfmt "), None);
    }

    #[test]
    fn a_recording_is_named_but_not_decoded_as_a_picture() {
        // `kind` says what it is so a directory listing can show it. `decode`
        // refuses, because a film is not a picture and returning its first
        // frame would be this deciding on the caller's behalf.
        let clip = include_bytes!("../fixtures/clip.avi");
        assert_eq!(decode(clip, 0, MOST_PIXELS), Err(Trouble::NotAPicture));
    }

    #[test]
    fn a_jpeg_that_stops_after_its_marker_is_truncated_rather_than_guessed() {
        assert_eq!(
            decode(&[0xFF, 0xD8, 0xFF, 0xE0], 0, 1024),
            Err(Trouble::Truncated)
        );
    }

    #[test]
    fn alpha_is_flattened_onto_what_the_caller_asked_for() {
        // Half-transparent white over black is grey.
        assert_eq!(over(255, 255, 255, 128, 0x00_0000), 0x80_8080);
        // And fully transparent is the background.
        assert_eq!(over(255, 0, 0, 0, 0x12_3456), 0x12_3456);
        // Opaque ignores the background entirely.
        assert_eq!(over(1, 2, 3, 255, 0xFF_FFFF), 0x01_0203);
    }

    #[test]
    fn the_paeth_predictor_picks_the_nearest() {
        // Straight from the specification's own examples.
        assert_eq!(paeth(0, 0, 0), 0);
        assert_eq!(paeth(10, 20, 10), 20);
        assert_eq!(paeth(20, 10, 10), 20);
        assert_eq!(paeth(1, 2, 3), 1);
    }

    #[test]
    fn small_samples_scale_so_that_white_is_white() {
        // One bit: the bright value has to reach 255, which a shift would miss.
        let mut reader = Samples::new(&[0b1010_0000], 1);
        assert_eq!(reader.scaled(), Ok(255));
        assert_eq!(reader.scaled(), Ok(0));
        assert_eq!(reader.scaled(), Ok(255));

        let mut four = Samples::new(&[0xF0], 4);
        assert_eq!(four.scaled(), Ok(255));
        assert_eq!(four.scaled(), Ok(0));

        // And sixteen bits are taken from the top.
        let mut wide = Samples::new(&[0xAB, 0xCD], 16);
        assert_eq!(wide.scaled(), Ok(0xAB));
    }

    #[test]
    fn reading_past_the_end_of_a_row_is_an_error() {
        let mut reader = Samples::new(&[0x00], 8);
        assert_eq!(reader.raw(), Ok(0));
        assert_eq!(reader.raw(), Err(Trouble::Truncated));
    }

    const TRUECOLOUR: &[u8] = &[
        0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x04, 0x00, 0x00, 0x00, 0x02, 0x08, 0x02, 0x00, 0x00, 0x00, 0xf0,
        0xca, 0xea, 0x34, 0x00, 0x00, 0x00, 0x1d, 0x49, 0x44, 0x41, 0x54, 0x78, 0xda, 0x63, 0xf8,
        0xcf, 0xc0, 0xc0, 0x00, 0xc6, 0x40, 0xc0, 0xc2, 0xc8, 0xc0, 0xd0, 0xd0, 0xd8, 0x50, 0xcf,
        0xc0, 0xc8, 0xc0, 0xc8, 0x00, 0x00, 0x7a, 0xf6, 0x08, 0x02, 0xd9, 0xee, 0x8e, 0xd2, 0x00,
        0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
    ];

    const WITH_ALPHA: &[u8] = &[
        0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0xf4,
        0x22, 0x7f, 0x8a, 0x00, 0x00, 0x00, 0x11, 0x49, 0x44, 0x41, 0x54, 0x78, 0xda, 0x63, 0xf8,
        0xff, 0xff, 0x7f, 0xc3, 0x7f, 0x06, 0x86, 0xff, 0x00, 0x1c, 0x6f, 0x05, 0x7c, 0xf4, 0x53,
        0x9f, 0x32, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
    ];

    const PALETTE: &[u8] = &[
        0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x04, 0x00, 0x00, 0x00, 0x01, 0x04, 0x03, 0x00, 0x00, 0x00, 0x0b,
        0x12, 0x12, 0xfe, 0x00, 0x00, 0x00, 0x0c, 0x50, 0x4c, 0x54, 0x45, 0xff, 0x00, 0x00, 0x00,
        0xff, 0x00, 0x00, 0x00, 0xff, 0xff, 0xff, 0xff, 0xfb, 0x00, 0x60, 0xf6, 0x00, 0x00, 0x00,
        0x04, 0x74, 0x52, 0x4e, 0x53, 0xff, 0x00, 0xff, 0xff, 0xd3, 0xb0, 0x72, 0x94, 0x00, 0x00,
        0x00, 0x0b, 0x49, 0x44, 0x41, 0x54, 0x78, 0xda, 0x63, 0x60, 0x54, 0x06, 0x00, 0x00, 0x28,
        0x00, 0x25, 0xa9, 0x67, 0x62, 0x08, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae,
        0x42, 0x60, 0x82,
    ];

    const GREY16: &[u8] = &[
        0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00, 0x01, 0x10, 0x00, 0x00, 0x00, 0x00, 0x81,
        0xd9, 0xfc, 0x15, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x44, 0x41, 0x54, 0x78, 0xda, 0x63, 0x58,
        0x7d, 0x96, 0x81, 0x01, 0x00, 0x05, 0x18, 0x01, 0x79, 0x6f, 0x69, 0x5b, 0x2f, 0x00, 0x00,
        0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
    ];

    const SUB_FILTER: &[u8] = &[
        0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x04, 0x00, 0x00, 0x00, 0x01, 0x08, 0x00, 0x00, 0x00, 0x00, 0xdc,
        0x57, 0x50, 0x11, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x44, 0x41, 0x54, 0x78, 0xda, 0x63, 0xe4,
        0xe2, 0xe2, 0xe2, 0x02, 0x00, 0x00, 0x6e, 0x00, 0x2a, 0x76, 0x84, 0xd3, 0x14, 0x00, 0x00,
        0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
    ];

    /// Real PNG files, written by a reference encoder (Python's `zlib` and the
    /// chunk layout from the specification) rather than by this crate. A
    /// decoder tested only against its own encoder is a decoder that agrees
    /// with itself.
    #[test]
    fn a_truecolour_png_decodes() {
        let picture = png(TRUECOLOUR, 0, 1024).unwrap();
        assert_eq!((picture.width, picture.height), (4, 2));
        assert_eq!(picture.at(0, 0), Some(0xFF_0000));
        assert_eq!(picture.at(1, 0), Some(0x00_FF00));
        assert_eq!(picture.at(2, 0), Some(0x00_00FF));
        assert_eq!(picture.at(3, 0), Some(0xFF_FFFF));
        // The second row is Paeth-filtered against the first, so getting it
        // right is the whole of the filter arithmetic.
        assert_eq!(picture.at(0, 1), Some(0x00_0000));
        assert_eq!(picture.at(1, 1), Some(0x80_8080));
        assert_eq!(picture.at(2, 1), Some(0xFF_0000));
        assert_eq!(picture.at(3, 1), Some(0xFF_0000));
    }

    #[test]
    fn transparency_lands_on_the_background_the_caller_gave() {
        // Half-transparent white, then opaque red.
        let on_black = png(WITH_ALPHA, 0x00_0000, 1024).unwrap();
        assert_eq!(on_black.at(0, 0), Some(0x80_8080));
        assert_eq!(on_black.at(1, 0), Some(0xFF_0000));

        // The same file on a different background gives a different picture,
        // which is the point of taking one.
        let on_blue = png(WITH_ALPHA, 0x00_00FF, 1024).unwrap();
        assert_eq!(on_blue.at(0, 0), Some(0x80_80FF));
        assert_eq!(on_blue.at(1, 0), Some(0xFF_0000));
    }

    #[test]
    fn a_four_bit_palette_unpacks_two_pixels_to_the_byte() {
        let picture = png(PALETTE, 0x00_0000, 1024).unwrap();
        assert_eq!((picture.width, picture.height), (4, 1));
        assert_eq!(picture.at(0, 0), Some(0xFF_0000));
        // Entry one is transparent in `tRNS`, so it becomes the background.
        assert_eq!(picture.at(1, 0), Some(0x00_0000));
        assert_eq!(picture.at(2, 0), Some(0x00_00FF));
        assert_eq!(picture.at(3, 0), Some(0xFF_FFFF));
    }

    #[test]
    fn sixteen_bit_samples_are_taken_from_the_top() {
        let picture = png(GREY16, 0, 1024).unwrap();
        assert_eq!(picture.at(0, 0), Some(0xAB_ABAB));
        assert_eq!(picture.at(1, 0), Some(0x00_0000));
    }

    #[test]
    fn the_sub_filter_adds_the_pixel_to_its_left() {
        let picture = png(SUB_FILTER, 0, 1024).unwrap();
        for (column, grey) in [(0u32, 10u32), (1, 20), (2, 30), (3, 40)] {
            let expected = grey << 16 | grey << 8 | grey;
            assert_eq!(picture.at(column, 0), Some(expected), "column {column}");
        }
    }

    #[test]
    fn a_png_with_a_broken_checksum_is_refused() {
        let mut broken = TRUECOLOUR.to_vec();
        // A byte inside the IDAT chunk's compressed body: the file is eight
        // bytes of signature, a twenty-five byte IHDR, then IDAT, and IEND is
        // the last twelve.
        broken[50] ^= 0xFF;
        assert!(matches!(
            png(&broken, 0, 1024),
            Err(Trouble::Compressed(_)) | Err(Trouble::Truncated)
        ));
    }

    #[test]
    fn a_truncated_png_says_so_rather_than_guessing() {
        for cut in 9..TRUECOLOUR.len() - 1 {
            assert!(
                png(&TRUECOLOUR[..cut], 0, 1024).is_err(),
                "a file cut at {cut} produced a picture"
            );
        }
    }

    #[test]
    fn an_enormous_header_is_refused_before_anything_is_allocated() {
        let mut huge = TRUECOLOUR.to_vec();
        // Width and height in IHDR, which starts at byte 16.
        huge[16..20].copy_from_slice(&0xFFFF_FFFFu32.to_be_bytes());
        huge[20..24].copy_from_slice(&0xFFFF_FFFFu32.to_be_bytes());
        assert_eq!(png(&huge, 0, MOST_PIXELS), Err(Trouble::TooLarge));
    }

    const JPEG_QUADS: &[u8] = &[
        0xff, 0xd8, 0xff, 0xe0, 0x00, 0x10, 0x4a, 0x46, 0x49, 0x46, 0x00, 0x01, 0x01, 0x01, 0x00,
        0x60, 0x00, 0x60, 0x00, 0x00, 0xff, 0xdb, 0x00, 0x43, 0x00, 0x01, 0x01, 0x01, 0x01, 0x01,
        0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01,
        0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01,
        0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01,
        0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0xff,
        0xdb, 0x00, 0x43, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01,
        0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01,
        0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01,
        0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01,
        0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0xff, 0xc0, 0x00, 0x11, 0x08, 0x00, 0x10,
        0x00, 0x10, 0x03, 0x01, 0x22, 0x00, 0x02, 0x11, 0x01, 0x03, 0x11, 0x01, 0xff, 0xc4, 0x00,
        0x1f, 0x00, 0x00, 0x01, 0x05, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b,
        0xff, 0xc4, 0x00, 0xb5, 0x10, 0x00, 0x02, 0x01, 0x03, 0x03, 0x02, 0x04, 0x03, 0x05, 0x05,
        0x04, 0x04, 0x00, 0x00, 0x01, 0x7d, 0x01, 0x02, 0x03, 0x00, 0x04, 0x11, 0x05, 0x12, 0x21,
        0x31, 0x41, 0x06, 0x13, 0x51, 0x61, 0x07, 0x22, 0x71, 0x14, 0x32, 0x81, 0x91, 0xa1, 0x08,
        0x23, 0x42, 0xb1, 0xc1, 0x15, 0x52, 0xd1, 0xf0, 0x24, 0x33, 0x62, 0x72, 0x82, 0x09, 0x0a,
        0x16, 0x17, 0x18, 0x19, 0x1a, 0x25, 0x26, 0x27, 0x28, 0x29, 0x2a, 0x34, 0x35, 0x36, 0x37,
        0x38, 0x39, 0x3a, 0x43, 0x44, 0x45, 0x46, 0x47, 0x48, 0x49, 0x4a, 0x53, 0x54, 0x55, 0x56,
        0x57, 0x58, 0x59, 0x5a, 0x63, 0x64, 0x65, 0x66, 0x67, 0x68, 0x69, 0x6a, 0x73, 0x74, 0x75,
        0x76, 0x77, 0x78, 0x79, 0x7a, 0x83, 0x84, 0x85, 0x86, 0x87, 0x88, 0x89, 0x8a, 0x92, 0x93,
        0x94, 0x95, 0x96, 0x97, 0x98, 0x99, 0x9a, 0xa2, 0xa3, 0xa4, 0xa5, 0xa6, 0xa7, 0xa8, 0xa9,
        0xaa, 0xb2, 0xb3, 0xb4, 0xb5, 0xb6, 0xb7, 0xb8, 0xb9, 0xba, 0xc2, 0xc3, 0xc4, 0xc5, 0xc6,
        0xc7, 0xc8, 0xc9, 0xca, 0xd2, 0xd3, 0xd4, 0xd5, 0xd6, 0xd7, 0xd8, 0xd9, 0xda, 0xe1, 0xe2,
        0xe3, 0xe4, 0xe5, 0xe6, 0xe7, 0xe8, 0xe9, 0xea, 0xf1, 0xf2, 0xf3, 0xf4, 0xf5, 0xf6, 0xf7,
        0xf8, 0xf9, 0xfa, 0xff, 0xc4, 0x00, 0x1f, 0x01, 0x00, 0x03, 0x01, 0x01, 0x01, 0x01, 0x01,
        0x01, 0x01, 0x01, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x02, 0x03, 0x04, 0x05,
        0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0xff, 0xc4, 0x00, 0xb5, 0x11, 0x00, 0x02, 0x01, 0x02,
        0x04, 0x04, 0x03, 0x04, 0x07, 0x05, 0x04, 0x04, 0x00, 0x01, 0x02, 0x77, 0x00, 0x01, 0x02,
        0x03, 0x11, 0x04, 0x05, 0x21, 0x31, 0x06, 0x12, 0x41, 0x51, 0x07, 0x61, 0x71, 0x13, 0x22,
        0x32, 0x81, 0x08, 0x14, 0x42, 0x91, 0xa1, 0xb1, 0xc1, 0x09, 0x23, 0x33, 0x52, 0xf0, 0x15,
        0x62, 0x72, 0xd1, 0x0a, 0x16, 0x24, 0x34, 0xe1, 0x25, 0xf1, 0x17, 0x18, 0x19, 0x1a, 0x26,
        0x27, 0x28, 0x29, 0x2a, 0x35, 0x36, 0x37, 0x38, 0x39, 0x3a, 0x43, 0x44, 0x45, 0x46, 0x47,
        0x48, 0x49, 0x4a, 0x53, 0x54, 0x55, 0x56, 0x57, 0x58, 0x59, 0x5a, 0x63, 0x64, 0x65, 0x66,
        0x67, 0x68, 0x69, 0x6a, 0x73, 0x74, 0x75, 0x76, 0x77, 0x78, 0x79, 0x7a, 0x82, 0x83, 0x84,
        0x85, 0x86, 0x87, 0x88, 0x89, 0x8a, 0x92, 0x93, 0x94, 0x95, 0x96, 0x97, 0x98, 0x99, 0x9a,
        0xa2, 0xa3, 0xa4, 0xa5, 0xa6, 0xa7, 0xa8, 0xa9, 0xaa, 0xb2, 0xb3, 0xb4, 0xb5, 0xb6, 0xb7,
        0xb8, 0xb9, 0xba, 0xc2, 0xc3, 0xc4, 0xc5, 0xc6, 0xc7, 0xc8, 0xc9, 0xca, 0xd2, 0xd3, 0xd4,
        0xd5, 0xd6, 0xd7, 0xd8, 0xd9, 0xda, 0xe2, 0xe3, 0xe4, 0xe5, 0xe6, 0xe7, 0xe8, 0xe9, 0xea,
        0xf2, 0xf3, 0xf4, 0xf5, 0xf6, 0xf7, 0xf8, 0xf9, 0xfa, 0xff, 0xda, 0x00, 0x0c, 0x03, 0x01,
        0x00, 0x02, 0x11, 0x03, 0x11, 0x00, 0x3f, 0x00, 0xfc, 0x5f, 0xaf, 0xe9, 0x42, 0xbf, 0x83,
        0x7a, 0xff, 0x00, 0x71, 0x0a, 0x3f, 0x69, 0x87, 0xec, 0x33, 0xff, 0x00, 0x89, 0x72, 0xff,
        0x00, 0x88, 0x29, 0xff, 0x00, 0x1d, 0x45, 0xfe, 0xb9, 0x7f, 0xae, 0x5f, 0xf1, 0x12, 0x3f,
        0xe6, 0xc9, 0xff, 0x00, 0xab, 0xdf, 0xd9, 0xdf, 0xea, 0xf7, 0xfa, 0x85, 0xff, 0x00, 0x57,
        0x73, 0x3c, 0xfa, 0xe7, 0xd7, 0x3f, 0xb7, 0x3f, 0xea, 0x17, 0xea, 0xff, 0x00, 0x55, 0xff,
        0x00, 0x97, 0xfe, 0xdf, 0xf7, 0x33, 0xfb, 0x60, 0xbc, 0x4a, 0xff, 0x00, 0x8a, 0xd1, 0xff,
        0x00, 0xc4, 0xbb, 0xff, 0x00, 0xc2, 0x2f, 0xfc, 0x4b, 0x67, 0xfc, 0x4b, 0x67, 0xfc, 0x45,
        0xbf, 0xf9, 0x99, 0x7f, 0xc4, 0x62, 0xff, 0x00, 0x5d, 0x3f, 0xe2, 0x31, 0x7f, 0xc4, 0x32,
        0xff, 0x00, 0xa8, 0x0f, 0x0b, 0x3f, 0xd5, 0xcf, 0xf5, 0x73, 0xfe, 0x21, 0x67, 0xfd, 0x4f,
        0x7f, 0xb5, 0xff, 0x00, 0xb7, 0x7f, 0xe6, 0x57, 0xfd, 0x97, 0xff, 0x00, 0x0a, 0x3f, 0xff,
        0xd9,
    ];

    const JPEG_GREY: &[u8] = &[
        0xff, 0xd8, 0xff, 0xe0, 0x00, 0x10, 0x4a, 0x46, 0x49, 0x46, 0x00, 0x01, 0x01, 0x01, 0x00,
        0x60, 0x00, 0x60, 0x00, 0x00, 0xff, 0xdb, 0x00, 0x43, 0x00, 0x02, 0x01, 0x01, 0x02, 0x01,
        0x01, 0x02, 0x02, 0x02, 0x02, 0x02, 0x02, 0x02, 0x02, 0x03, 0x05, 0x03, 0x03, 0x03, 0x03,
        0x03, 0x06, 0x04, 0x04, 0x03, 0x05, 0x07, 0x06, 0x07, 0x07, 0x07, 0x06, 0x07, 0x07, 0x08,
        0x09, 0x0b, 0x09, 0x08, 0x08, 0x0a, 0x08, 0x07, 0x07, 0x0a, 0x0d, 0x0a, 0x0a, 0x0b, 0x0c,
        0x0c, 0x0c, 0x0c, 0x07, 0x09, 0x0e, 0x0f, 0x0d, 0x0c, 0x0e, 0x0b, 0x0c, 0x0c, 0x0c, 0xff,
        0xdb, 0x00, 0x43, 0x01, 0x02, 0x02, 0x02, 0x03, 0x03, 0x03, 0x06, 0x03, 0x03, 0x06, 0x0c,
        0x08, 0x07, 0x08, 0x0c, 0x0c, 0x0c, 0x0c, 0x0c, 0x0c, 0x0c, 0x0c, 0x0c, 0x0c, 0x0c, 0x0c,
        0x0c, 0x0c, 0x0c, 0x0c, 0x0c, 0x0c, 0x0c, 0x0c, 0x0c, 0x0c, 0x0c, 0x0c, 0x0c, 0x0c, 0x0c,
        0x0c, 0x0c, 0x0c, 0x0c, 0x0c, 0x0c, 0x0c, 0x0c, 0x0c, 0x0c, 0x0c, 0x0c, 0x0c, 0x0c, 0x0c,
        0x0c, 0x0c, 0x0c, 0x0c, 0x0c, 0x0c, 0x0c, 0x0c, 0xff, 0xc0, 0x00, 0x11, 0x08, 0x00, 0x08,
        0x00, 0x20, 0x03, 0x01, 0x22, 0x00, 0x02, 0x11, 0x01, 0x03, 0x11, 0x01, 0xff, 0xc4, 0x00,
        0x1f, 0x00, 0x00, 0x01, 0x05, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b,
        0xff, 0xc4, 0x00, 0xb5, 0x10, 0x00, 0x02, 0x01, 0x03, 0x03, 0x02, 0x04, 0x03, 0x05, 0x05,
        0x04, 0x04, 0x00, 0x00, 0x01, 0x7d, 0x01, 0x02, 0x03, 0x00, 0x04, 0x11, 0x05, 0x12, 0x21,
        0x31, 0x41, 0x06, 0x13, 0x51, 0x61, 0x07, 0x22, 0x71, 0x14, 0x32, 0x81, 0x91, 0xa1, 0x08,
        0x23, 0x42, 0xb1, 0xc1, 0x15, 0x52, 0xd1, 0xf0, 0x24, 0x33, 0x62, 0x72, 0x82, 0x09, 0x0a,
        0x16, 0x17, 0x18, 0x19, 0x1a, 0x25, 0x26, 0x27, 0x28, 0x29, 0x2a, 0x34, 0x35, 0x36, 0x37,
        0x38, 0x39, 0x3a, 0x43, 0x44, 0x45, 0x46, 0x47, 0x48, 0x49, 0x4a, 0x53, 0x54, 0x55, 0x56,
        0x57, 0x58, 0x59, 0x5a, 0x63, 0x64, 0x65, 0x66, 0x67, 0x68, 0x69, 0x6a, 0x73, 0x74, 0x75,
        0x76, 0x77, 0x78, 0x79, 0x7a, 0x83, 0x84, 0x85, 0x86, 0x87, 0x88, 0x89, 0x8a, 0x92, 0x93,
        0x94, 0x95, 0x96, 0x97, 0x98, 0x99, 0x9a, 0xa2, 0xa3, 0xa4, 0xa5, 0xa6, 0xa7, 0xa8, 0xa9,
        0xaa, 0xb2, 0xb3, 0xb4, 0xb5, 0xb6, 0xb7, 0xb8, 0xb9, 0xba, 0xc2, 0xc3, 0xc4, 0xc5, 0xc6,
        0xc7, 0xc8, 0xc9, 0xca, 0xd2, 0xd3, 0xd4, 0xd5, 0xd6, 0xd7, 0xd8, 0xd9, 0xda, 0xe1, 0xe2,
        0xe3, 0xe4, 0xe5, 0xe6, 0xe7, 0xe8, 0xe9, 0xea, 0xf1, 0xf2, 0xf3, 0xf4, 0xf5, 0xf6, 0xf7,
        0xf8, 0xf9, 0xfa, 0xff, 0xc4, 0x00, 0x1f, 0x01, 0x00, 0x03, 0x01, 0x01, 0x01, 0x01, 0x01,
        0x01, 0x01, 0x01, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x02, 0x03, 0x04, 0x05,
        0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0xff, 0xc4, 0x00, 0xb5, 0x11, 0x00, 0x02, 0x01, 0x02,
        0x04, 0x04, 0x03, 0x04, 0x07, 0x05, 0x04, 0x04, 0x00, 0x01, 0x02, 0x77, 0x00, 0x01, 0x02,
        0x03, 0x11, 0x04, 0x05, 0x21, 0x31, 0x06, 0x12, 0x41, 0x51, 0x07, 0x61, 0x71, 0x13, 0x22,
        0x32, 0x81, 0x08, 0x14, 0x42, 0x91, 0xa1, 0xb1, 0xc1, 0x09, 0x23, 0x33, 0x52, 0xf0, 0x15,
        0x62, 0x72, 0xd1, 0x0a, 0x16, 0x24, 0x34, 0xe1, 0x25, 0xf1, 0x17, 0x18, 0x19, 0x1a, 0x26,
        0x27, 0x28, 0x29, 0x2a, 0x35, 0x36, 0x37, 0x38, 0x39, 0x3a, 0x43, 0x44, 0x45, 0x46, 0x47,
        0x48, 0x49, 0x4a, 0x53, 0x54, 0x55, 0x56, 0x57, 0x58, 0x59, 0x5a, 0x63, 0x64, 0x65, 0x66,
        0x67, 0x68, 0x69, 0x6a, 0x73, 0x74, 0x75, 0x76, 0x77, 0x78, 0x79, 0x7a, 0x82, 0x83, 0x84,
        0x85, 0x86, 0x87, 0x88, 0x89, 0x8a, 0x92, 0x93, 0x94, 0x95, 0x96, 0x97, 0x98, 0x99, 0x9a,
        0xa2, 0xa3, 0xa4, 0xa5, 0xa6, 0xa7, 0xa8, 0xa9, 0xaa, 0xb2, 0xb3, 0xb4, 0xb5, 0xb6, 0xb7,
        0xb8, 0xb9, 0xba, 0xc2, 0xc3, 0xc4, 0xc5, 0xc6, 0xc7, 0xc8, 0xc9, 0xca, 0xd2, 0xd3, 0xd4,
        0xd5, 0xd6, 0xd7, 0xd8, 0xd9, 0xda, 0xe2, 0xe3, 0xe4, 0xe5, 0xe6, 0xe7, 0xe8, 0xe9, 0xea,
        0xf2, 0xf3, 0xf4, 0xf5, 0xf6, 0xf7, 0xf8, 0xf9, 0xfa, 0xff, 0xda, 0x00, 0x0c, 0x03, 0x01,
        0x00, 0x02, 0x11, 0x03, 0x11, 0x00, 0x3f, 0x00, 0xfc, 0x72, 0xfd, 0x9a, 0x3f, 0xe5, 0xdf,
        0xea, 0x2b, 0xf4, 0x1f, 0xf6, 0x69, 0xff, 0x00, 0x96, 0x1f, 0x85, 0x14, 0x50, 0x07, 0xe8,
        0x47, 0xec, 0xd3, 0xff, 0x00, 0x2c, 0x3f, 0x0a, 0xfd, 0x07, 0xfd, 0x9a, 0x3f, 0xe5, 0xdf,
        0xe8, 0x28, 0xa2, 0x80, 0x3f, 0xff, 0xd9,
    ];

    /// How far a decoded sample may be from the original before it is wrong.
    ///
    /// JPEG is lossy, so an exact comparison would be a test of one encoder's
    /// rounding. Twenty-four levels out of 255 is loose enough that any correct
    /// decoder passes and tight enough that a wrong transform, a swapped chroma
    /// channel or a mis-ordered zigzag does not: each of those moves a sample by
    /// far more than that, and two of them move it to the wrong colour entirely.
    const NEAR: i32 = 24;

    fn close(got: Option<u32>, want: u32, what: &str) {
        let got = got.unwrap_or_else(|| panic!("{what}: nothing there"));
        for shift in [16, 8, 0] {
            let a = ((got >> shift) & 0xFF) as i32;
            let b = ((want >> shift) & 0xFF) as i32;
            assert!(
                (a - b).abs() <= NEAR,
                "{what}: got {got:06x}, wanted about {want:06x}"
            );
        }
    }

    /// Four flat quadrants, which is the shortest thing that catches a decoder
    /// with its colour channels crossed: red and blue swapped looks perfect on
    /// grey and obviously wrong here.
    #[test]
    fn a_colour_jpeg_decodes_to_about_the_right_colours() {
        let picture = jpeg::decode(JPEG_QUADS, 4096).unwrap();
        assert_eq!((picture.width, picture.height), (16, 16));
        close(picture.at(3, 3), 0xFF_0000, "top left should be red");
        close(picture.at(12, 3), 0x00_FF00, "top right should be green");
        close(picture.at(3, 12), 0x00_00FF, "bottom left should be blue");
        close(
            picture.at(12, 12),
            0xFF_FFFF,
            "bottom right should be white",
        );
    }

    /// A ramp, which catches a transform that is right at the corners and wrong
    /// in between -- a flat-block shortcut applied where it should not be, or a
    /// shift by the wrong number of bits.
    #[test]
    fn a_greyscale_jpeg_decodes_as_a_ramp() {
        let picture = jpeg::decode(JPEG_GREY, 4096).unwrap();
        assert_eq!((picture.width, picture.height), (32, 8));
        for x in [0u32, 8, 16, 24, 31] {
            let expected = x * 255 / 31;
            let grey = expected << 16 | expected << 8 | expected;
            close(picture.at(x, 4), grey, "the ramp");
        }
        // And it really is a ramp rather than a flat field.
        let left = picture.at(0, 4).unwrap() & 0xFF;
        let right = picture.at(31, 4).unwrap() & 0xFF;
        assert!(right > left + 200, "the ramp is flat: {left} to {right}");
    }

    #[test]
    fn progressive_jpeg_is_refused_by_name_rather_than_decoded_as_noise() {
        // The same file with its baseline marker changed to the progressive one.
        let mut progressive = JPEG_QUADS.to_vec();
        let at = progressive
            .windows(2)
            .position(|pair| pair == [0xFF, 0xC0])
            .expect("the fixture has a baseline frame header");
        progressive[at + 1] = 0xC2;
        assert_eq!(
            jpeg::decode(&progressive, 4096),
            Err(Trouble::Unsupported("progressive coding"))
        );
    }

    #[test]
    fn a_truncated_jpeg_says_so() {
        for cut in [4usize, 20, 100, JPEG_QUADS.len() - 4] {
            assert!(
                jpeg::decode(&JPEG_QUADS[..cut], 4096).is_err(),
                "a file cut at {cut} produced a picture"
            );
        }
    }

    #[test]
    fn the_cap_applies_to_jpeg_too() {
        assert_eq!(jpeg::decode(JPEG_QUADS, 4), Err(Trouble::TooLarge));
    }

    #[test]
    fn rubbish_is_not_a_picture() {
        assert_eq!(decode(b"", 0, 1024), Err(Trouble::NotAPicture));
        assert_eq!(decode(b"hello", 0, 1024), Err(Trouble::NotAPicture));
        assert_eq!(png(b"hello", 0, 1024), Err(Trouble::NotAPicture));
    }
}
