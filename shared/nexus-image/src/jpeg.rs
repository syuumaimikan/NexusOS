//! Baseline JPEG, decoded.
//!
//! The format most photographs are in, and the one this crate refused by name
//! until now. It is a bigger job than PNG and a different shape: PNG is a
//! container around DEFLATE and a row filter, both exactly reversible; JPEG is
//! a Huffman code over quantised frequency coefficients, and getting a picture
//! back out means undoing a cosine transform.
//!
//! # What is here
//!
//! Baseline sequential: greyscale and YCbCr, with 4:4:4, 4:2:2 and 4:2:0
//! chroma sampling, and restart markers. That is what every camera, every
//! screenshot tool and every Motion-JPEG stream produces.
//!
//! **Progressive JPEG is refused by name.** It is the same coefficients sent in
//! several passes, which needs the whole coefficient array held and refined
//! rather than one block decoded and finished, and it is a second decoder
//! wearing the first one's clothes. Arithmetic coding and twelve-bit samples
//! are refused for the same reason: real, rare, and not worth guessing at.
//!
//! # No floating point
//!
//! The inverse transform is integer: a table of cosines scaled by 2^11, applied
//! down the columns and then across the rows. This system's kernel does not
//! enable the FPU, so a decoder that needed one could not run here — and the
//! integer form is what the reference implementations use anyway, because it
//! gives the same answer on every machine.
//!
//! The table is the definition rather than a factored butterfly. The first
//! version of this file was factored, written from memory, and decoded every
//! photograph to a flat grey; the table is slower and is obviously the thing
//! the specification says, which is what matters first.

use alloc::vec::Vec;

use crate::Trouble;

/// The most components a frame may have.
///
/// Three: greyscale is one and colour is three. CMYK JPEGs have four and are a
/// print format; they are refused rather than rendered wrongly.
const MAX_COMPONENTS: usize = 3;

/// One 8x8 block's worth of coefficients.
const BLOCK: usize = 64;

/// The order coefficients are stored in: out from the top-left corner.
///
/// Low frequencies first, which is what makes the trailing zeros of a quantised
/// block run together and compress.
const ZIGZAG: [usize; BLOCK] = [
    0, 1, 8, 16, 9, 2, 3, 10, 17, 24, 32, 25, 18, 11, 4, 5, 12, 19, 26, 33, 40, 48, 41, 34, 27, 20,
    13, 6, 7, 14, 21, 28, 35, 42, 49, 56, 57, 50, 43, 36, 29, 22, 15, 23, 30, 37, 44, 51, 58, 59,
    52, 45, 38, 31, 39, 46, 53, 60, 61, 54, 47, 55, 62, 63,
];

/// A Huffman table, as JPEG describes one: how many codes of each length, then
/// the values in order.
#[derive(Clone)]
struct Huffman {
    /// The smallest code of each length, and the index its value sits at.
    /// Indexed by length - 1.
    smallest: [i32; 16],
    index: [i32; 16],
    /// Largest code of each length, or -1 when there are none.
    largest: [i32; 16],
    values: Vec<u8>,
}

impl Huffman {
    fn new(counts: &[u8; 16], values: Vec<u8>) -> Self {
        let mut smallest = [0i32; 16];
        let mut largest = [-1i32; 16];
        let mut index = [0i32; 16];

        let mut code = 0i32;
        let mut at = 0i32;
        for length in 0..16 {
            let count = i32::from(counts[length]);
            smallest[length] = code;
            index[length] = at;
            if count > 0 {
                largest[length] = code + count - 1;
            } else {
                largest[length] = -1;
            }
            code = (code + count) << 1;
            at += count;
        }

        Self {
            smallest,
            index,
            largest,
            values,
        }
    }

    /// Read one value, a bit at a time.
    fn decode(&self, bits: &mut Bits<'_>) -> Result<u8, Trouble> {
        let mut code = 0i32;
        for length in 0..16 {
            code = (code << 1) | i32::from(bits.bit()?);
            if self.largest[length] >= code && code >= self.smallest[length] {
                let at = (self.index[length] + code - self.smallest[length]) as usize;
                return self.values.get(at).copied().ok_or(Trouble::Malformed);
            }
        }
        Err(Trouble::Malformed)
    }
}

/// The entropy-coded data, read one bit at a time, most significant first.
struct Bits<'a> {
    bytes: &'a [u8],
    at: usize,
    /// The byte being read, and how many of its bits are left.
    current: u8,
    left: u8,
}

impl<'a> Bits<'a> {
    const fn new(bytes: &'a [u8]) -> Self {
        Self {
            bytes,
            at: 0,
            current: 0,
            left: 0,
        }
    }

    /// The next bit.
    ///
    /// A `0xFF` in the stream is followed by a `0x00` that is not data: it is
    /// there so that no marker can appear by accident inside the coefficients.
    /// Anything else after `0xFF` is a marker, and a marker here means the scan
    /// has ended — which is not an error, so the reader answers zeros and lets
    /// the caller stop on its own count of blocks.
    fn bit(&mut self) -> Result<u8, Trouble> {
        if self.left == 0 {
            let Some(byte) = self.bytes.get(self.at).copied() else {
                return Err(Trouble::Truncated);
            };
            self.at += 1;
            if byte == 0xFF {
                match self.bytes.get(self.at).copied() {
                    Some(0x00) => self.at += 1,
                    Some(_) | None => {
                        // A marker. Step back so the caller can find it.
                        self.at -= 1;
                        return Err(Trouble::Truncated);
                    }
                }
            }
            self.current = byte;
            self.left = 8;
        }
        self.left -= 1;
        Ok((self.current >> self.left) & 1)
    }

    /// `count` bits as a number.
    fn take(&mut self, count: u8) -> Result<i32, Trouble> {
        let mut value = 0i32;
        for _ in 0..count {
            value = (value << 1) | i32::from(self.bit()?);
        }
        Ok(value)
    }

    /// Throw away the rest of the byte, and skip a restart marker if one is
    /// next. Returns whether one was found.
    fn restart(&mut self) -> bool {
        self.left = 0;
        // Restart markers are FFD0..FFD7.
        while self.at + 1 < self.bytes.len() {
            if self.bytes[self.at] == 0xFF {
                let marker = self.bytes[self.at + 1];
                if (0xD0..=0xD7).contains(&marker) {
                    self.at += 2;
                    return true;
                }
                return false;
            }
            // Padding before the marker, which some encoders emit.
            self.at += 1;
        }
        false
    }
}

/// Turn the `size` bits just read into the signed value they stand for.
///
/// JPEG stores a magnitude category and then that many bits. A leading zero
/// means the value is negative, and the arithmetic below is the standard way of
/// saying so without a branch per bit.
const fn extend(value: i32, size: u8) -> i32 {
    if size == 0 {
        return 0;
    }
    if value < (1 << (size - 1)) {
        value - (1 << size) + 1
    } else {
        value
    }
}

/// One colour channel of the frame.
struct Component {
    id: u8,
    /// How many blocks of this component are in one MCU, across and down.
    across: usize,
    down: usize,
    quantiser: usize,
    /// Which Huffman tables the scan chose for it.
    dc_table: usize,
    ac_table: usize,
    /// The running DC value, which each block stores as a difference from the
    /// last one in the same component.
    previous_dc: i32,
    /// The decoded samples, one byte each, `width` by `height` for this
    /// component before upsampling.
    samples: Vec<u8>,
    width: usize,
    height: usize,
}

/// Decode a baseline JPEG.
pub fn decode(bytes: &[u8], most_pixels: usize) -> Result<crate::Picture, Trouble> {
    if bytes.len() < 4 || bytes[0] != 0xFF || bytes[1] != 0xD8 {
        return Err(Trouble::NotAPicture);
    }

    let mut quantisers = [[0u16; BLOCK]; 4];
    let mut dc_tables: [Option<Huffman>; 4] = [None, None, None, None];
    let mut ac_tables: [Option<Huffman>; 4] = [None, None, None, None];
    let mut components: Vec<Component> = Vec::new();
    let mut width = 0usize;
    let mut height = 0usize;
    let mut restart_interval = 0usize;

    let mut at = 2usize;
    loop {
        // Markers are 0xFF followed by the kind. Padding bytes of 0xFF between
        // segments are legal and skipped.
        while bytes.get(at) == Some(&0xFF) && bytes.get(at + 1) == Some(&0xFF) {
            at += 1;
        }
        let (Some(0xFF), Some(&marker)) = (bytes.get(at), bytes.get(at + 1)) else {
            return Err(Trouble::Truncated);
        };
        at += 2;

        match marker {
            // Start of frame, baseline.
            0xC0 | 0xC1 => {
                let body = segment(bytes, &mut at)?;
                if body.len() < 6 {
                    return Err(Trouble::Malformed);
                }
                if body[0] != 8 {
                    return Err(Trouble::Unsupported("samples that are not eight bits"));
                }
                height = usize::from(u16::from_be_bytes([body[1], body[2]]));
                width = usize::from(u16::from_be_bytes([body[3], body[4]]));
                let count = usize::from(body[5]);
                if count == 0 || count > MAX_COMPONENTS {
                    return Err(Trouble::Unsupported(
                        "a colour model that is not grey or YCbCr",
                    ));
                }
                if body.len() < 6 + count * 3 {
                    return Err(Trouble::Malformed);
                }
                for index in 0..count {
                    let part = &body[6 + index * 3..9 + index * 3];
                    let across = usize::from(part[1] >> 4);
                    let down = usize::from(part[1] & 0x0F);
                    if across == 0 || down == 0 || across > 4 || down > 4 {
                        return Err(Trouble::Malformed);
                    }
                    components.push(Component {
                        id: part[0],
                        across,
                        down,
                        quantiser: usize::from(part[2]),
                        dc_table: 0,
                        ac_table: 0,
                        previous_dc: 0,
                        samples: Vec::new(),
                        width: 0,
                        height: 0,
                    });
                }
            }
            // Every other kind of frame.
            0xC2 => return Err(Trouble::Unsupported("progressive coding")),
            0xC3 | 0xC5..=0xC7 | 0xC9..=0xCB | 0xCD..=0xCF => {
                return Err(Trouble::Unsupported("a coding this decoder does not read"));
            }
            // Huffman tables.
            0xC4 => {
                let body = segment(bytes, &mut at)?;
                read_huffman(body, &mut dc_tables, &mut ac_tables)?;
            }
            // Quantisation tables.
            0xDB => {
                let body = segment(bytes, &mut at)?;
                read_quantisers(body, &mut quantisers)?;
            }
            // Restart interval.
            0xDD => {
                let body = segment(bytes, &mut at)?;
                if body.len() < 2 {
                    return Err(Trouble::Malformed);
                }
                restart_interval = usize::from(u16::from_be_bytes([body[0], body[1]]));
            }
            // Start of scan: everything after this is coefficients.
            0xDA => {
                let body = segment(bytes, &mut at)?;
                read_scan_header(body, &mut components)?;
                return scan(
                    &bytes[at..],
                    &mut components,
                    &quantisers,
                    &dc_tables,
                    &ac_tables,
                    width,
                    height,
                    restart_interval,
                    most_pixels,
                );
            }
            0xD9 => return Err(Trouble::Truncated),
            // Anything else with a length: application data, comments.
            _ => {
                segment(bytes, &mut at)?;
            }
        }
    }
}

/// The body of the segment at `at`, stepping past it.
fn segment<'a>(bytes: &'a [u8], at: &mut usize) -> Result<&'a [u8], Trouble> {
    let Some(header) = bytes.get(*at..*at + 2) else {
        return Err(Trouble::Truncated);
    };
    let length = usize::from(u16::from_be_bytes([header[0], header[1]]));
    if length < 2 {
        return Err(Trouble::Malformed);
    }
    let body = bytes.get(*at + 2..*at + length).ok_or(Trouble::Truncated)?;
    *at += length;
    Ok(body)
}

/// One `DQT` segment, which may carry several tables.
fn read_quantisers(mut body: &[u8], into: &mut [[u16; BLOCK]; 4]) -> Result<(), Trouble> {
    while !body.is_empty() {
        let head = body[0];
        let precision = head >> 4;
        let slot = usize::from(head & 0x0F);
        if slot >= into.len() {
            return Err(Trouble::Malformed);
        }
        body = &body[1..];
        match precision {
            0 => {
                let values = body.get(..BLOCK).ok_or(Trouble::Truncated)?;
                for (index, value) in values.iter().enumerate() {
                    into[slot][ZIGZAG[index]] = u16::from(*value);
                }
                body = &body[BLOCK..];
            }
            1 => {
                let values = body.get(..BLOCK * 2).ok_or(Trouble::Truncated)?;
                for index in 0..BLOCK {
                    into[slot][ZIGZAG[index]] =
                        u16::from_be_bytes([values[index * 2], values[index * 2 + 1]]);
                }
                body = &body[BLOCK * 2..];
            }
            _ => return Err(Trouble::Malformed),
        }
    }
    Ok(())
}

/// One `DHT` segment, which may carry several tables.
fn read_huffman(
    mut body: &[u8],
    dc: &mut [Option<Huffman>; 4],
    ac: &mut [Option<Huffman>; 4],
) -> Result<(), Trouble> {
    while !body.is_empty() {
        let head = body[0];
        let is_ac = head >> 4 == 1;
        let slot = usize::from(head & 0x0F);
        if slot >= 4 {
            return Err(Trouble::Malformed);
        }
        let counts_slice = body.get(1..17).ok_or(Trouble::Truncated)?;
        let mut counts = [0u8; 16];
        counts.copy_from_slice(counts_slice);
        let total: usize = counts.iter().map(|count| usize::from(*count)).sum();
        let values = body.get(17..17 + total).ok_or(Trouble::Truncated)?.to_vec();
        body = &body[17 + total..];

        let table = Huffman::new(&counts, values);
        if is_ac {
            ac[slot] = Some(table);
        } else {
            dc[slot] = Some(table);
        }
    }
    Ok(())
}

/// The `SOS` header: which tables each component uses in this scan.
fn read_scan_header(body: &[u8], components: &mut [Component]) -> Result<(), Trouble> {
    if body.is_empty() {
        return Err(Trouble::Malformed);
    }
    let count = usize::from(body[0]);
    if count != components.len() {
        return Err(Trouble::Unsupported("a scan over some of the components"));
    }
    if body.len() < 1 + count * 2 + 3 {
        return Err(Trouble::Malformed);
    }
    for index in 0..count {
        let id = body[1 + index * 2];
        let tables = body[2 + index * 2];
        let component = components
            .iter_mut()
            .find(|component| component.id == id)
            .ok_or(Trouble::Malformed)?;
        component.dc_table = usize::from(tables >> 4);
        component.ac_table = usize::from(tables & 0x0F);
        if component.dc_table >= 4 || component.ac_table >= 4 {
            return Err(Trouble::Malformed);
        }
    }
    // The three bytes after are the spectral range and the successive
    // approximation, which only progressive scans use. A baseline scan says
    // 0, 63, 0 -- anything else is a progressive scan that said it was
    // baseline, and is refused rather than decoded as noise.
    let tail = &body[1 + count * 2..];
    if tail[0] != 0 || tail[1] != 63 || tail[2] != 0 {
        return Err(Trouble::Unsupported("a partial spectral scan"));
    }
    Ok(())
}

/// Decode the entropy-coded data and build the picture.
#[allow(clippy::too_many_arguments)]
fn scan(
    data: &[u8],
    components: &mut [Component],
    quantisers: &[[u16; BLOCK]; 4],
    dc_tables: &[Option<Huffman>; 4],
    ac_tables: &[Option<Huffman>; 4],
    width: usize,
    height: usize,
    restart_interval: usize,
    most_pixels: usize,
) -> Result<crate::Picture, Trouble> {
    if width == 0 || height == 0 || components.is_empty() {
        return Err(Trouble::Malformed);
    }
    let count = crate::pixel_count(width as u32, height as u32, most_pixels)?;

    // The MCU is the block of pixels one pass of every component covers. Its
    // size comes from the largest sampling factor: a 4:2:0 image has a
    // luminance factor of two in each direction, so one MCU is sixteen pixels
    // square and carries four luminance blocks and one of each chroma.
    let widest = components.iter().map(|part| part.across).max().unwrap_or(1);
    let tallest = components.iter().map(|part| part.down).max().unwrap_or(1);
    let mcu_width = widest * 8;
    let mcu_height = tallest * 8;
    let across = width.div_ceil(mcu_width);
    let down = height.div_ceil(mcu_height);

    for part in components.iter_mut() {
        part.width = across * part.across * 8;
        part.height = down * part.down * 8;
        let size = part
            .width
            .checked_mul(part.height)
            .ok_or(Trouble::TooLarge)?;
        if size > crate::MOST_PIXELS {
            return Err(Trouble::TooLarge);
        }
        part.samples = alloc::vec![0u8; size];
        part.previous_dc = 0;
    }

    let mut bits = Bits::new(data);
    let mut block = [0i32; BLOCK];
    let mut since_restart = 0usize;

    for row in 0..down {
        for column in 0..across {
            if restart_interval > 0 && since_restart == restart_interval {
                since_restart = 0;
                bits.restart();
                for part in components.iter_mut() {
                    part.previous_dc = 0;
                }
            }
            since_restart += 1;

            for index in 0..components.len() {
                let (across_blocks, down_blocks) =
                    (components[index].across, components[index].down);
                for vertical in 0..down_blocks {
                    for horizontal in 0..across_blocks {
                        decode_block(
                            &mut bits,
                            &mut components[index],
                            quantisers,
                            dc_tables,
                            ac_tables,
                            &mut block,
                        )?;
                        idct(&mut block);
                        place(
                            &mut components[index],
                            &block,
                            (column * across_blocks + horizontal) * 8,
                            (row * down_blocks + vertical) * 8,
                        );
                    }
                }
            }
        }
    }

    Ok(crate::Picture {
        width: width as u32,
        height: height as u32,
        pixels: to_pixels(components, width, height, count),
    })
}

/// One 8x8 block: Huffman, then dequantise into natural order.
fn decode_block(
    bits: &mut Bits<'_>,
    component: &mut Component,
    quantisers: &[[u16; BLOCK]; 4],
    dc_tables: &[Option<Huffman>; 4],
    ac_tables: &[Option<Huffman>; 4],
    block: &mut [i32; BLOCK],
) -> Result<(), Trouble> {
    block.fill(0);
    let quantiser = quantisers
        .get(component.quantiser)
        .ok_or(Trouble::Malformed)?;
    let dc = dc_tables[component.dc_table]
        .as_ref()
        .ok_or(Trouble::Malformed)?;
    let ac = ac_tables[component.ac_table]
        .as_ref()
        .ok_or(Trouble::Malformed)?;

    // The DC coefficient is a difference from the previous block's.
    let size = dc.decode(bits)?;
    if size > 15 {
        return Err(Trouble::Malformed);
    }
    let difference = extend(bits.take(size)?, size);
    component.previous_dc += difference;
    block[0] = component.previous_dc * i32::from(quantiser[0]);

    // Then runs of zeros and a value, until the end-of-block or sixty-four.
    let mut index = 1usize;
    while index < BLOCK {
        let symbol = ac.decode(bits)?;
        let run = usize::from(symbol >> 4);
        let size = symbol & 0x0F;
        if size == 0 {
            if run == 15 {
                // Sixteen zeros, and keep going.
                index += 16;
                continue;
            }
            break;
        }
        index += run;
        if index >= BLOCK {
            return Err(Trouble::Malformed);
        }
        let value = extend(bits.take(size)?, size);
        let natural = ZIGZAG[index];
        block[natural] = value * i32::from(quantiser[natural]);
        index += 1;
    }
    Ok(())
}

/// How many bits the cosine table is scaled by.
const SCALE: i32 = 11;

/// The one-dimensional inverse transform, as a table.
///
/// `COSINE[u][x]` is `c(u) * cos((2x+1) * u * pi / 16) / 2`, scaled by
/// `2^SCALE` and rounded, where `c(0)` is `1/sqrt(2)` and every other `c(u)` is
/// one. Those are the constants in the definition of the transform; having them
/// as a table rather than as a factored butterfly is slower and is obviously
/// the thing the specification says, which is what matters first. The first
/// version of this file was a factored one written from memory, and it decoded
/// every photograph to a flat grey.
const COSINE: [[i32; 8]; 8] = [
    [724, 724, 724, 724, 724, 724, 724, 724],
    [1004, 851, 569, 200, -200, -569, -851, -1004],
    [946, 392, -392, -946, -946, -392, 392, 946],
    [851, -200, -1004, -569, 569, 1004, 200, -851],
    [724, -724, -724, 724, 724, -724, -724, 724],
    [569, -1004, 200, 851, -851, -200, 1004, -569],
    [392, -946, 946, -392, -392, 946, -946, 392],
    [200, -569, 851, -1004, 1004, -851, 569, -200],
];

/// The inverse discrete cosine transform, in integers.
///
/// Separable: the same one-dimensional transform down the columns, then across
/// the rows. Integer throughout, because this system's kernel does not enable
/// the floating-point unit -- and because integer is what every practical
/// decoder uses anyway, since it gives the same answer on every machine.
///
/// The intermediate values fit: a dequantised coefficient reaches a few
/// thousand, times a table entry under 2^11, summed over eight terms, is about
/// 2^25 -- and the second pass works on values already shifted back down.
fn idct(block: &mut [i32; BLOCK]) {
    let half = 1 << (SCALE - 1);
    let mut middle = [0i32; BLOCK];

    // Down the columns.
    for column in 0..8 {
        for x in 0..8 {
            let mut total = 0i32;
            for u in 0..8 {
                total += block[u * 8 + column] * COSINE[u][x];
            }
            middle[x * 8 + column] = (total + half) >> SCALE;
        }
    }

    // And across the rows.
    for row in 0..8 {
        for x in 0..8 {
            let mut total = 0i32;
            for u in 0..8 {
                total += middle[row * 8 + u] * COSINE[u][x];
            }
            block[row * 8 + x] = (total + half) >> SCALE;
        }
    }
}

/// Put one decoded block into its component's plane.
fn place(component: &mut Component, block: &[i32; BLOCK], left: usize, top: usize) {
    for row in 0..8 {
        let y = top + row;
        if y >= component.height {
            break;
        }
        for column in 0..8 {
            let x = left + column;
            if x >= component.width {
                break;
            }
            // Samples come out centred on zero and are stored centred on 128.
            let value = block[row * 8 + column] + 128;
            component.samples[y * component.width + x] = value.clamp(0, 255) as u8;
        }
    }
}

/// Upsample the components and convert to pixels.
fn to_pixels(components: &[Component], width: usize, height: usize, count: usize) -> Vec<u32> {
    let mut pixels = alloc::vec![0u32; count];
    let widest = components.iter().map(|part| part.across).max().unwrap_or(1);
    let tallest = components.iter().map(|part| part.down).max().unwrap_or(1);

    // Nearest-neighbour upsampling: a chroma sample covers as many pixels as
    // its sampling factor is smaller than the largest. Smooth upsampling is
    // better and is a separate piece of work; at a glance, on a photograph, the
    // difference is a softer edge on strong colour boundaries.
    let sample = |part: &Component, x: usize, y: usize| -> i32 {
        let sx = x * part.across / widest;
        let sy = y * part.down / tallest;
        let sx = sx.min(part.width.saturating_sub(1));
        let sy = sy.min(part.height.saturating_sub(1));
        i32::from(part.samples[sy * part.width + sx])
    };

    for y in 0..height {
        for x in 0..width {
            let pixel = if components.len() == 1 {
                let grey = sample(&components[0], x, y).clamp(0, 255) as u32;
                grey << 16 | grey << 8 | grey
            } else {
                // YCbCr to RGB, in integers scaled by 2^16, which is the
                // conversion every decoder uses and is exact enough that no two
                // implementations disagree by more than one level.
                let luma = sample(&components[0], x, y);
                let blue_difference = sample(&components[1], x, y) - 128;
                let red_difference = sample(&components[2], x, y) - 128;
                let red = luma + (91881 * red_difference >> 16);
                let green = luma - ((22554 * blue_difference + 46802 * red_difference) >> 16);
                let blue = luma + (116130 * blue_difference >> 16);
                (red.clamp(0, 255) as u32) << 16
                    | (green.clamp(0, 255) as u32) << 8
                    | blue.clamp(0, 255) as u32
            };
            pixels[y * width + x] = pixel;
        }
    }
    pixels
}
